//! 存储后端的构造：把 [`StorageConfig`] 变成可用的 [`SyncStorage`]。
//!
//! 引擎只依赖 [`Backend`] / [`BackendFactory`]；生产实现 [`ProductionBackends`] 用 GUI 那份
//! `SshManager`（known_hosts 同一份，主机密钥校验照常）和连接表，测试实现直接交出一个共享的
//! `MemoryStorage`。

use std::sync::Arc;

use async_trait::async_trait;

use super::storage::{
    MemoryStorage, SftpStorage, StorageConfig, StorageError, SyncStorage, WebDavStorage,
};
use crate::config::connections::ConnectionStore;
use crate::data::JsonStore;
use crate::ssh::connection::SshManager;

/// 同步存储 + 引擎额外需要的两件事。
#[async_trait]
pub trait Backend: SyncStorage {
    /// 同步目录在存储端的**绝对路径**（只有 SFTP 有意义）：创建同步时把它加进受保护路径，
    /// 防止 Agent 把同步目录删了。
    async fn protected_root(&self) -> Option<String> {
        None
    }

    /// 断开长连接（停止同步、换存储位置时）。默认什么都不用做。
    async fn close(&self) {}
}

#[async_trait]
impl Backend for MemoryStorage {}

#[async_trait]
impl Backend for WebDavStorage {}

#[async_trait]
impl Backend for SftpStorage {
    async fn protected_root(&self) -> Option<String> {
        self.resolved_root().await.ok()
    }

    async fn close(&self) {
        SftpStorage::close(self).await;
    }
}

pub trait BackendFactory: Send + Sync {
    /// 按配置构造后端。**不做网络 IO**（SFTP 惰性连接）。`secret` 是 WebDAV 的应用密码，SFTP 忽略。
    fn build(
        &self,
        config: &StorageConfig,
        secret: Option<&str>,
    ) -> Result<Arc<dyn Backend>, StorageError>;
}

pub struct ProductionBackends {
    ssh: SshManager,
    connections: JsonStore<ConnectionStore>,
}

impl ProductionBackends {
    pub fn new(ssh: SshManager, connections: JsonStore<ConnectionStore>) -> Self {
        Self { ssh, connections }
    }
}

impl BackendFactory for ProductionBackends {
    fn build(
        &self,
        config: &StorageConfig,
        secret: Option<&str>,
    ) -> Result<Arc<dyn Backend>, StorageError> {
        match config {
            StorageConfig::WebDav {
                url,
                username,
                root,
            } => Ok(Arc::new(WebDavStorage::new(
                url,
                username,
                secret.unwrap_or(""),
                root,
            )?)),
            StorageConfig::Sftp {
                connection_id,
                root,
            } => Ok(Arc::new(SftpStorage::for_saved_connection(
                self.ssh.clone(),
                self.connections.clone(),
                connection_id,
                root,
            ))),
        }
    }
}

/// 测试用：所有设备共用同一个 `MemoryStorage`（= 同一个云端）。
pub struct MemoryBackends {
    pub storage: MemoryStorage,
}

impl BackendFactory for MemoryBackends {
    fn build(
        &self,
        _config: &StorageConfig,
        _secret: Option<&str>,
    ) -> Result<Arc<dyn Backend>, StorageError> {
        Ok(Arc::new(self.storage.clone()))
    }
}
