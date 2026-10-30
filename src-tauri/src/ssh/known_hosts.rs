//! TOFU (Trust-On-First-Use) host key store.
//!
//! Persists SHA-256 fingerprints of seen SSH server host keys to a JSON file
//! under the application's config directory. Subsequent connections compare
//! the presented key against the stored fingerprint and reject mismatches
//! unless the user explicitly elects to trust the new key.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use russh::keys::ssh_key::{HashAlg, PublicKey};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::data::{DataChangeBus, DataSourceId};
use crate::error::AppError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnownHostEntry {
    pub algorithm: String,
    pub fingerprint_sha256: String,
    pub first_seen: String,
    pub last_seen: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct KnownHostsFile {
    version: u32,
    entries: HashMap<String, KnownHostEntry>,
}

impl Default for KnownHostsFile {
    fn default() -> Self {
        Self {
            version: 1,
            entries: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum VerifyOutcome {
    /// First time seeing this host. Caller should record on success.
    TrustOnFirstUse,
    /// Presented fingerprint matches the stored one.
    Match(KnownHostEntry),
    /// Presented fingerprint differs from the stored one. MUST reject
    /// unless the user explicitly trusts the new key.
    Mismatch {
        stored: KnownHostEntry,
        presented: KnownHostEntry,
    },
}

pub struct KnownHostsStore {
    path: PathBuf,
    inner: RwLock<KnownHostsFile>,
    /// 每次**成功写盘**后广播 `DataSourceId::KnownHosts`（以后同步引擎订阅）。
    /// `None` = 不广播（临时 fallback 文件、纯内存 store、无头模式、测试）。
    bus: Option<DataChangeBus>,
    /// 启动加载时碰到了损坏的文件（已隔离成 `.corrupt.<ts>.bak`、改用空 store）。
    /// 跨设备同步据此把这份数据标成「需要从镜像恢复」，而不是把空表当成用户删光了所有主机。
    recovered_from_corruption: bool,
}

impl KnownHostsStore {
    /// Create a purely in-memory store with no backing file.
    /// Used as the ultimate fallback when even the temp directory is not writable.
    /// `record`/`replace`/`forget` update the in-memory map but never persist to disk.
    pub fn in_memory() -> Arc<Self> {
        Arc::new(Self {
            path: PathBuf::new(),
            inner: RwLock::new(KnownHostsFile::default()),
            bus: None,
            recovered_from_corruption: false,
        })
    }

    /// 从配置目录里登记表指定的位置（`DataSourceId::KnownHosts`）加载。
    ///
    /// GUI 与无头 `--mcp-server` **必须读同一份**（否则无头模式把每台主机都当首次见到，
    /// 指纹不匹配永不触发），所以两边都经这个入口取路径，而不是各拼各的。
    /// `bus` 是可选的变更广播，写盘成功后发 `KnownHosts`；其它行为与 [`Self::load`] 完全一致。
    pub async fn load_in_config_dir(
        config_dir: &Path,
        bus: Option<DataChangeBus>,
    ) -> Result<Arc<Self>, AppError> {
        Self::load_with_bus(DataSourceId::KnownHosts.path(config_dir), bus).await
    }

    /// Load store from disk. Missing file is treated as empty. Corrupt JSON
    /// is renamed aside (`.corrupt.<ts>.bak`) so that the user is forced to
    /// re-confirm host identities rather than silently re-TOFU on next start.
    pub async fn load(path: PathBuf) -> Result<Arc<Self>, AppError> {
        Self::load_with_bus(path, None).await
    }

    async fn load_with_bus(
        path: PathBuf,
        bus: Option<DataChangeBus>,
    ) -> Result<Arc<Self>, AppError> {
        let mut recovered_from_corruption = false;
        let inner = if path.exists() {
            match fs::read_to_string(&path) {
                Ok(content) if content.trim().is_empty() => KnownHostsFile::default(),
                Ok(content) => match serde_json::from_str::<KnownHostsFile>(&content) {
                    Ok(file) => file,
                    Err(e) => {
                        log::error!(
                            "known_hosts.json 解析失败: {}; 隔离原文件并以空 store 启动",
                            e
                        );
                        let ts = chrono::Utc::now().timestamp();
                        // Append a `.corrupt.<ts>.bak` suffix WITHOUT replacing
                        // the existing extension. `Path::with_extension` would
                        // overwrite e.g. `.json` and silently drop the original
                        // filename, so we build the new filename by hand.
                        let mut new_name = path
                            .file_name()
                            .map(|n| n.to_os_string())
                            .unwrap_or_else(|| std::ffi::OsString::from("known_hosts"));
                        new_name.push(format!(".corrupt.{}.bak", ts));
                        let bak = path.with_file_name(new_name);
                        let _ = fs::rename(&path, &bak);
                        recovered_from_corruption = true;
                        KnownHostsFile::default()
                    }
                },
                Err(e) => {
                    return Err(AppError::Config(format!("读取 known_hosts 失败: {}", e)));
                }
            }
        } else {
            KnownHostsFile::default()
        };

        Ok(Arc::new(Self {
            path,
            inner: RwLock::new(inner),
            bus,
            recovered_from_corruption,
        }))
    }

    /// 启动时是否因文件损坏而改用了空表（见字段说明）。
    pub fn recovered_from_corruption(&self) -> bool {
        self.recovered_from_corruption
    }

    /// Compute (algorithm, fingerprint) of a server public key.
    /// Fingerprint is SHA-256 in OpenSSH form, e.g. "SHA256:abcd...".
    pub fn fingerprint(key: &PublicKey) -> (String, String) {
        let algo = key.algorithm().as_str().to_string();
        let fp = key.fingerprint(HashAlg::Sha256).to_string();
        (algo, fp)
    }

    fn key_of(host: &str, port: u16) -> String {
        format!("{}:{}", host.to_lowercase(), port)
    }

    /// Verify a presented public key against the store. Does not mutate.
    pub async fn verify(&self, host: &str, port: u16, key: &PublicKey) -> VerifyOutcome {
        let (algo, fp) = Self::fingerprint(key);
        let now = Utc::now().to_rfc3339();
        let presented = KnownHostEntry {
            algorithm: algo,
            fingerprint_sha256: fp,
            first_seen: now.clone(),
            last_seen: now,
        };
        let id = Self::key_of(host, port);
        let guard = self.inner.read().await;
        match guard.entries.get(&id) {
            None => VerifyOutcome::TrustOnFirstUse,
            Some(stored) => {
                if stored.fingerprint_sha256 == presented.fingerprint_sha256 {
                    VerifyOutcome::Match(stored.clone())
                } else {
                    VerifyOutcome::Mismatch {
                        stored: stored.clone(),
                        presented,
                    }
                }
            }
        }
    }

    /// Record a new entry (first-time TOFU). If an entry already exists, only
    /// `last_seen` is refreshed; this method must NOT silently overwrite a
    /// differing fingerprint �?call [`replace`] for that.
    pub async fn record(
        &self,
        host: &str,
        port: u16,
        entry: KnownHostEntry,
    ) -> Result<(), AppError> {
        let id = Self::key_of(host, port);
        let mut guard = self.inner.write().await;
        match guard.entries.get_mut(&id) {
            Some(existing) if existing.fingerprint_sha256 == entry.fingerprint_sha256 => {
                existing.last_seen = entry.last_seen;
            }
            Some(_) => {
                // Differing fingerprint: refuse silent overwrite.
                return Err(AppError::Other(format!(
                    "refusing to overwrite known host entry for {} (use replace())",
                    id
                )));
            }
            None => {
                guard.entries.insert(id, entry);
            }
        }
        self.persist_and_notify(&guard)
    }

    /// Replace an existing entry with a new fingerprint after explicit user
    /// confirmation.
    pub async fn replace(
        &self,
        host: &str,
        port: u16,
        entry: KnownHostEntry,
    ) -> Result<(), AppError> {
        let id = Self::key_of(host, port);
        let mut guard = self.inner.write().await;
        guard.entries.insert(id, entry);
        self.persist_and_notify(&guard)
    }

    pub async fn forget(&self, host: &str, port: u16) -> Result<(), AppError> {
        let id = Self::key_of(host, port);
        let mut guard = self.inner.write().await;
        guard.entries.remove(&id);
        self.persist_and_notify(&guard)
    }

    pub async fn list(&self) -> Vec<(String, KnownHostEntry)> {
        self.inner
            .read()
            .await
            .entries
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// 整份文档的 JSON（跨设备同步读它做差异捕获）。
    pub async fn sync_json(&self) -> Result<serde_json::Value, AppError> {
        let guard = self.inner.read().await;
        serde_json::to_value(&*guard)
            .map_err(|e| AppError::Config(format!("序列化 known_hosts 失败: {e}")))
    }

    /// 在**写锁内**对整份文档做一次「读 → 改 → 写」，供跨设备同步使用。
    ///
    /// `f` 拿到当前文档的 JSON，返回 `Some(新 JSON)` 就换入并写盘（`None` = 不改）。写锁在
    /// `f` 执行期间一直持有，所以同步引擎「先捕获本机改动、再落地远端改动」是一个整体，
    /// 中间不会有 `record` / `replace` / `forget` 插队被覆盖。
    ///
    /// 新 JSON 里缺 `first_seen` / `last_seen` 的条目（这两个字段是本机的，不跨设备同步）用
    /// 当前时间补上；缺指纹或算法的条目视为无效、丢掉。返回 `(是否写了盘, 最终文档 JSON)`。
    pub async fn sync_update<F>(&self, f: F) -> Result<(bool, serde_json::Value), AppError>
    where
        F: FnOnce(&serde_json::Value) -> Result<Option<serde_json::Value>, AppError> + Send,
    {
        let mut guard = self.inner.write().await;
        let current = serde_json::to_value(&*guard)
            .map_err(|e| AppError::Config(format!("序列化 known_hosts 失败: {e}")))?;
        let Some(next) = f(&current)? else {
            return Ok((false, current));
        };
        let next_file = known_hosts_file_from_json(next, &guard);
        let next_json = serde_json::to_value(&next_file)
            .map_err(|e| AppError::Config(format!("序列化 known_hosts 失败: {e}")))?;
        if next_json == current {
            return Ok((false, current));
        }
        Self::persist_locked(&self.path, &next_file)?;
        *guard = next_file;
        if let Some(bus) = &self.bus {
            if !self.path.as_os_str().is_empty() {
                bus.publish(DataSourceId::KnownHosts);
            }
        }
        Ok((true, next_json))
    }

    /// 写盘；真的写成功了才广播（纯内存 store 没有文件，不广播）。
    fn persist_and_notify(&self, file: &KnownHostsFile) -> Result<(), AppError> {
        Self::persist_locked(&self.path, file)?;
        if let Some(bus) = &self.bus {
            if !self.path.as_os_str().is_empty() {
                bus.publish(DataSourceId::KnownHosts);
            }
        }
        Ok(())
    }

    /// Atomic write: tmp + fsync + rename. Skips write for in-memory stores
    /// (empty path).
    fn persist_locked(path: &Path, file: &KnownHostsFile) -> Result<(), AppError> {
        if path.as_os_str().is_empty() {
            return Ok(()); // in-memory store, nothing to persist
        }
        let json = serde_json::to_string_pretty(file)
            .map_err(|e| AppError::Config(format!("serialize known_hosts: {}", e)))?;
        crate::config::persist::atomic_write(path, &json)
            .map_err(|e| AppError::Config(format!("write known_hosts: {}", e)))
    }
}

/// 同步落地得到的 JSON → 内存结构。`previous` 里同一条目的 `first_seen` / `last_seen` 沿用，
/// 全新的条目用当前时间。
fn known_hosts_file_from_json(json: serde_json::Value, previous: &KnownHostsFile) -> KnownHostsFile {
    let now = Utc::now().to_rfc3339();
    let mut entries = HashMap::new();
    if let Some(map) = json.get("entries").and_then(|v| v.as_object()) {
        for (host_port, entry) in map {
            let text = |k: &str| entry.get(k).and_then(|v| v.as_str()).map(str::to_string);
            let (Some(algorithm), Some(fingerprint_sha256)) =
                (text("algorithm"), text("fingerprint_sha256"))
            else {
                continue;
            };
            let kept = previous.entries.get(host_port);
            entries.insert(
                host_port.clone(),
                KnownHostEntry {
                    algorithm,
                    fingerprint_sha256,
                    first_seen: text("first_seen")
                        .or_else(|| kept.map(|e| e.first_seen.clone()))
                        .unwrap_or_else(|| now.clone()),
                    last_seen: text("last_seen")
                        .or_else(|| kept.map(|e| e.last_seen.clone()))
                        .unwrap_or_else(|| now.clone()),
                },
            );
        }
    }
    KnownHostsFile {
        version: previous.version,
        entries,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh::keys::ssh_key::PublicKey;
    use std::str::FromStr;
    use tempfile::TempDir;

    // Two distinct hard-coded ed25519 public keys (OpenSSH format) used to
    // exercise TOFU logic without needing an RNG that satisfies the
    // ssh-key/signature CryptoRng bound (which differs across rand versions).
    const KEY_A: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBHWGOIRJ4XqkiQGFm1B/LFRfWOQbW6q0RQGJ8U/+RsR a@example";
    const KEY_B: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIK4qVQ4uH2GgYHN5pGM5OoH7e3Ckj1iaC0bKqnKvX9XK b@example";

    fn key_a() -> PublicKey {
        PublicKey::from_str(KEY_A).expect("parse KEY_A")
    }
    fn key_b() -> PublicKey {
        PublicKey::from_str(KEY_B).expect("parse KEY_B")
    }

    #[tokio::test]
    async fn fingerprint_is_sha256_format() {
        let k = key_a();
        let (_algo, fp) = KnownHostsStore::fingerprint(&k);
        assert!(fp.starts_with("SHA256:"), "got: {}", fp);
    }

    #[tokio::test]
    async fn verify_first_time_then_match() {
        let dir = TempDir::new().unwrap();
        let store = KnownHostsStore::load(dir.path().join("known.json"))
            .await
            .unwrap();
        let k = key_a();
        let outcome = store.verify("example.com", 22, &k).await;
        assert!(matches!(outcome, VerifyOutcome::TrustOnFirstUse));
        let (algo, fp) = KnownHostsStore::fingerprint(&k);
        let now = Utc::now().to_rfc3339();
        store
            .record(
                "example.com",
                22,
                KnownHostEntry {
                    algorithm: algo,
                    fingerprint_sha256: fp,
                    first_seen: now.clone(),
                    last_seen: now,
                },
            )
            .await
            .unwrap();
        let outcome = store.verify("example.com", 22, &k).await;
        assert!(matches!(outcome, VerifyOutcome::Match(_)));
    }

    #[tokio::test]
    async fn mismatch_is_detected() {
        let dir = TempDir::new().unwrap();
        let store = KnownHostsStore::load(dir.path().join("k.json"))
            .await
            .unwrap();
        let k1 = key_a();
        let k2 = key_b();
        let (algo, fp) = KnownHostsStore::fingerprint(&k1);
        let now = Utc::now().to_rfc3339();
        store
            .record(
                "h",
                22,
                KnownHostEntry {
                    algorithm: algo,
                    fingerprint_sha256: fp,
                    first_seen: now.clone(),
                    last_seen: now,
                },
            )
            .await
            .unwrap();
        let outcome = store.verify("h", 22, &k2).await;
        assert!(matches!(outcome, VerifyOutcome::Mismatch { .. }));
    }

    #[tokio::test]
    async fn host_key_normalized_lowercase_and_port_separated() {
        let dir = TempDir::new().unwrap();
        let store = KnownHostsStore::load(dir.path().join("k.json"))
            .await
            .unwrap();
        let k = key_a();
        let (algo, fp) = KnownHostsStore::fingerprint(&k);
        let now = Utc::now().to_rfc3339();
        store
            .record(
                "Example.COM",
                22,
                KnownHostEntry {
                    algorithm: algo,
                    fingerprint_sha256: fp,
                    first_seen: now.clone(),
                    last_seen: now,
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            store.verify("example.com", 22, &k).await,
            VerifyOutcome::Match(_)
        ));
        assert!(matches!(
            store.verify("example.com", 2222, &k).await,
            VerifyOutcome::TrustOnFirstUse
        ));
    }

    #[tokio::test]
    async fn persist_and_reload() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("k.json");
        let store = KnownHostsStore::load(path.clone()).await.unwrap();
        let k = key_a();
        let (algo, fp) = KnownHostsStore::fingerprint(&k);
        let now = Utc::now().to_rfc3339();
        store
            .record(
                "h",
                22,
                KnownHostEntry {
                    algorithm: algo,
                    fingerprint_sha256: fp,
                    first_seen: now.clone(),
                    last_seen: now,
                },
            )
            .await
            .unwrap();
        drop(store);
        let store2 = KnownHostsStore::load(path).await.unwrap();
        assert!(matches!(
            store2.verify("h", 22, &k).await,
            VerifyOutcome::Match(_)
        ));
    }

    fn entry_for(key: &PublicKey) -> KnownHostEntry {
        let (algo, fp) = KnownHostsStore::fingerprint(key);
        let now = Utc::now().to_rfc3339();
        KnownHostEntry {
            algorithm: algo,
            fingerprint_sha256: fp,
            first_seen: now.clone(),
            last_seen: now,
        }
    }

    /// 从配置目录加载走登记表里的 `known_hosts.json`，GUI 与无头模式因此读同一份。
    #[tokio::test]
    async fn load_in_config_dir_uses_the_registered_location() {
        let dir = TempDir::new().unwrap();
        let store = KnownHostsStore::load_in_config_dir(dir.path(), None)
            .await
            .unwrap();
        store.record("h", 22, entry_for(&key_a())).await.unwrap();
        assert!(
            DataSourceId::KnownHosts.path(dir.path()).is_file(),
            "必须写在登记表指定的位置"
        );
        assert_eq!(DataSourceId::KnownHosts.rel_path(), "known_hosts.json");
    }

    /// 每次成功写盘后广播 `KnownHosts`（record / replace / forget 各一次）；
    /// 没有订阅者或没有总线时写入照常。
    #[tokio::test]
    async fn successful_writes_publish_a_known_hosts_change() {
        let dir = TempDir::new().unwrap();
        let bus = DataChangeBus::new();
        let mut rx = bus.subscribe();
        let store = KnownHostsStore::load_in_config_dir(dir.path(), Some(bus))
            .await
            .unwrap();

        store.record("h", 22, entry_for(&key_a())).await.unwrap();
        store.replace("h", 22, entry_for(&key_b())).await.unwrap();
        store.forget("h", 22).await.unwrap();

        for _ in 0..3 {
            assert_eq!(rx.try_recv().unwrap().source, DataSourceId::KnownHosts);
        }
        assert!(rx.try_recv().is_err(), "三次写入就是三条广播");
    }

    /// 被拒绝的写入（指纹不同的 record）没有落盘，也不得广播；
    /// 校验结果（Mismatch）不受总线影响。
    #[tokio::test]
    async fn rejected_record_does_not_publish_and_mismatch_still_detected() {
        let dir = TempDir::new().unwrap();
        let bus = DataChangeBus::new();
        let store = KnownHostsStore::load_in_config_dir(dir.path(), Some(bus.clone()))
            .await
            .unwrap();
        store.record("h", 22, entry_for(&key_a())).await.unwrap();
        let mut rx = bus.subscribe();

        let err = store.record("h", 22, entry_for(&key_b())).await;
        assert!(err.is_err(), "指纹不同不得静默覆盖");
        assert!(rx.try_recv().is_err(), "被拒绝的写入不得广播");
        assert!(matches!(
            store.verify("h", 22, &key_b()).await,
            VerifyOutcome::Mismatch { .. }
        ));
    }

    /// 纯内存 store 没有文件可写，自然也不广播（也没有总线可发）。
    #[tokio::test]
    async fn in_memory_store_still_works_without_a_bus() {
        let store = KnownHostsStore::in_memory();
        store.record("h", 22, entry_for(&key_a())).await.unwrap();
        assert!(matches!(
            store.verify("h", 22, &key_a()).await,
            VerifyOutcome::Match(_)
        ));
    }
}
