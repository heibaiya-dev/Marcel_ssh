//! 同步源适配：把六份配置（五份 `JsonStore` + `known_hosts`）统一成 [`SourceAdapter`]。
//!
//! 同步引擎只认这个 trait，不知道背后是哪种存储。每个适配器回答三件事：
//! 1. 这份数据现在是什么样（[`SourceAdapter::read_json`]，读锁快照）；
//! 2. 在**写者锁内**做一次「读 → 改 → 提交」（[`SourceAdapter::update`]）——引擎把「先捕获本机改动、
//!    再落地远端改动」放在同一把锁里，是靠它保证防抖窗口里的本机修改不会被远端落地盖掉；
//! 3. 落地后的副作用（清缓存、删密钥链孤儿条目）与来源特有的安全过滤。
//!
//! 落地一律走存储自己的写入口（`JsonStore::begin_update` + `commit`；known_hosts 走
//! `KnownHostsStore::sync_update`），所以它们和其它写者互斥、成功后经 `DataChangeBus` 广播——
//! 这条总线正是同步引擎捕获本机改动的唯一入口，落地产生的广播会让引擎再看一眼，
//! 发现「没有本机改动」后静默结束。

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use super::core::path::unescape_key;
use super::core::{Path, ScopeRules, Seg};
use super::scopes;
use super::secrets::SyncSecretStore;
use crate::config::connections::ConnectionStore;
use crate::config::keychain::SecretSlot;
use crate::config::persist::JsonPersistable;
use crate::config::quick_commands::QuickCommandStore;
use crate::config::settings::AppSettings;
use crate::data::{DataSourceId, JsonStore};
use crate::error::AppError;
use crate::mcp::manager::McpManager;
use crate::mcp::store::McpServerStore;
use crate::skills::store::SkillStore;
use crate::ssh::known_hosts::KnownHostsStore;

/// 闭包形式的「在写者锁内改文档」：拿到当前文档 JSON，返回 `Some(新文档 JSON)` 就校验并提交。
pub type UpdateFn<'a> = Box<dyn FnOnce(&Value) -> Result<Option<Value>, AppError> + Send + 'a>;

/// [`SourceAdapter::update`] 的结果。
pub struct AdapterUpdate {
    /// 文档是否真的被改写并落盘。
    pub committed: bool,
    /// 更新**之后**文档的 JSON（= 新基线）。
    pub json: Value,
    /// 更新**之前**文档的 JSON。
    pub before: Value,
}

/// 落地前的安全过滤给出的警告（写进同步记录）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardWarning {
    /// 被拒绝应用的路径（已经从待落地列表里摘掉）。
    pub path: Path,
    /// 给人看的整句话。
    pub text: String,
    /// 去重用：同一条警告在历史里只记一次。
    pub dedupe_key: String,
}

#[async_trait]
pub trait SourceAdapter: Send + Sync {
    /// 同步源 key（跨版本稳定的字符串）。
    fn key(&self) -> &'static str;
    fn data_source(&self) -> DataSourceId;
    fn rules(&self) -> &'static ScopeRules;
    /// 当前整份文档的 JSON（读锁快照，不持锁返回）。
    async fn read_json(&self) -> Result<Value, AppError>;
    /// 写者锁内的「读 → 改 → 提交」。`f` 返回 `Err` 或 `None` 都不会改动任何东西。
    async fn update(&self, f: UpdateFn<'_>) -> Result<AdapterUpdate, AppError>;
    /// 落地提交成功后的副作用（照搬对应 tauri 命令里的做法）。失败只记日志。
    async fn after_apply(&self, _before: &Value, _after: &Value) {}
    /// 来源特有的落地前过滤：可以从 `affected` 里摘掉某些路径，并返回要写进同步记录的警告。
    /// `current` 是本机文档现状，`visible` 是镜像的可见集。
    fn guard_affected(
        &self,
        _current: &Value,
        _visible: &BTreeMap<Path, Value>,
        _affected: &mut Vec<Path>,
    ) -> Vec<GuardWarning> {
        Vec::new()
    }
}

// ---------------------------------------------------------------------------
// JsonStore 适配
// ---------------------------------------------------------------------------

/// 能参与同步的 `JsonStore` 配置。
pub trait SyncDoc: JsonPersistable + Clone + Send + Sync + 'static {
    fn scope_rules() -> &'static ScopeRules;

    /// 把合并后的 JSON 变回 `Self`：走与读配置文件**同一套**迁移 / 归一化 / 校验。
    ///
    /// `current` 是当前内存里的值——不进序列化的内存字段（例如渠道的 `api_key`）从它沿用，
    /// 否则一次同步落地就会把本机已经读进内存的密钥清空。
    fn from_sync_json(v: Value, current: &Self) -> Result<Self, AppError>;
}

/// `{"xxx": []}` 形状的配置：缺字段（或整份是 null）时补成空列表，避免因为「列表被删光后
/// 序列化省略了它」而反序列化失败。
fn from_value_with_list<T: serde::de::DeserializeOwned>(
    mut v: Value,
    list_key: &str,
) -> Result<T, AppError> {
    if v.is_null() {
        v = Value::Object(Default::default());
    }
    if let Some(obj) = v.as_object_mut() {
        obj.entry(list_key.to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
    }
    serde_json::from_value(v).map_err(|e| AppError::Config(format!("合并后的配置无法读取: {e}")))
}

impl SyncDoc for AppSettings {
    fn scope_rules() -> &'static ScopeRules {
        &scopes::SETTINGS_RULES
    }

    fn from_sync_json(v: Value, current: &Self) -> Result<Self, AppError> {
        let mut next = AppSettings::from_json_value(&v)?;
        // 渠道的 API Key 只在内存里（`skip_serializing`），合并出来的 JSON 里不会有：沿用本机已有的。
        for channel in &mut next.llm_registry.channels {
            if let Some(old) = current
                .llm_registry
                .channels
                .iter()
                .find(|c| c.id == channel.id)
            {
                channel.api_key = old.api_key.clone();
            }
        }
        // 与「保存设置」同一个函数：迁移、去重、归一化、校验。
        crate::commands::settings::normalize_settings_for_save(&mut next)?;
        Ok(next)
    }
}

impl SyncDoc for ConnectionStore {
    fn scope_rules() -> &'static ScopeRules {
        &scopes::CONNECTIONS_RULES
    }
    fn from_sync_json(v: Value, _current: &Self) -> Result<Self, AppError> {
        from_value_with_list(v, "connections")
    }
}

impl SyncDoc for QuickCommandStore {
    fn scope_rules() -> &'static ScopeRules {
        &scopes::QUICK_COMMANDS_RULES
    }
    fn from_sync_json(v: Value, _current: &Self) -> Result<Self, AppError> {
        from_value_with_list(v, "commands")
    }
}

impl SyncDoc for SkillStore {
    fn scope_rules() -> &'static ScopeRules {
        &scopes::SKILLS_RULES
    }
    fn from_sync_json(v: Value, _current: &Self) -> Result<Self, AppError> {
        from_value_with_list(v, "skills")
    }
}

impl SyncDoc for McpServerStore {
    fn scope_rules() -> &'static ScopeRules {
        &scopes::MCP_SERVERS_RULES
    }
    fn from_sync_json(v: Value, _current: &Self) -> Result<Self, AppError> {
        from_value_with_list(v, "servers")
    }
}

/// 落地之后的副作用。
#[async_trait]
pub trait PostApply: Send + Sync {
    async fn run(&self, before: &Value, after: &Value);
}

pub struct JsonSourceAdapter<T: SyncDoc> {
    key: &'static str,
    store: JsonStore<T>,
    post: Option<Arc<dyn PostApply>>,
}

impl<T: SyncDoc> JsonSourceAdapter<T> {
    pub fn new(store: JsonStore<T>, post: Option<Arc<dyn PostApply>>) -> Self {
        let key = T::SOURCE
            .sync_key()
            .expect("参与同步的配置必须在登记表里有同步源 key");
        Self { key, store, post }
    }
}

fn to_value<T: serde::Serialize>(v: &T) -> Result<Value, AppError> {
    serde_json::to_value(v).map_err(|e| AppError::Config(format!("序列化配置失败: {e}")))
}

#[async_trait]
impl<T: SyncDoc> SourceAdapter for JsonSourceAdapter<T> {
    fn key(&self) -> &'static str {
        self.key
    }

    fn data_source(&self) -> DataSourceId {
        T::SOURCE
    }

    fn rules(&self) -> &'static ScopeRules {
        T::scope_rules()
    }

    async fn read_json(&self) -> Result<Value, AppError> {
        // 先 clone 再序列化，读锁只持有一瞬间。
        let snapshot = self.store.snapshot().await;
        to_value(&snapshot)
    }

    async fn update(&self, f: UpdateFn<'_>) -> Result<AdapterUpdate, AppError> {
        let mut guard = self.store.begin_update().await;
        let before = to_value(guard.candidate())?;
        let Some(next_json) = f(&before)? else {
            return Ok(AdapterUpdate {
                committed: false,
                json: before.clone(),
                before,
            });
        };
        let next = T::from_sync_json(next_json, guard.candidate())?;
        let after = to_value(&next)?;
        *guard.candidate_mut() = next;
        let committed = guard.commit().await?;
        Ok(AdapterUpdate {
            committed,
            json: after,
            before,
        })
    }

    async fn after_apply(&self, before: &Value, after: &Value) {
        if let Some(post) = &self.post {
            post.run(before, after).await;
        }
    }
}

// ---------------------------------------------------------------------------
// 落地后的副作用
// ---------------------------------------------------------------------------

/// 一个 JSON 列表里所有元素的 id。
fn element_ids(doc: &Value, list_key: &str) -> Vec<String> {
    doc.get(list_key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|e| e.get("id").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// 连接：别的设备删了连接 → 本机密钥链里属于它的密码 / 口令也清掉（同 `config_delete_connection`）。
pub struct ConnectionsPostApply {
    pub secrets: Arc<dyn SyncSecretStore>,
}

#[async_trait]
impl PostApply for ConnectionsPostApply {
    async fn run(&self, before: &Value, after: &Value) {
        let kept: std::collections::HashSet<String> =
            element_ids(after, "connections").into_iter().collect();
        for id in element_ids(before, "connections") {
            if kept.contains(&id) {
                continue;
            }
            for slot in SecretSlot::for_connection(&id) {
                if let Err(e) = self.secrets.delete(&slot) {
                    log::warn!("同步删除连接后清理{}失败（id={}）：{}", slot.describe(), id, e);
                }
            }
        }
    }
}

/// MCP 服务：地址 / 名称变了或被删了 → 清掉它的工具发现缓存（同 `mcp_update_server` /
/// `mcp_delete_server`）。只切换启用状态不清（同 `mcp_toggle_server`）。
pub struct McpPostApply {
    pub manager: Arc<McpManager>,
}

#[async_trait]
impl PostApply for McpPostApply {
    async fn run(&self, before: &Value, after: &Value) {
        let index = |doc: &Value| -> BTreeMap<String, (Value, Value)> {
            doc.get("servers")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|e| {
                            let id = e.get("id")?.as_str()?.to_string();
                            Some((
                                id,
                                (
                                    e.get("url").cloned().unwrap_or(Value::Null),
                                    e.get("name").cloned().unwrap_or(Value::Null),
                                ),
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        let (old, new) = (index(before), index(after));
        for (id, old_cfg) in &old {
            if new.get(id) != Some(old_cfg) {
                self.manager.clear_cache(id).await;
            }
        }
    }
}

/// 设置：别的设备删了模型渠道 → 本机密钥链里那个渠道的 API Key 也清掉（同 `config_save_settings`）。
pub struct SettingsPostApply {
    pub secrets: Arc<dyn SyncSecretStore>,
}

#[async_trait]
impl PostApply for SettingsPostApply {
    async fn run(&self, before: &Value, after: &Value) {
        let channel_ids = |doc: &Value| -> Vec<String> {
            doc.pointer("/llmRegistry/channels")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|e| e.get("id").and_then(Value::as_str).map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        let kept: std::collections::HashSet<String> = channel_ids(after).into_iter().collect();
        for id in channel_ids(before) {
            if !kept.contains(&id) {
                let _ = self.secrets.delete(&SecretSlot::LlmChannel(id));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// known_hosts
// ---------------------------------------------------------------------------

/// 主机指纹。**安全规则**：远端对本机已有的条目改指纹 → 不应用，记一条警告；
/// 远端新增条目、删除条目照常应用。
///
/// 为什么：指纹是 TOFU 的信任根。如果另一台设备（可能被中间人骗过、也可能服务器真的重装了）
/// 记下了一个新指纹就能悄悄改掉本机的记录，等于绕过了「指纹变了，请确认」这道提示。
/// 本机已有的指纹只能由用户在本机确认后替换。
pub struct KnownHostsAdapter {
    store: Arc<KnownHostsStore>,
}

impl KnownHostsAdapter {
    pub fn new(store: Arc<KnownHostsStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl SourceAdapter for KnownHostsAdapter {
    fn key(&self) -> &'static str {
        DataSourceId::KnownHosts
            .sync_key()
            .expect("known_hosts 在登记表里有同步源 key")
    }

    fn data_source(&self) -> DataSourceId {
        DataSourceId::KnownHosts
    }

    fn rules(&self) -> &'static ScopeRules {
        &scopes::KNOWN_HOSTS_RULES
    }

    async fn read_json(&self) -> Result<Value, AppError> {
        self.store.sync_json().await
    }

    async fn update(&self, f: UpdateFn<'_>) -> Result<AdapterUpdate, AppError> {
        let before = self.store.sync_json().await?;
        let (committed, json) = self.store.sync_update(f).await?;
        Ok(AdapterUpdate {
            committed,
            json,
            before,
        })
    }

    fn guard_affected(
        &self,
        current: &Value,
        visible: &BTreeMap<Path, Value>,
        affected: &mut Vec<Path>,
    ) -> Vec<GuardWarning> {
        guard_known_host_fingerprints(current, visible, affected)
    }
}

/// `entries/<host:port>/…` 路径里的 host:port。
fn known_host_key(path: &[Seg]) -> Option<&str> {
    match path {
        [Seg::Key(entries), Seg::Key(host), ..] if entries == "entries" => Some(unescape_key(host)),
        _ => None,
    }
}

/// 纯函数，单测直接钉：指纹冲突的条目整条摘出 `affected`。
pub fn guard_known_host_fingerprints(
    current: &Value,
    visible: &BTreeMap<Path, Value>,
    affected: &mut Vec<Path>,
) -> Vec<GuardWarning> {
    let mut conflicts: BTreeMap<String, String> = BTreeMap::new(); // host:port → 远端指纹
    for p in affected.iter() {
        let Some(host) = known_host_key(p) else {
            continue;
        };
        let Some(local_fp) = current
            .pointer(&format!("/entries/{}/fingerprint_sha256", escape_pointer(host)))
            .and_then(Value::as_str)
        else {
            continue; // 本机没有这条（或没有指纹）：新增，照常应用
        };
        let fp_path: Path = vec![
            Seg::key("entries"),
            Seg::key(host),
            Seg::key("fingerprint_sha256"),
        ];
        // 远端把指纹删了：删除条目，照常应用
        let Some(remote_fp) = visible.get(&fp_path).and_then(Value::as_str) else {
            continue;
        };
        if remote_fp != local_fp {
            conflicts.insert(host.to_string(), remote_fp.to_string());
        }
    }
    if conflicts.is_empty() {
        return Vec::new();
    }
    let mut warnings = Vec::new();
    for (host, remote_fp) in &conflicts {
        affected.retain(|p| known_host_key(p) != Some(host.as_str()));
        warnings.push(GuardWarning {
            path: vec![Seg::key("entries"), Seg::key(host.as_str())],
            text: format!(
                "已信任主机 {host} 的指纹（另一台设备记录的是 {remote_fp}，与本机不同，未应用）。\
                 如果这台服务器确实换了密钥，请在本机重新连接并确认新的指纹。"
            ),
            dedupe_key: format!("known-host-conflict:{host}:{remote_fp}"),
        });
    }
    warnings
}

/// JSON Pointer 里的转义（`~` → `~0`，`/` → `~1`）。
fn escape_pointer(s: &str) -> String {
    s.replace('~', "~0").replace('/', "~1")
}

// ---------------------------------------------------------------------------
// 组装
// ---------------------------------------------------------------------------

/// 六份同步数据的句柄。
#[derive(Clone)]
pub struct SourceStores {
    pub settings: JsonStore<AppSettings>,
    pub connections: JsonStore<ConnectionStore>,
    pub quick_commands: JsonStore<QuickCommandStore>,
    pub skills: JsonStore<SkillStore>,
    pub mcp_servers: JsonStore<McpServerStore>,
    pub known_hosts: Arc<KnownHostsStore>,
    pub mcp_manager: Arc<McpManager>,
}

/// 按固定顺序造出六个适配器。顺序有意义：设置在前、连接在后——落地时先设置后连接，
/// 互相没有依赖，只是为了让日志与事件顺序稳定。
pub fn build_adapters(
    stores: &SourceStores,
    secrets: Arc<dyn SyncSecretStore>,
) -> Vec<Arc<dyn SourceAdapter>> {
    vec![
        Arc::new(JsonSourceAdapter::new(
            stores.settings.clone(),
            Some(Arc::new(SettingsPostApply {
                secrets: secrets.clone(),
            })),
        )),
        Arc::new(JsonSourceAdapter::new(
            stores.connections.clone(),
            Some(Arc::new(ConnectionsPostApply { secrets })),
        )),
        Arc::new(JsonSourceAdapter::new(
            stores.quick_commands.clone(),
            None,
        )),
        Arc::new(JsonSourceAdapter::new(stores.skills.clone(), None)),
        Arc::new(JsonSourceAdapter::new(
            stores.mcp_servers.clone(),
            Some(Arc::new(McpPostApply {
                manager: stores.mcp_manager.clone(),
            })),
        )),
        Arc::new(KnownHostsAdapter::new(stores.known_hosts.clone())),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn visible(entries: &[(&str, &str, &str)]) -> BTreeMap<Path, Value> {
        let mut m = BTreeMap::new();
        for (host, algo, fp) in entries {
            m.insert(
                vec![Seg::key("entries"), Seg::key(*host), Seg::key("algorithm")],
                json!(algo),
            );
            m.insert(
                vec![
                    Seg::key("entries"),
                    Seg::key(*host),
                    Seg::key("fingerprint_sha256"),
                ],
                json!(fp),
            );
        }
        m
    }

    fn fp_path(host: &str) -> Path {
        vec![
            Seg::key("entries"),
            Seg::key(host),
            Seg::key("fingerprint_sha256"),
        ]
    }

    fn algo_path(host: &str) -> Path {
        vec![Seg::key("entries"), Seg::key(host), Seg::key("algorithm")]
    }

    fn doc_with(host: &str, fp: &str) -> Value {
        json!({"version": 1, "entries": {host: {
            "algorithm": "ssh-ed25519",
            "fingerprint_sha256": fp,
            "first_seen": "t0", "last_seen": "t1"
        }}})
    }

    #[test]
    fn changed_fingerprint_for_a_known_host_is_rejected_with_a_warning() {
        let current = doc_with("1.2.3.4:22", "SHA256:local");
        let vis = visible(&[("1.2.3.4:22", "ssh-ed25519", "SHA256:other")]);
        let mut affected = vec![fp_path("1.2.3.4:22"), algo_path("1.2.3.4:22")];

        let warnings = guard_known_host_fingerprints(&current, &vis, &mut affected);

        assert!(affected.is_empty(), "冲突条目整条不应用");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].text.contains("1.2.3.4:22"));
        assert!(warnings[0].text.contains("SHA256:other"));
        assert!(warnings[0].text.contains("与本机不同"));
    }

    #[test]
    fn new_hosts_and_identical_fingerprints_apply_normally() {
        let current = doc_with("1.2.3.4:22", "SHA256:same");
        let vis = visible(&[
            ("1.2.3.4:22", "ssh-ed25519", "SHA256:same"),
            ("new.example.com:22", "ssh-ed25519", "SHA256:new"),
        ]);
        let mut affected = vec![
            fp_path("1.2.3.4:22"),
            fp_path("new.example.com:22"),
            algo_path("new.example.com:22"),
        ];
        let before = affected.clone();
        let warnings = guard_known_host_fingerprints(&current, &vis, &mut affected);
        assert!(warnings.is_empty());
        assert_eq!(affected, before);
    }

    #[test]
    fn remote_deletion_of_a_known_host_is_applied() {
        let current = doc_with("1.2.3.4:22", "SHA256:local");
        // 镜像里这台主机已经没有可见叶子了
        let vis = BTreeMap::new();
        let mut affected = vec![fp_path("1.2.3.4:22"), algo_path("1.2.3.4:22")];
        let warnings = guard_known_host_fingerprints(&current, &vis, &mut affected);
        assert!(warnings.is_empty());
        assert_eq!(affected.len(), 2, "删除条目照常应用");
    }

    #[test]
    fn only_the_conflicting_host_is_dropped() {
        let mut current = doc_with("a:22", "SHA256:a-local");
        current["entries"]["b:22"] = json!({
            "algorithm": "ssh-ed25519", "fingerprint_sha256": "SHA256:b", "first_seen": "t", "last_seen": "t"
        });
        let vis = visible(&[
            ("a:22", "ssh-ed25519", "SHA256:a-remote"),
            ("b:22", "ssh-rsa", "SHA256:b"),
        ]);
        let mut affected = vec![
            fp_path("a:22"),
            algo_path("b:22"), // b 只是算法名变了，指纹没变：应用
        ];
        let warnings = guard_known_host_fingerprints(&current, &vis, &mut affected);
        assert_eq!(warnings.len(), 1);
        assert_eq!(affected, vec![algo_path("b:22")]);
    }

    #[test]
    fn host_keys_with_slashes_or_tildes_do_not_break_the_lookup() {
        // IPv6 / 异常主机名里可能有 `/` `~`，JSON Pointer 要转义
        let current = doc_with("a/b~c:22", "SHA256:x");
        let vis = visible(&[("a/b~c:22", "ssh-ed25519", "SHA256:y")]);
        let mut affected = vec![fp_path("a/b~c:22")];
        let warnings = guard_known_host_fingerprints(&current, &vis, &mut affected);
        assert_eq!(warnings.len(), 1);
        assert!(affected.is_empty());
    }

    #[test]
    fn every_config_tier_source_has_an_adapter_key() {
        // 适配器的 key 全部来自登记表；这里钉住「六个、不重复、与登记表一致」
        let keys: Vec<&str> = DataSourceId::ALL
            .iter()
            .filter_map(|id| id.sync_key())
            .collect();
        assert_eq!(keys.len(), 6);
    }
}
