//! 同步引擎：把账本（`ledger`）、六个同步源（`sources`）和存储后端（`backends`）编排成「一轮同步」。
//!
//! 引擎本身**不是并发的**：它被 `manager.rs` 的 actor 独占（同一时刻只有一个任务在动它），所以
//! 里面用 `&mut self`，没有任何锁。一轮同步（[`SyncEngine::run_round`]）的步骤——
//!
//! 1. **捕获**：对每个同步源，当前文档与基线 `diff`，本机 op 先并入镜像、进待上传队列；
//! 2. **大量删除闸**：扣下的删除不进镜像、不上传，状态变 `needsConfirm`；
//! 3. **上传**：待上传队列写成一个增量段；
//! 4. **拉取**：读其它设备目录里比游标新的快照 / 增量段，并入镜像；
//! 5. **落地**：对镜像有新内容的源，在该配置的**写者锁内**先再捕获一次本机改动（防抖窗口里的修改
//!    不能被吞），再把文档对齐到镜像，提交；
//! 6. **压缩**：本机增量段攒到 50 个就写一个快照、清理旧文件。
//!
//! 「先捕获、再落地」这个顺序是正确性的根：落地用 `affected_vs_doc`（文档现状 vs 镜像）把文档对齐到
//! 镜像，没有被捕获进镜像的本机修改会被当成「文档偏离镜像」而被旧值盖回去。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use serde_json::Value;
use zeroize::Zeroizing;

use super::backends::{Backend, BackendFactory};
use super::core::crypto::{self, DataKey, FileKind, RecoveryKey};
use super::core::format::{
    self, decode_segment, decode_snapshot, decode_sync_code, encode_segment, encode_snapshot,
    encode_sync_code, file_path, is_valid_device_id, new_device_id, parse_file_name,
    SyncCodePayload, SyncMeta, DEVICES_DIR, META_FILE,
};
use super::core::path::{split_platform_prefix, PrefixKind};
use super::core::{
    affected_vs_doc, apply_to_doc, export_all, export_all_at, flatten_scoped, DeviceInfo, DiffCtx,
    FormatError, Hlc, Op, OpKind, Path, Platform, Seg,
};
use super::docedit;
use super::history::{change_records, source_label, ChangeCtx, HistoryRecord, HistoryStore};
use super::join::{reconcile_connections, remap_connection_refs, HostKeyBundle, SftpBundle};
use super::ledger::Ledger;
use super::secrets::SyncSecretStore;
use super::sources::{AdapterUpdate, GuardWarning, SourceAdapter};
use super::storage::{StorageConfig, StorageError};
use super::store::{Cursor, LedgerFiles};
use super::types::{SyncCreateResult, SyncDevice, SyncState, SyncStatus};
use crate::config::connections::ConnectionStore;
use crate::config::keychain::SecretSlot;
use crate::config::settings::AppSettings;
use crate::data::{DataSourceId, JsonStore};
use crate::error::AppError;
use crate::ssh::known_hosts::KnownHostsStore;

/// 一个增量段最多装多少条 op（超过的拆成多个段，免得单个文件大到传不动）。
const MAX_OPS_PER_SEGMENT: usize = 4000;
/// 本机增量段攒到这么多就压缩成快照。
pub const COMPACT_AFTER_SEGS: u32 = 50;

/// 前端事件的出口。
pub trait SyncEvents: Send + Sync {
    fn status(&self, status: &SyncStatus);
    fn data_applied(&self, source: &str);
}

/// 引擎的外部依赖。
pub struct SyncDeps {
    pub config_dir: PathBuf,
    /// 六个同步源（顺序固定）。
    pub adapters: Vec<Arc<dyn SourceAdapter>>,
    /// 加入同步（带 SFTP 服务器）/ 创建同步（加受保护路径）要直接写这两份配置。
    pub settings: JsonStore<AppSettings>,
    pub connections: JsonStore<ConnectionStore>,
    pub known_hosts: Arc<KnownHostsStore>,
    pub secrets: Arc<dyn SyncSecretStore>,
    pub backends: Arc<dyn BackendFactory>,
    pub events: Arc<dyn SyncEvents>,
    /// 当前时间（Unix 毫秒）。测试可注入，生产用 `wall_clock_ms`。
    pub now_ms: Arc<dyn Fn() -> u64 + Send + Sync>,
    pub app_version: String,
    pub platform: Platform,
    pub default_device_name: String,
    /// 启动加载时碰到损坏、被迫改用默认值的配置（需要从镜像恢复）。
    pub load_failures: Vec<DataSourceId>,
}

/// 命令层 / 界面读得到的共享视图（不用等引擎空闲）。
pub struct SharedView {
    pub status: RwLock<SyncStatus>,
    pub devices: RwLock<Vec<SyncDevice>>,
    pub history: Mutex<HistoryStore>,
    /// 当前生效的存储配置（生成同步码用）。
    pub storage: RwLock<Option<StorageConfig>>,
}

impl SharedView {
    pub fn new(history: HistoryStore) -> Self {
        Self {
            status: RwLock::new(SyncStatus::off()),
            devices: RwLock::new(Vec::new()),
            history: Mutex::new(history),
            storage: RwLock::new(None),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoundKind {
    /// 完整一轮：捕获 → 上传 → 拉取 → 落地 → 压缩。
    Full,
    /// 本机改动触发：只捕获并上传，不碰网络上别人的数据。
    Local,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ApplyMode {
    /// 常规落地：锁内先捕获本机改动。
    Normal,
    /// 配置损坏恢复：不捕获（捕获到的只是「全变成默认值」），直接用镜像整份恢复。
    Restore,
    /// 加入同步：本机数据以 `Hlc::zero_for` 并入镜像（云端赢），再整份落地。
    Join,
}

pub enum JoinSpec {
    Code(Zeroizing<String>),
    Manual {
        storage: StorageConfig,
        secret: Option<Zeroizing<String>>,
        recovery_key: Zeroizing<String>,
    },
}

#[derive(Default)]
struct PullReport {
    needs_upgrade: bool,
    soft_errors: Vec<String>,
}

enum PullError {
    /// 网络 / 认证 / 权限：整轮失败，退避重试。
    Hard(AppError),
    /// 这台设备有本版本读不懂的新格式数据。
    TooNew,
    /// 这台设备的某个文件坏了（解不开 / 结构不对）：记下来，不耽误其它设备。
    Soft(String),
}

/// 给人看的错误文案（不带 `Config error:` 之类的英文前缀）。
pub fn plain_error(e: &AppError) -> String {
    match e {
        AppError::Other(m)
        | AppError::Config(m)
        | AppError::Network(m)
        | AppError::Ssh(m)
        | AppError::Cancelled(m) => m.clone(),
        other => other.to_string(),
    }
}

fn storage_err(e: StorageError) -> AppError {
    AppError::from(e)
}

pub struct SyncEngine {
    deps: Arc<SyncDeps>,
    view: Arc<SharedView>,
    files: LedgerFiles,
    pub(crate) ledger: Ledger,
    backend: Option<(StorageConfig, Arc<dyn Backend>)>,
    dk: Option<DataKey>,
    needs_upgrade: bool,
    last_error: Option<String>,
    syncing: bool,
    /// 上一次发布给前端的状态（没变就不再发）。
    last_published: Option<SyncStatus>,
    /// 加入同步时连接去重产生的 id 改写表（本机 id → 云端 id）。
    join_remap: BTreeMap<String, String>,
}

impl SyncEngine {
    // ═══════════════════════════════════════════════════════════════════
    // 构造与基础设施
    // ═══════════════════════════════════════════════════════════════════

    pub fn new(deps: Arc<SyncDeps>, view: Arc<SharedView>) -> Self {
        let files = LedgerFiles::new(&deps.config_dir);
        let loaded = files.load();
        let mut ledger = Ledger::from_loaded(loaded);

        if ledger.enabled() {
            // 版本升级：每个源的第一次捕获不发 Remove
            if ledger.st.last_app_version != deps.app_version {
                for a in &deps.adapters {
                    ledger.suppress_pending.insert(a.key().to_string());
                }
            }
            // 配置损坏被迫用默认值：不要把「全变成默认值」当成用户的删除推出去，改从镜像恢复
            for failed in &deps.load_failures {
                if let Some(key) = failed.sync_key() {
                    ledger.set_needs_restore(key, true);
                }
            }
        }

        let engine = SyncEngine {
            deps,
            view,
            files,
            ledger,
            backend: None,
            dk: None,
            needs_upgrade: false,
            last_error: None,
            syncing: false,
            last_published: None,
            join_remap: BTreeMap::new(),
        };
        engine.refresh_view();
        engine
    }

    pub fn enabled(&self) -> bool {
        self.ledger.enabled()
    }

    fn now_ms(&self) -> u64 {
        (self.deps.now_ms)()
    }

    fn rfc3339(&self, ms: u64) -> String {
        rfc3339_from_ms(ms)
    }

    fn now_rfc3339(&self) -> String {
        self.rfc3339(self.now_ms())
    }

    fn device_info(&self) -> DeviceInfo {
        DeviceInfo {
            id: self.ledger.st.device_id.clone(),
            name: self.ledger.st.device_name.clone(),
            platform: self.deps.platform.as_str().to_string(),
            app_version: self.deps.app_version.clone(),
        }
    }

    pub fn status(&self) -> SyncStatus {
        let l = &self.ledger;
        if !l.enabled() {
            return SyncStatus::off();
        }
        let held = l.held_summary();
        let state = if self.syncing {
            SyncState::Syncing
        } else if !held.is_empty() {
            SyncState::NeedsConfirm
        } else if self.needs_upgrade {
            SyncState::NeedsUpgrade
        } else if self.last_error.is_some() {
            SyncState::Error
        } else {
            SyncState::Idle
        };
        SyncStatus {
            enabled: true,
            state,
            last_sync_at: l.st.last_sync_at.clone(),
            last_error: self.last_error.clone(),
            pending_count: l.outbox.len() + held.iter().map(|h| h.count).sum::<usize>(),
            device_id: Some(l.st.device_id.clone()),
            device_name: Some(l.st.device_name.clone()),
            storage: l.st.storage.clone(),
            held: if held.is_empty() { None } else { Some(held) },
        }
    }

    /// 把状态 / 设备 / 存储配置同步给共享视图并（有变化时）通知前端。
    fn publish(&mut self) {
        let status = self.status();
        self.refresh_view();
        if self.last_published.as_ref() != Some(&status) {
            self.deps.events.status(&status);
            self.last_published = Some(status);
        }
    }

    fn refresh_view(&self) {
        *self.view.status.write() = self.status();
        *self.view.storage.write() = self.ledger.st.storage.clone();
        *self.view.devices.write() = self.device_list();
    }

    fn device_list(&self) -> Vec<SyncDevice> {
        let l = &self.ledger;
        if !l.enabled() {
            return Vec::new();
        }
        let mut out = vec![SyncDevice {
            id: l.st.device_id.clone(),
            name: l.st.device_name.clone(),
            platform: self.deps.platform.as_str().to_string(),
            app_version: self.deps.app_version.clone(),
            last_seen_at: l
                .st
                .last_sync_at
                .clone()
                .unwrap_or_else(|| self.now_rfc3339()),
            is_self: true,
        }];
        for (id, rec) in &l.st.devices {
            if *id == l.st.device_id {
                continue;
            }
            out.push(SyncDevice {
                id: id.clone(),
                name: if rec.name.is_empty() {
                    "未命名设备".to_string()
                } else {
                    rec.name.clone()
                },
                platform: rec.platform.clone(),
                app_version: rec.app_version.clone(),
                last_seen_at: rec.last_seen_at.clone().unwrap_or_default(),
                is_self: false,
            });
        }
        out
    }

    fn flush(&mut self) -> Result<(), AppError> {
        self.ledger.flush(&self.files)
    }

    fn set_syncing(&mut self, on: bool) {
        if self.syncing != on {
            self.syncing = on;
            self.publish();
        }
    }

    fn ensure_backend(&mut self) -> Result<Arc<dyn Backend>, AppError> {
        let Some(storage) = self.ledger.st.storage.clone() else {
            return Err(AppError::Other("还没有配置同步存储位置".into()));
        };
        if let Some((cfg, backend)) = &self.backend {
            if *cfg == storage {
                return Ok(backend.clone());
            }
        }
        let secret = self.storage_secret()?;
        let backend = self
            .deps
            .backends
            .build(&storage, secret.as_deref().map(|s| s.as_str()))
            .map_err(storage_err)?;
        self.backend = Some((storage, backend.clone()));
        Ok(backend)
    }

    /// WebDAV 的应用密码（SFTP 存储没有）。
    fn storage_secret(&self) -> Result<Option<Zeroizing<String>>, AppError> {
        if !matches!(self.ledger.st.storage, Some(StorageConfig::WebDav { .. })) {
            return Ok(None);
        }
        Ok(self
            .deps
            .secrets
            .get(&SecretSlot::SyncStoragePassword)?
            .map(Zeroizing::new))
    }

    fn data_key(&mut self) -> Result<DataKey, AppError> {
        if let Some(dk) = &self.dk {
            return Ok(dk.clone());
        }
        let Some(b64) = self.deps.secrets.get(&SecretSlot::SyncDataKey)?.map(Zeroizing::new)
        else {
            return Err(AppError::Other(
                "找不到同步的加密密钥（可能被系统清理了）。请先停止同步，再用恢复密钥重新加入。".into(),
            ));
        };
        let dk = DataKey::from_b64(&b64)?;
        self.dk = Some(dk.clone());
        Ok(dk)
    }

    // ═══════════════════════════════════════════════════════════════════
    // 一轮同步
    // ═══════════════════════════════════════════════════════════════════

    pub async fn run_round(&mut self, kind: RoundKind) -> Result<(), AppError> {
        if !self.ledger.enabled() {
            return Ok(());
        }
        let result = self.round_inner(kind).await;
        self.syncing = false;
        match &result {
            Ok(()) => {
                self.last_error = None;
                if kind == RoundKind::Full {
                    self.ledger.st.last_sync_at = Some(self.now_rfc3339());
                    self.ledger.dirty.state = true;
                }
            }
            Err(e) => self.last_error = Some(plain_error(e)),
        }
        let flushed = self.flush();
        self.publish();
        match (result, flushed) {
            (Err(e), _) => Err(e),
            (Ok(()), Err(e)) => Err(e),
            (Ok(()), Ok(())) => Ok(()),
        }
    }

    async fn round_inner(&mut self, kind: RoundKind) -> Result<(), AppError> {
        // 1. 捕获本机改动
        self.capture_all().await?;
        self.flush()?;
        self.publish();

        if kind == RoundKind::Local && self.ledger.outbox.is_empty() {
            return Ok(()); // 本机改动被扣下了 / 根本没有：不碰网络
        }

        let backend = self.ensure_backend()?;
        let dk = self.data_key()?;
        self.set_syncing(true);

        // 2. 上传
        self.upload_outbox(&backend, &dk).await?;
        if kind == RoundKind::Local {
            return Ok(());
        }

        // 3. 拉取
        let report = self.pull(&backend, &dk).await?;
        self.needs_upgrade = report.needs_upgrade;

        // 4. 落地
        let apply_errors = self.apply_pending().await;

        // 5. 落地时锁内的再次捕获可能产生了新 op
        self.upload_outbox(&backend, &dk).await?;

        // 6. 压缩（失败不影响这一轮的结论）
        if let Err(e) = self.maybe_compact(&backend, &dk).await {
            log::warn!("同步：压缩本机增量段失败（下一轮再试）: {}", plain_error(&e));
        }

        let mut errors = report.soft_errors;
        errors.extend(apply_errors);
        if errors.is_empty() {
            Ok(())
        } else {
            Err(AppError::Other(errors.join("；")))
        }
    }

    /// 对每个源捕获一次本机改动。
    async fn capture_all(&mut self) -> Result<(), AppError> {
        let wall = self.now_ms();
        let platform = self.deps.platform;
        let adapters = self.deps.adapters.clone();
        for adapter in &adapters {
            let key = adapter.key();
            if self.ledger.is_restoring(key) {
                continue; // 等恢复落地，不能把「全变成默认值」推出去
            }
            let doc = adapter.read_json().await?;
            self.ledger
                .capture_into(key, adapter.rules(), platform, wall, &doc)?;
        }
        // 每个源都至少捕获过一次（恢复中的源会被镜像整份覆盖，不需要再抑制）：升级抑制到此为止
        self.ledger.suppress_pending.clear();
        if self.ledger.st.last_app_version != self.deps.app_version {
            self.ledger.st.last_app_version = self.deps.app_version.clone();
            self.ledger.dirty.state = true;
        }
        Ok(())
    }

    async fn upload_outbox(
        &mut self,
        backend: &Arc<dyn Backend>,
        dk: &DataKey,
    ) -> Result<(), AppError> {
        while !self.ledger.outbox.is_empty() {
            let n = self.ledger.outbox.len().min(MAX_OPS_PER_SEGMENT);
            let batch: Vec<Op> = self.ledger.outbox[..n].to_vec();
            // 序号先预留并落盘：put 成功但随后记账失败时，重传用的是新序号，
            // 读的一方不会因为「这个序号我读过了」漏掉重传里多出来的 op。序号允许有空洞。
            self.ledger.st.seq += 1;
            let seq = self.ledger.st.seq;
            self.ledger.dirty.state = true;
            self.flush()?;
            let bytes = encode_segment(dk, &self.device_info(), seq, &batch)?;
            let path = file_path(&self.ledger.st.device_id, FileKind::Segment, seq);
            backend.put(&path, &bytes).await.map_err(storage_err)?;
            self.ledger.outbox.drain(..n);
            self.ledger.dirty.outbox = true;
            self.ledger.st.segs_since_snapshot += 1;
            self.ledger.dirty.state = true;
            self.flush()?;
        }
        Ok(())
    }

    // ── 拉取 ─────────────────────────────────────────────────────────────

    async fn pull(
        &mut self,
        backend: &Arc<dyn Backend>,
        dk: &DataKey,
    ) -> Result<PullReport, AppError> {
        let mut report = PullReport::default();
        let entries = backend.list(DEVICES_DIR).await.map_err(storage_err)?;
        let mut devices: Vec<String> = entries
            .into_iter()
            .filter(|e| {
                e.is_dir && is_valid_device_id(&e.name) && e.name != self.ledger.st.device_id
            })
            .map(|e| e.name)
            .collect();
        devices.sort();
        for device in devices {
            match self.pull_device(backend, dk, &device).await {
                Ok(()) => {}
                Err(PullError::Hard(e)) => {
                    // 已读到的部分先落盘（镜像先于游标），下一轮接着来
                    let _ = self.flush();
                    return Err(e);
                }
                Err(PullError::TooNew) => report.needs_upgrade = true,
                Err(PullError::Soft(msg)) => report.soft_errors.push(msg),
            }
        }
        self.flush()?;
        Ok(report)
    }

    fn classify(&self, device: &str, e: FormatError) -> PullError {
        if e.is_too_new() {
            return PullError::TooNew;
        }
        let name = self
            .ledger
            .st
            .devices
            .get(device)
            .map(|d| d.name.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "另一台设备".to_string());
        PullError::Soft(format!("读取「{name}」的同步数据失败：{e}"))
    }

    async fn pull_device(
        &mut self,
        backend: &Arc<dyn Backend>,
        dk: &DataKey,
        device: &str,
    ) -> Result<(), PullError> {
        let files = backend
            .list(&format::device_dir(device))
            .await
            .map_err(|e| PullError::Hard(storage_err(e)))?;
        let mut snaps: Vec<u64> = Vec::new();
        let mut segs: Vec<u64> = Vec::new();
        for f in files.iter().filter(|f| !f.is_dir) {
            match parse_file_name(&f.name) {
                Some((FileKind::Snapshot, seq)) => snaps.push(seq),
                Some((FileKind::Segment, seq)) => segs.push(seq),
                None => {}
            }
        }
        snaps.sort_unstable();
        segs.sort_unstable();

        let mut cursor = self.ledger.cursor(device);
        let wall = self.now_ms();

        // 有比游标新的快照：先读最新那个（整份镜像合并），再读其后的增量段
        if let Some(&latest) = snaps.last() {
            if latest > cursor.seg {
                let path = file_path(device, FileKind::Snapshot, latest);
                let Some(bytes) = backend
                    .get(&path)
                    .await
                    .map_err(|e| PullError::Hard(storage_err(e)))?
                else {
                    return Ok(()); // 列出来又读不到（对方正在压缩）：下一轮重新列
                };
                let snap = decode_snapshot(dk, device, latest, &bytes)
                    .map_err(|e| self.classify(device, e))?;
                self.ledger
                    .ingest_snapshot(&snap, wall, rfc3339_from_ms);
                cursor = Cursor {
                    snap: latest,
                    seg: latest,
                };
                self.ledger.set_cursor(device, cursor.clone());
            }
        }

        for seq in segs {
            if seq <= cursor.seg {
                continue;
            }
            let path = file_path(device, FileKind::Segment, seq);
            let Some(bytes) = backend
                .get(&path)
                .await
                .map_err(|e| PullError::Hard(storage_err(e)))?
            else {
                return Ok(());
            };
            let seg = decode_segment(dk, device, seq, &bytes)
                .map_err(|e| self.classify(device, e))?;
            self.ledger.ingest_segment(&seg, wall, rfc3339_from_ms);
            cursor.seg = seq;
            self.ledger.set_cursor(device, cursor.clone());
        }
        Ok(())
    }

    // ── 落地 ─────────────────────────────────────────────────────────────

    /// 对所有需要落地的源落地。返回各源的错误文案（落地失败不影响别的源）。
    async fn apply_pending(&mut self) -> Vec<String> {
        let mut errors = Vec::new();
        let adapters = self.deps.adapters.clone();
        for adapter in adapters {
            let key = adapter.key();
            if !self.ledger.needs_apply(key) {
                continue;
            }
            let mode = if self.ledger.is_restoring(key) {
                ApplyMode::Restore
            } else {
                ApplyMode::Normal
            };
            if let Err(e) = self.merge_into_doc(&adapter, mode).await {
                log::warn!("同步：{} 落地失败（下一轮重试）: {}", source_label(key), plain_error(&e));
                errors.push(format!("{}：{}", source_label(key), plain_error(&e)));
            }
        }
        errors
    }

    /// 在一个源的写者锁内：（常规模式）先捕获本机改动 → 把文档对齐到镜像 → 提交 → 基线 = 提交后的序列化。
    async fn merge_into_doc(
        &mut self,
        adapter: &Arc<dyn SourceAdapter>,
        mode: ApplyMode,
    ) -> Result<bool, AppError> {
        let key = adapter.key();
        let rules = adapter.rules();
        let platform = self.deps.platform;
        let wall = self.now_ms();
        let zero = Hlc::zero_for(self.ledger.device_id().to_string());
        let remap = if mode == ApplyMode::Join {
            self.join_remap.clone()
        } else {
            BTreeMap::new()
        };

        #[derive(Default)]
        struct Scratch {
            dedupe: Vec<(String, String)>,
            warnings: Vec<GuardWarning>,
            /// 加入同步时本机引用改写的落点（sync 路径 → 改写后的值）。落地之后要再确认一次。
            ref_fixes: Vec<(Path, Value)>,
        }
        let mut scratch = Scratch::default();

        let update: AdapterUpdate = {
            let ledger = &mut self.ledger;
            let scratch = &mut scratch;
            let adapter_ref = adapter.clone();
            adapter
                .update(Box::new(move |current: &Value| {
                    let mut doc = current.clone();
                    match mode {
                        ApplyMode::Normal => {
                            // 防抖窗口里的本机改动：落地之前先捕获，否则会被镜像里的旧值盖回去
                            ledger.capture_into(key, rules, platform, wall, current)?;
                        }
                        ApplyMode::Restore => {}
                        ApplyMode::Join => {
                            if key == "connections" {
                                let visible = ledger
                                    .mirrors
                                    .get(key)
                                    .map(|m| m.visible())
                                    .unwrap_or_default();
                                scratch.dedupe = reconcile_connections(&mut doc, &visible);
                            }
                            if !remap.is_empty() {
                                remap_connection_refs(key, &mut doc, &remap);
                                // 记下改写后的值：下面 ZERO 导出的本机数据在对上云端的同名字段时会输给
                                // 云端（云端赢），落地后要把这些引用改回来，否则留下指向已删连接的悬空引用
                                scratch.ref_fixes = ref_targets(key, &doc, rules, platform, ledger);
                            }
                            // 本机已有数据以 ZERO 并入镜像：云端在任何冲突上都赢，本机独有的保留，
                            // 并且要上传（别的设备也该有这些）
                            let idmaps = ledger.st.idmaps.entry(key.to_string()).or_default();
                            let ops = export_all_at(key, &doc, rules, platform, idmaps, &zero);
                            if !ops.is_empty() {
                                let mirror = ledger.mirrors.entry(key.to_string()).or_default();
                                for op in &ops {
                                    mirror.merge(op);
                                }
                                ledger.dirty.mirrors.insert(key.to_string());
                                ledger.outbox.extend(ops);
                                ledger.dirty.outbox = true;
                            }
                        }
                    }

                    let mirror = ledger.mirrors.get(key).cloned().unwrap_or_default();
                    let mut affected = {
                        let idmaps = ledger.st.idmaps.entry(key.to_string()).or_default();
                        affected_vs_doc(&doc, &mirror, rules, platform, idmaps)
                    };
                    ledger.dirty.state = true;

                    // 扣下的大量删除：这些元素在镜像里还活着，但本机已经删了，别把它们写回来
                    let held = ledger.held_paths(key);
                    if !held.is_empty() {
                        affected.retain(|p| !held.iter().any(|h| p.starts_with(h)));
                    }
                    // 来源特有的安全过滤（known_hosts 的指纹冲突）
                    scratch.warnings =
                        adapter_ref.guard_affected(&doc, &mirror.visible(), &mut affected);

                    let mut new_doc = if affected.is_empty() {
                        doc.clone()
                    } else {
                        apply_to_doc(&doc, &mirror, &affected, rules, platform)
                    };
                    // 加入同步：让改写过的引用在落地之后仍然成立。上面按 ZERO 导出的本机数据遇到
                    // 「云端也有这个字段」会输给云端（云端赢），一条指向已被去重掉的连接的引用就被
                    // 云端的旧值盖回去了（典型：云端的多机清单是空的）。悬空引用是错的，所以这里把
                    // 它按当前时钟写回来——这是本机的修正，不是要让云端旧值赢。
                    if mode == ApplyMode::Join && !scratch.ref_fixes.is_empty() {
                        let mut ops = Vec::new();
                        for (path, value) in &scratch.ref_fixes {
                            let dpath = match split_platform_prefix(path, platform) {
                                (PrefixKind::Other, _) => continue,
                                (_, rest) => rest.to_vec(),
                            };
                            if docedit::get_leaf(&new_doc, &dpath) == Some(value) {
                                continue;
                            }
                            docedit::set_leaf(&mut new_doc, &dpath, value.clone());
                            let hlc = ledger.clock.now(wall);
                            ops.push(Op {
                                source: key.to_string(),
                                path: path.clone(),
                                kind: OpKind::Set(value.clone()),
                                hlc,
                            });
                        }
                        if !ops.is_empty() {
                            let mirror = ledger.mirrors.entry(key.to_string()).or_default();
                            for op in &ops {
                                mirror.merge(op);
                            }
                            ledger.dirty.mirrors.insert(key.to_string());
                            ledger.outbox.extend(ops);
                            ledger.dirty.outbox = true;
                        }
                    }
                    if new_doc == *current {
                        Ok(None)
                    } else {
                        Ok(Some(new_doc))
                    }
                }))
                .await?
        };

        let AdapterUpdate {
            committed,
            json,
            before,
        } = update;

        // 提交成功（或确认无需改动）才推进基线与落地标记
        self.ledger.set_baseline(key, json.clone());
        self.ledger.set_applied(key, true);
        self.ledger.set_needs_restore(key, false);
        self.ledger.dirty.mirrors.insert(key.to_string());

        let mut records: Vec<HistoryRecord> = Vec::new();
        let at = self.now_rfc3339();
        if committed {
            let idmaps = self.ledger.st.idmaps.get(key).cloned().unwrap_or_default();
            let mirror = self.ledger.mirrors.get(key).cloned().unwrap_or_default();
            let ctx = ChangeCtx {
                source: key,
                rules,
                platform,
                idmaps: &idmaps,
                mirror: &mirror,
                devices: self.ledger.device_records(),
                self_device_id: self.ledger.device_id(),
                at: &at,
            };
            let changes = change_records(&ctx, &before, &json);
            if mode == ApplyMode::Join {
                if !changes.is_empty() {
                    records.push(HistoryRecord::summary(
                        key,
                        &at,
                        "云端",
                        &format!(
                            "加入同步：把云端的 {} 处{}合并到了本机",
                            changes.len(),
                            source_label(key)
                        ),
                    ));
                }
            } else {
                records.extend(changes);
            }
        }
        for w in &scratch.warnings {
            records.push(HistoryRecord::warning(
                key,
                &at,
                "云端",
                &w.path,
                &w.text,
                &w.dedupe_key,
            ));
        }
        if !records.is_empty() {
            self.view.history.lock().push_many(records);
        }

        // 加入同步的连接去重：被丢掉的本机连接的密码 / 口令搬到云端那条名下。
        // **必须在 `after_apply` 之前**：`ConnectionsPostApply` 会把从文档里消失的连接
        // 在密钥链里的凭据一并删掉（同 `config_delete_connection`），搬晚了就什么都搬不着了。
        if !scratch.dedupe.is_empty() {
            for (local_id, cloud_id) in &scratch.dedupe {
                self.migrate_connection_secrets(local_id, cloud_id);
                self.join_remap.insert(local_id.clone(), cloud_id.clone());
            }
        }

        if committed {
            adapter.after_apply(&before, &json).await;
            self.deps.events.data_applied(key);
        }
        Ok(committed)
    }

    /// 把本机连接 `from` 的密码 / 私钥口令 / 跳板机凭据搬到 `to` 名下（`to` 已经有的不覆盖），再清掉 `from`。
    fn migrate_connection_secrets(&self, from: &str, to: &str) {
        let secrets = &self.deps.secrets;
        for (old, new) in SecretSlot::for_connection(from)
            .into_iter()
            .zip(SecretSlot::for_connection(to))
        {
            let moved = (|| -> Result<(), AppError> {
                if let Some(value) = secrets.get(&old)?.map(Zeroizing::new) {
                    if secrets.get(&new)?.is_none() {
                        secrets.set(&new, &value)?;
                    }
                    secrets.delete(&old)?;
                }
                Ok(())
            })();
            if let Err(e) = moved {
                log::warn!("同步：迁移重复连接的{}失败: {}", old.describe(), e);
            }
        }
    }

    // ── 压缩 ─────────────────────────────────────────────────────────────

    async fn maybe_compact(
        &mut self,
        backend: &Arc<dyn Backend>,
        dk: &DataKey,
    ) -> Result<(), AppError> {
        if self.ledger.st.segs_since_snapshot < COMPACT_AFTER_SEGS || !self.ledger.outbox.is_empty()
        {
            return Ok(());
        }
        let seq = self.ledger.st.seq;
        let device = self.ledger.st.device_id.clone();
        let (mirrors, idmaps) = self.ledger.snapshot_parts();
        let bytes = encode_snapshot(dk, &self.device_info(), seq, &mirrors, &idmaps)?;
        backend
            .put(&file_path(&device, FileKind::Snapshot, seq), &bytes)
            .await
            .map_err(storage_err)?;
        self.ledger.st.segs_since_snapshot = 0;
        self.ledger.dirty.state = true;
        self.flush()?;

        // 清理：本机 ≤ seq 的增量段，以及更早的快照（只留最新两个）。别人的文件永远不动。
        let own = backend
            .list(&format::device_dir(&device))
            .await
            .map_err(storage_err)?;
        let mut snaps: Vec<u64> = Vec::new();
        let mut stale: Vec<String> = Vec::new();
        for f in own.iter().filter(|f| !f.is_dir) {
            match parse_file_name(&f.name) {
                Some((FileKind::Segment, s)) if s <= seq => {
                    stale.push(file_path(&device, FileKind::Segment, s))
                }
                Some((FileKind::Snapshot, s)) => snaps.push(s),
                _ => {}
            }
        }
        snaps.sort_unstable();
        if snaps.len() > 2 {
            for s in &snaps[..snaps.len() - 2] {
                stale.push(file_path(&device, FileKind::Snapshot, *s));
            }
        }
        for path in stale {
            backend.delete(&path).await.map_err(storage_err)?;
        }
        Ok(())
    }

    /// 测试专用：跳过「捕获」，直接拉取并落地——模拟「防抖窗口里本机刚改了东西、还没被捕获，
    /// 远端落地就来了」。落地步骤自己会在写者锁内先捕获一次，这里验证它没有把本机改动吞掉。
    #[cfg(test)]
    pub(crate) async fn test_pull_and_apply_without_capture(&mut self) -> Vec<String> {
        let backend = self.ensure_backend().expect("backend");
        let dk = self.data_key().expect("dk");
        self.pull(&backend, &dk).await.expect("pull");
        let errors = self.apply_pending().await;
        self.flush().expect("flush");
        self.publish();
        errors
    }

    // ═══════════════════════════════════════════════════════════════════
    // 开启 / 加入 / 停止
    // ═══════════════════════════════════════════════════════════════════

    /// 连通性 + 权限自检。`secret` 为空且当前已开启同一份 WebDAV 存储时用已保存的密码。
    pub async fn test_storage(
        &self,
        storage: &StorageConfig,
        secret: Option<Zeroizing<String>>,
    ) -> Result<(), AppError> {
        let secret = match secret.filter(|s| !s.is_empty()) {
            Some(s) => Some(s),
            None if self.ledger.st.storage.as_ref() == Some(storage) => self.storage_secret()?,
            None => None,
        };
        let backend = self
            .deps
            .backends
            .build(storage, secret.as_deref().map(|s| s.as_str()))
            .map_err(storage_err)?;
        let result = backend.probe().await.map_err(storage_err);
        backend.close().await;
        result
    }

    /// 开启同步（第一台设备）。
    pub async fn create(
        &mut self,
        storage: StorageConfig,
        secret: Option<Zeroizing<String>>,
    ) -> Result<SyncCreateResult, AppError> {
        if self.ledger.enabled() {
            return Err(AppError::Other("同步已经开启了。".into()));
        }
        let secret = secret.filter(|s| !s.is_empty());
        let backend = self
            .deps
            .backends
            .build(&storage, secret.as_deref().map(|s| s.as_str()))
            .map_err(storage_err)?;
        backend.probe().await.map_err(storage_err)?;
        if backend.get(META_FILE).await.map_err(storage_err)?.is_some() {
            backend.close().await;
            return Err(AppError::Other(
                "该位置已有同步数据，请用「加入已有同步」。".into(),
            ));
        }

        let (rk, dk, wrapped) = crypto::generate_vault()?;
        let device_id = new_device_id();

        // SFTP 存储：把同步目录加进受保护路径，免得 Agent 把它删了
        let mut protected = None;
        if let Some(root) = backend.protected_root().await {
            self.add_protected_path(&root).await;
            protected = Some(root);
        }

        // 全新账本：每个源全量导出为 Set op（当前时钟）
        let previous = std::mem::replace(&mut self.ledger, Ledger::fresh());
        self.ledger.set_identity(&device_id);
        self.ledger.st.device_name = self.deps.default_device_name.clone();
        self.ledger.st.storage = Some(storage.clone());
        self.ledger.st.last_app_version = self.deps.app_version.clone();
        self.ledger.st.protected_path = protected;
        let built = self.create_inner(&backend, &dk, &wrapped, secret.as_deref().map(|s| s.as_str()), &rk).await;
        match built {
            Ok(result) => {
                backend.close().await;
                self.backend = None;
                self.dk = Some(dk);
                self.publish();
                Ok(result)
            }
            Err(e) => {
                // 回滚：远端只可能留下本机的设备目录与 meta（尽力清理），本机账本放回去
                let _ = backend
                    .delete(&format::device_dir(&device_id))
                    .await;
                let _ = backend.delete(META_FILE).await;
                backend.close().await;
                self.ledger = previous;
                let _ = self.files.wipe();
                for slot in SecretSlot::sync_slots() {
                    let _ = self.deps.secrets.delete(&slot);
                }
                self.publish();
                Err(e)
            }
        }
    }

    async fn create_inner(
        &mut self,
        backend: &Arc<dyn Backend>,
        dk: &DataKey,
        wrapped: &crypto::WrappedKey,
        storage_secret: Option<&str>,
        rk: &RecoveryKey,
    ) -> Result<SyncCreateResult, AppError> {
        let wall = self.now_ms();
        let platform = self.deps.platform;
        for adapter in self.deps.adapters.clone() {
            let key = adapter.key();
            let doc = adapter.read_json().await?;
            let ledger = &mut self.ledger;
            let idmaps = ledger.st.idmaps.entry(key.to_string()).or_default();
            let ops = {
                let mut ctx = DiffCtx {
                    rules: adapter.rules(),
                    platform,
                    idmaps,
                    clock: &mut ledger.clock,
                    wall_ms: wall,
                    suppress_removes: false,
                };
                export_all(key, &doc, &mut ctx)
            };
            let mirror = ledger.mirrors.entry(key.to_string()).or_default();
            for op in &ops {
                mirror.merge(op);
            }
            ledger.set_baseline(key, doc);
            ledger.set_applied(key, true);
        }

        // 快照先于 meta：meta 是「这份同步已经存在」的标记，放最后写
        let _ = self.files.wipe();
        self.view.history.lock().clear();
        self.ledger.st.seq = 1;
        let (mirrors, idmaps) = self.ledger.snapshot_parts();
        let snap = encode_snapshot(dk, &self.device_info(), 1, &mirrors, &idmaps)?;
        let device = self.ledger.st.device_id.clone();
        backend
            .put(&file_path(&device, FileKind::Snapshot, 1), &snap)
            .await
            .map_err(storage_err)?;
        let meta = SyncMeta::new(wrapped.clone(), self.now_rfc3339());
        backend
            .put(META_FILE, &meta.to_bytes())
            .await
            .map_err(storage_err)?;

        // 本机存密钥、落账本
        self.deps
            .secrets
            .set(&SecretSlot::SyncDataKey, &dk.to_b64())?;
        self.deps
            .secrets
            .set(&SecretSlot::SyncRecoveryKey, rk.canonical())?;
        if let (Some(StorageConfig::WebDav { .. }), Some(pw)) =
            (&self.ledger.st.storage, storage_secret)
        {
            self.deps.secrets.set(&SecretSlot::SyncStoragePassword, pw)?;
        }
        self.ledger.st.enabled = true;
        self.ledger.mark_all_dirty();
        self.flush()?;

        let storage = self.ledger.st.storage.clone().expect("刚设置过");
        let code = self
            .build_sync_code(&storage, storage_secret, rk.canonical())
            .await?;
        Ok(SyncCreateResult {
            recovery_key: rk.display().to_string(),
            sync_code: code.to_string(),
        })
    }

    /// 把同步目录（SFTP 服务器上的绝对路径）加进受保护路径。失败只记日志，不拦开启。
    async fn add_protected_path(&self, root: &str) {
        let root = root.to_string();
        let r = self
            .deps
            .settings
            .mutate(move |s| {
                if !s.custom_protected_paths.iter().any(|p| p == &root) {
                    s.custom_protected_paths.push(root);
                }
                Ok(())
            })
            .await;
        if let Err(e) = r {
            log::warn!("同步：把同步目录加入受保护路径失败: {e}");
        }
    }

    /// 生成同步码（加新设备用）。**本身就是机密**：含恢复密钥与存储密码。
    pub async fn build_sync_code(
        &self,
        storage: &StorageConfig,
        storage_secret: Option<&str>,
        rk_canonical: &str,
    ) -> Result<Zeroizing<String>, AppError> {
        build_sync_code(&self.deps, storage, storage_secret, rk_canonical).await
    }

    /// 加入已有同步。
    pub async fn join(&mut self, spec: JoinSpec) -> Result<(), AppError> {
        if self.ledger.enabled() {
            return Err(AppError::Other("同步已经开启了。".into()));
        }

        // ── 解析参数；同步码里的 SFTP 服务器先装进本机（失败要回滚） ──
        let mut installed = InstalledBundle::default();
        let (storage, secret, rk) = match spec {
            JoinSpec::Manual {
                storage,
                secret,
                recovery_key,
            } => (
                storage,
                secret.filter(|s| !s.is_empty()),
                RecoveryKey::parse(&recovery_key)?,
            ),
            JoinSpec::Code(code) => {
                let payload: SyncCodePayload<StorageConfig> = decode_sync_code(&code)?;
                let rk = RecoveryKey::parse(&payload.rk)?;
                let secret = Some(Zeroizing::new(payload.secret.clone())).filter(|s| !s.is_empty());
                let storage = payload.storage.clone();
                if matches!(storage, StorageConfig::Sftp { .. }) {
                    let bundle = payload
                        .sftp_connection
                        .clone()
                        .and_then(|v| serde_json::from_value::<SftpBundle>(v).ok())
                        .ok_or_else(|| {
                            AppError::Other("同步码里缺少 SFTP 服务器的信息，请在另一台设备上重新生成同步码。".into())
                        })?;
                    check_bundle_supported(&bundle)?;
                    installed = self.install_bundle(&bundle, secret.as_deref().map(|s| s.as_str())).await?;
                }
                (storage, secret, rk)
            }
        };

        let result = self.join_inner(storage, secret, rk).await;
        if result.is_err() {
            self.rollback_bundle(installed).await;
        }
        result
    }

    async fn join_inner(
        &mut self,
        storage: StorageConfig,
        secret: Option<Zeroizing<String>>,
        rk: RecoveryKey,
    ) -> Result<(), AppError> {
        let backend = self
            .deps
            .backends
            .build(&storage, secret.as_deref().map(|s| s.as_str()))
            .map_err(storage_err)?;

        // ── 读 meta、解出数据密钥 ──
        let Some(meta_bytes) = backend.get(META_FILE).await.map_err(storage_err)? else {
            backend.close().await;
            return Err(AppError::Other(
                "在这个位置没有找到同步数据。请确认存储位置（地址 / 目录）和另一台设备上的一致。".into(),
            ));
        };
        let meta = SyncMeta::from_bytes(&meta_bytes)?;
        let dk = meta.unwrap_data_key(&rk)?;
        // 到这里才确定要加入了：清掉上一次残留的账本与记录，之后落地产生的同步记录才不会被误删
        let _ = self.files.wipe();
        self.view.history.lock().clear();

        // ── 换新身份，拉取全部设备的数据 ──
        let previous = std::mem::replace(&mut self.ledger, Ledger::fresh());
        self.join_remap.clear();
        self.ledger.set_identity(&new_device_id());
        self.ledger.st.device_name = self.deps.default_device_name.clone();
        self.ledger.st.storage = Some(storage.clone());
        self.ledger.st.last_app_version = self.deps.app_version.clone();

        let outcome = self.join_pull_and_merge(&backend, &dk).await;
        let outcome = match outcome {
            Ok(()) => self.finish_join(&storage, secret.as_deref().map(|s| s.as_str()), &dk, &rk),
            Err(e) => Err(e),
        };
        match outcome {
            Ok(()) => {
                backend.close().await;
                self.backend = None;
                self.dk = Some(dk);
                self.last_error = None;
                self.needs_upgrade = false;
                self.publish();
                Ok(())
            }
            Err(e) => {
                backend.close().await;
                self.ledger = previous;
                self.join_remap.clear();
                let _ = self.files.wipe();
                for slot in SecretSlot::sync_slots() {
                    let _ = self.deps.secrets.delete(&slot);
                }
                self.publish();
                Err(e)
            }
        }
    }

    async fn join_pull_and_merge(
        &mut self,
        backend: &Arc<dyn Backend>,
        dk: &DataKey,
    ) -> Result<(), AppError> {
        let report = self.pull(backend, dk).await?;
        if report.needs_upgrade {
            return Err(AppError::Other(
                "云端的数据来自更新版本的应用，请先把本机升级到最新版本再加入。".into(),
            ));
        }
        if let Some(first) = report.soft_errors.first() {
            return Err(AppError::Other(first.clone()));
        }

        // 连接先于其它源：去重产生的 id 改写表要在处理设置 / 快捷指令之前就位
        let adapters = self.deps.adapters.clone();
        let mut ordered: Vec<&Arc<dyn SourceAdapter>> = Vec::with_capacity(adapters.len());
        ordered.extend(adapters.iter().filter(|a| a.key() == "connections"));
        ordered.extend(adapters.iter().filter(|a| a.key() != "connections"));
        for adapter in ordered {
            self.merge_into_doc(adapter, ApplyMode::Join).await?;
        }
        Ok(())
    }

    fn finish_join(
        &mut self,
        storage: &StorageConfig,
        storage_secret: Option<&str>,
        dk: &DataKey,
        rk: &RecoveryKey,
    ) -> Result<(), AppError> {
        self.deps
            .secrets
            .set(&SecretSlot::SyncDataKey, &dk.to_b64())?;
        self.deps
            .secrets
            .set(&SecretSlot::SyncRecoveryKey, rk.canonical())?;
        if let (StorageConfig::WebDav { .. }, Some(pw)) = (storage, storage_secret) {
            self.deps.secrets.set(&SecretSlot::SyncStoragePassword, pw)?;
        }
        self.ledger.st.enabled = true;
        // 所有源都已经在加入时对齐过镜像
        for adapter in &self.deps.adapters {
            self.ledger.set_applied(adapter.key(), true);
        }
        self.ledger.mark_all_dirty();
        self.flush()
    }

    async fn install_bundle(
        &self,
        bundle: &SftpBundle,
        password: Option<&str>,
    ) -> Result<InstalledBundle, AppError> {
        let mut installed = InstalledBundle::default();
        let conn = bundle.connection.clone();
        let id = conn.id.clone();

        // 连接：本机已有同 id 的就沿用（用户可能改过）
        let existed = self.deps.connections.read().await.get_by_id(&id).is_some();
        if !existed {
            let to_add = conn.clone();
            self.deps
                .connections
                .mutate(move |store| {
                    store.add(to_add);
                    Ok(())
                })
                .await?;
            installed.connection_added = Some(id.clone());
        }
        if let Some(pw) = password.filter(|p| !p.is_empty()) {
            let slot = SecretSlot::SshPassword(id.clone());
            if self.deps.secrets.get(&slot)?.is_none() {
                self.deps.secrets.set(&slot, pw)?;
                installed.password_set = Some(id.clone());
            }
        }
        // 主机指纹：同步码里带的等于用户在旧设备上确认过了
        if let Some(hk) = &bundle.host_key {
            match self
                .deps
                .known_hosts
                .record(&hk.host, hk.port, hk.entry.clone())
                .await
            {
                Ok(()) => installed.host_key = Some((hk.host.clone(), hk.port)),
                Err(_) => {
                    // 本机已有这台主机、指纹不同：不覆盖（安全边界），让用户自己去确认
                    self.rollback_bundle(installed).await;
                    return Err(AppError::Other(format!(
                        "本机已经记录了 {}:{} 的另一个主机指纹，和同步码里的不一致。请先在终端里连接这台服务器并确认指纹，再重新加入。",
                        hk.host, hk.port
                    )));
                }
            }
        }
        Ok(installed)
    }

    async fn rollback_bundle(&self, installed: InstalledBundle) {
        if let Some(id) = installed.connection_added {
            let _ = self
                .deps
                .connections
                .mutate(move |store| {
                    store.remove(&id);
                    Ok(())
                })
                .await;
        }
        if let Some(id) = installed.password_set {
            let _ = self.deps.secrets.delete(&SecretSlot::SshPassword(id));
        }
        if let Some((host, port)) = installed.host_key {
            let _ = self.deps.known_hosts.forget(&host, port).await;
        }
    }

    /// 停止同步：本机配置数据不动。`wipe_remote` 同时删掉云端本应用的同步文件。
    pub async fn disable(&mut self, wipe_remote: bool) -> Result<(), AppError> {
        if self.ledger.enabled() && wipe_remote {
            let backend = self.ensure_backend()?;
            backend.delete(DEVICES_DIR).await.map_err(storage_err)?;
            backend.delete(META_FILE).await.map_err(storage_err)?;
        }
        if let Some((_, backend)) = self.backend.take() {
            backend.close().await;
        }
        for slot in SecretSlot::sync_slots() {
            if let Err(e) = self.deps.secrets.delete(&slot) {
                log::warn!("同步：清理{}失败: {}", slot.describe(), e);
            }
        }
        self.files.wipe()?;
        self.ledger = Ledger::fresh();
        self.dk = None;
        self.needs_upgrade = false;
        self.last_error = None;
        self.syncing = false;
        self.join_remap.clear();
        self.view.history.lock().clear();
        self.publish();
        Ok(())
    }

    // ═══════════════════════════════════════════════════════════════════
    // 其它命令
    // ═══════════════════════════════════════════════════════════════════

    pub fn set_device_name(&mut self, name: &str) -> Result<(), AppError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(AppError::Other("设备名称不能为空。".into()));
        }
        if name.chars().count() > 64 {
            return Err(AppError::Other("设备名称太长了（最多 64 个字）。".into()));
        }
        if !self.ledger.enabled() {
            return Err(AppError::Other("还没有开启同步。".into()));
        }
        self.ledger.st.device_name = name.to_string();
        self.ledger.dirty.state = true;
        self.flush()?;
        self.publish();
        Ok(())
    }

    /// 处理「大量删除」的确认：`accept` 放行并同步给其它设备，否则撤销本机删除。
    pub async fn confirm_held(&mut self, accept: bool) -> Result<(), AppError> {
        if !self.ledger.enabled() {
            return Err(AppError::Other("还没有开启同步。".into()));
        }
        if self.ledger.held.is_empty() {
            return Ok(());
        }
        if accept {
            self.ledger.release_held();
        } else {
            self.ledger.discard_held();
        }
        self.flush()?;
        self.publish();
        // 放行 → 上传；撤销 → 把被删的元素从镜像写回本机。两种都跑一轮完整同步。
        self.run_round(RoundKind::Full).await
    }
}

/// 加入同步时记下「本机文档里指向连接 id 的引用」当前的值（sync 路径 → 值）。
///
/// 只取受 `remap_connection_refs` 影响的那几处（快捷指令的 `sessionKey`、设置里的多机清单），
/// 用同一个扁平化把路径算出来，落地之后照这份清单把值写回去。
fn ref_targets(
    key: &str,
    doc: &Value,
    rules: &super::core::ScopeRules,
    platform: Platform,
    ledger: &Ledger,
) -> Vec<(Path, Value)> {
    let mut reg = ledger.st.idmaps.get(key).cloned().unwrap_or_default();
    let flat = flatten_scoped(doc, rules, platform, &mut reg);
    flat.into_iter()
        .filter(|(path, _)| match key {
            "quickCommands" => {
                matches!(path.as_slice(), [Seg::Key(list), Seg::Id { .. }, Seg::Key(f)]
                    if list == "commands" && f == "sessionKey")
            }
            "settings" => path
                .last()
                .is_some_and(|s| matches!(s, Seg::Key(k) if k == "multiHostConnectionIds")),
            _ => false,
        })
        .collect()
}

#[derive(Default)]
struct InstalledBundle {
    connection_added: Option<String>,
    password_set: Option<String>,
    host_key: Option<(String, u16)>,
}

/// 同步码带不过去的服务器：用私钥登录、或者要经过跳板机。
fn check_bundle_supported(bundle: &SftpBundle) -> Result<(), AppError> {
    let c = &bundle.connection;
    if c.auth_method != "Password" {
        return Err(AppError::Other(
            "这台服务器用私钥登录，同步码带不过去私钥。请先在本机添加这台服务器并导入私钥，再用「手动加入」输入恢复密钥。".into(),
        ));
    }
    if c.use_jump {
        return Err(AppError::Other(
            "这台服务器要经过跳板机登录，同步码带不过去跳板机的凭据。请先在本机添加这台服务器，再用「手动加入」输入恢复密钥。".into(),
        ));
    }
    Ok(())
}

/// 生成同步码。
pub async fn build_sync_code(
    deps: &SyncDeps,
    storage: &StorageConfig,
    storage_secret: Option<&str>,
    rk_canonical: &str,
) -> Result<Zeroizing<String>, AppError> {
    let mut secret = Zeroizing::new(storage_secret.unwrap_or("").to_string());
    let mut sftp_connection = None;
    if let StorageConfig::Sftp { connection_id, .. } = storage {
        let connection = deps
            .connections
            .read()
            .await
            .get_by_id(connection_id)
            .cloned()
            .ok_or_else(|| AppError::Other("同步用的服务器连接已经被删除了。".into()))?;
        if connection.auth_method == "Password" {
            if let Some(pw) = deps.secrets.get(&SecretSlot::SshPassword(connection_id.clone()))? {
                secret = Zeroizing::new(pw);
            }
        }
        let host_key = deps
            .known_hosts
            .list()
            .await
            .into_iter()
            .find(|(k, _)| *k == format!("{}:{}", connection.host.to_lowercase(), connection.port))
            .map(|(_, entry)| HostKeyBundle {
                host: connection.host.clone(),
                port: connection.port,
                entry,
            });
        let bundle = SftpBundle {
            connection,
            host_key,
        };
        sftp_connection = Some(
            serde_json::to_value(&bundle)
                .map_err(|e| AppError::Other(format!("生成同步码失败: {e}")))?,
        );
    }
    let payload = SyncCodePayload {
        storage: storage.clone(),
        secret: std::mem::take(&mut *secret),
        rk: rk_canonical.to_string(),
        sftp_connection,
    };
    Ok(encode_sync_code(&payload)?)
}

pub fn rfc3339_from_ms(ms: u64) -> String {
    chrono::DateTime::from_timestamp_millis(ms as i64)
        .unwrap_or_default()
        .to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::connections::SavedConnection;

    fn conn(auth: &str, use_jump: bool) -> SavedConnection {
        SavedConnection {
            id: "c1".into(),
            name: "服务器".into(),
            host: "example.com".into(),
            port: 22,
            username: "root".into(),
            auth_method: auth.into(),
            key_path: None,
            key_id: None,
            group: None,
            last_connected: None,
            use_jump,
            jump_host: None,
            jump_port: None,
            jump_username: None,
            jump_auth_method: None,
            jump_key_path: None,
            jump_key_id: None,
            remote_platform: None,
        }
    }

    fn bundle(auth: &str, use_jump: bool) -> SftpBundle {
        SftpBundle {
            connection: conn(auth, use_jump),
            host_key: None,
        }
    }

    #[test]
    fn sync_codes_cannot_carry_private_keys_or_jump_hosts() {
        assert!(check_bundle_supported(&bundle("Password", false)).is_ok());

        let key = check_bundle_supported(&bundle("PrivateKey", false)).unwrap_err();
        let text = plain_error(&key);
        assert!(text.contains("私钥"), "{text}");
        assert!(
            text.contains("手动加入"),
            "要告诉用户下一步怎么办（手动加入 + 恢复密钥）: {text}"
        );

        let jump = check_bundle_supported(&bundle("Password", true)).unwrap_err();
        assert!(plain_error(&jump).contains("跳板机"));
    }
}
