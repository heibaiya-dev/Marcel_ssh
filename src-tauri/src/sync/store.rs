//! 同步引擎的本机账本：`<配置目录>/sync/` 下的所有文件（登记表里的 `DataSourceId::SyncState`）。
//!
//! ```text
//! sync/state.json                 设备身份、开关、存储配置、游标、登记表、各源的落地标记……
//! sync/mirror/<source>.json       每个同步源的镜像（所有设备操作的并集）
//! sync/baseline/<source>.json     每个同步源的基线（本机代码上次自己序列化的结果）
//! sync/outbox.json                已捕获、还没上传的 op（重启不丢）
//! sync/held.json                  等待用户确认的大量删除（只含 Tombstone）
//! sync/history.json               同步记录（最近 500 条）
//! ```
//!
//! 全部经 `atomic_write`（tmp + fsync + rename）。**只改变了的才写**：引擎用脏标记驱动，
//! 空闲轮次一个字节都不写。同步源名来自远端文件（不可信输入），拼进文件名前必须过
//! [`valid_source_key`]。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path as FsPath, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::core::{Hlc, IdMapRegistry, Mirror, Op};
use super::storage::StorageConfig;
use crate::config::persist::atomic_write;
use crate::data::DataSourceId;
use crate::error::AppError;

/// 本机账本格式版本（`state.json` 的 `format`）。
pub const STATE_FORMAT: u32 = 1;

/// 同步源 key 能不能安全拼进文件名：1–64 个 `[A-Za-z0-9_-]`。
pub fn valid_source_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 64
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// 对某台其它设备「读到哪了」。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    /// 已吸收的最新快照序号。
    #[serde(default)]
    pub snap: u64,
    /// 已吸收到的最大序号（快照与增量段共用一套编号，吸收了快照就至少到它的序号）。
    #[serde(default)]
    pub seg: u64,
}

/// 对一台设备的认知（从它写的文件头里来）。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DeviceRecord {
    pub name: String,
    pub platform: String,
    pub app_version: String,
    /// 该设备最近一次被我们读到的改动的时间（RFC3339）。
    pub last_seen_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PersistedState {
    pub format: u32,
    pub device_id: String,
    pub device_name: String,
    pub enabled: bool,
    pub storage: Option<StorageConfig>,
    /// 本机已用掉的最大序号（增量段与快照共用）。
    pub seq: u64,
    /// 时钟的最后状态（重启后恢复，免得墙钟被回拨的设备发出比旧 op 还小的时间戳）。
    pub hlc: Option<Hlc>,
    pub cursors: BTreeMap<String, Cursor>,
    pub last_app_version: String,
    /// 同步源 key → id 映射登记表。
    pub idmaps: BTreeMap<String, IdMapRegistry>,
    /// 同步源 key → 镜像是否已落地到本机文档。缺项 = 看镜像是否非空。
    pub applied_mirror: BTreeMap<String, bool>,
    /// 同步源 key → 本机配置损坏过、要从镜像整份恢复（期间不捕获）。
    pub needs_restore: BTreeMap<String, bool>,
    pub devices: BTreeMap<String, DeviceRecord>,
    pub last_sync_at: Option<String>,
    /// 自上次快照以来本机上传了多少个增量段（到 50 就压缩）。
    pub segs_since_snapshot: u32,
    /// 我们为同步目录加进「受保护路径」的绝对路径（SFTP 存储）。
    pub protected_path: Option<String>,
}

impl Default for PersistedState {
    fn default() -> Self {
        Self {
            format: STATE_FORMAT,
            device_id: String::new(),
            device_name: String::new(),
            enabled: false,
            storage: None,
            seq: 0,
            hlc: None,
            cursors: BTreeMap::new(),
            last_app_version: String::new(),
            idmaps: BTreeMap::new(),
            applied_mirror: BTreeMap::new(),
            needs_restore: BTreeMap::new(),
            devices: BTreeMap::new(),
            last_sync_at: None,
            segs_since_snapshot: 0,
            protected_path: None,
        }
    }
}

/// 磁盘上整套账本读出来的样子。
#[derive(Default)]
pub struct LoadedLedger {
    pub state: PersistedState,
    pub mirrors: BTreeMap<String, Mirror>,
    pub baselines: BTreeMap<String, Value>,
    pub outbox: Vec<Op>,
    pub held: Vec<Op>,
}

/// 账本目录的读写口。
#[derive(Clone, Debug)]
pub struct LedgerFiles {
    dir: PathBuf,
}

impl LedgerFiles {
    /// `config_dir` 是应用配置目录；账本目录取自登记表。
    pub fn new(config_dir: &FsPath) -> Self {
        Self {
            dir: DataSourceId::SyncState.path(config_dir),
        }
    }

    pub fn dir(&self) -> &FsPath {
        &self.dir
    }

    fn file(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    fn mirror_file(&self, source: &str) -> PathBuf {
        self.dir.join("mirror").join(format!("{source}.json"))
    }

    fn baseline_file(&self, source: &str) -> PathBuf {
        self.dir.join("baseline").join(format!("{source}.json"))
    }

    pub fn history_file(&self) -> PathBuf {
        self.file("history.json")
    }

    fn write_json<T: Serialize>(&self, path: PathBuf, value: &T) -> Result<(), AppError> {
        let text = serde_json::to_string(value)
            .map_err(|e| AppError::Config(format!("序列化同步状态失败: {e}")))?;
        atomic_write(&path, &text)
            .map_err(|e| AppError::Config(format!("写入同步状态失败: {e}")))
    }

    fn read_json<T: serde::de::DeserializeOwned>(&self, path: &FsPath) -> Option<T> {
        let text = std::fs::read_to_string(path).ok()?;
        if text.trim().is_empty() {
            return None;
        }
        match serde_json::from_str(&text) {
            Ok(v) => Some(v),
            Err(e) => {
                // 账本损坏不应让应用起不来：按「没有」处理。镜像 / 基线丢了会在下一轮从云端重新学回来，
                // 最坏是本机还没上传的改动丢失（outbox），所以这里留一条显眼的日志。
                log::error!("同步状态文件 {} 无法解析，按空处理: {e}", path.display());
                None
            }
        }
    }

    // ── 写 ──

    pub fn save_state(&self, state: &PersistedState) -> Result<(), AppError> {
        self.write_json(self.file("state.json"), state)
    }

    pub fn save_mirror(&self, source: &str, mirror: &Mirror) -> Result<(), AppError> {
        debug_assert!(valid_source_key(source));
        self.write_json(self.mirror_file(source), mirror)
    }

    pub fn save_baseline(&self, source: &str, baseline: &Value) -> Result<(), AppError> {
        debug_assert!(valid_source_key(source));
        self.write_json(self.baseline_file(source), baseline)
    }

    pub fn save_outbox(&self, ops: &[Op]) -> Result<(), AppError> {
        self.write_json(self.file("outbox.json"), &ops)
    }

    pub fn save_held(&self, ops: &[Op]) -> Result<(), AppError> {
        self.write_json(self.file("held.json"), &ops)
    }

    // ── 读 ──

    /// 读整套账本。没有 `state.json` = 从没开过同步（返回默认的空账本）。
    pub fn load(&self) -> LoadedLedger {
        let Some(state) = self.read_json::<PersistedState>(&self.file("state.json")) else {
            return LoadedLedger::default();
        };
        let mut ledger = LoadedLedger {
            state,
            ..Default::default()
        };
        ledger.outbox = self.read_json(&self.file("outbox.json")).unwrap_or_default();
        ledger.held = self.read_json(&self.file("held.json")).unwrap_or_default();
        for (dir, is_mirror) in [("mirror", true), ("baseline", false)] {
            let Ok(entries) = std::fs::read_dir(self.dir.join(dir)) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Some(name) = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .and_then(|n| n.strip_suffix(".json"))
                else {
                    continue;
                };
                if !valid_source_key(name) {
                    continue;
                }
                let name = name.to_string();
                if is_mirror {
                    if let Some(m) = self.read_json::<Mirror>(&path) {
                        ledger.mirrors.insert(name, m);
                    }
                } else if let Some(b) = self.read_json::<Value>(&path) {
                    ledger.baselines.insert(name, b);
                }
            }
        }
        ledger
    }

    /// 删掉整个账本目录（停止同步）。目录不存在也算成功。
    pub fn wipe(&self) -> Result<(), AppError> {
        match std::fs::remove_dir_all(&self.dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(AppError::Config(format!("清除同步状态失败: {e}"))),
        }
    }
}

/// 哪些东西自上次落盘以来变过。
#[derive(Default)]
pub struct Dirty {
    pub state: bool,
    pub outbox: bool,
    pub held: bool,
    pub mirrors: BTreeSet<String>,
    pub baselines: BTreeSet<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::core::{Op, OpKind, Seg};
    use serde_json::json;

    fn op(source: &str, ms: u64) -> Op {
        Op {
            source: source.into(),
            path: vec![Seg::key("a")],
            kind: OpKind::Set(json!(ms)),
            hlc: Hlc {
                ms,
                counter: 0,
                device: "d".into(),
            },
        }
    }

    #[test]
    fn source_keys_are_validated_before_they_become_file_names() {
        for ok in ["settings", "quickCommands", "mcp-servers", "a_b", "X9"] {
            assert!(valid_source_key(ok), "{ok}");
        }
        for bad in ["", "../x", "a/b", "a\\b", "a.b", "x y", &"a".repeat(65), "设置"] {
            assert!(!valid_source_key(bad), "{bad:?}");
        }
    }

    #[test]
    fn ledger_round_trips_through_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let files = LedgerFiles::new(tmp.path());
        assert!(!files.load().state.enabled, "什么都没写过：空账本");

        let mut state = PersistedState {
            enabled: true,
            device_id: "dev1".into(),
            seq: 7,
            ..Default::default()
        };
        state.cursors.insert("dev2".into(), Cursor { snap: 3, seg: 9 });
        files.save_state(&state).unwrap();

        let mut mirror = Mirror::new();
        mirror.merge(&op("settings", 5));
        files.save_mirror("settings", &mirror).unwrap();
        files
            .save_baseline("settings", &json!({"fontSize": 14}))
            .unwrap();
        files.save_outbox(&[op("settings", 6)]).unwrap();
        files.save_held(&[op("connections", 8)]).unwrap();

        let back = files.load();
        assert_eq!(back.state, state);
        assert_eq!(back.mirrors["settings"], mirror);
        assert_eq!(back.baselines["settings"], json!({"fontSize": 14}));
        assert_eq!(back.outbox, vec![op("settings", 6)]);
        assert_eq!(back.held, vec![op("connections", 8)]);

        files.wipe().unwrap();
        assert!(!files.dir().exists());
        files.wipe().unwrap(); // 幂等
    }

    #[test]
    fn corrupt_ledger_files_degrade_to_empty_instead_of_failing() {
        let tmp = tempfile::tempdir().unwrap();
        let files = LedgerFiles::new(tmp.path());
        files
            .save_state(&PersistedState {
                enabled: true,
                ..Default::default()
            })
            .unwrap();
        std::fs::create_dir_all(files.dir().join("mirror")).unwrap();
        std::fs::write(files.dir().join("mirror").join("settings.json"), "{ broken").unwrap();
        std::fs::write(files.dir().join("outbox.json"), "not json").unwrap();
        // 文件名不合法的镜像文件直接忽略
        std::fs::write(files.dir().join("mirror").join("a.b.json"), "{}").unwrap();

        let back = files.load();
        assert!(back.state.enabled);
        assert!(back.mirrors.is_empty());
        assert!(back.outbox.is_empty());
    }

    #[test]
    fn state_json_uses_camel_case_and_tolerates_missing_fields() {
        let s: PersistedState =
            serde_json::from_str(r#"{"deviceId":"x","enabled":true,"futureField":1}"#).unwrap();
        assert_eq!(s.device_id, "x");
        assert!(s.enabled);
        assert_eq!(s.format, STATE_FORMAT);
        let v = serde_json::to_value(&s).unwrap();
        assert!(v.get("deviceId").is_some() && v.get("appliedMirror").is_some());
    }
}
