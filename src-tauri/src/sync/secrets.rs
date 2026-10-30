//! 同步引擎碰到的机密（数据密钥、恢复密钥、存储密码，以及加入时搬运的连接密码）的存取口。
//!
//! 生产实现 [`KeychainSecrets`] 就是 `config::keychain`；抽成 trait 的唯一理由是**测试不能碰真实的
//! 系统密钥链**（以前出过单元测试误删本机真实条目的事故）——测试里用 [`MemorySecrets`]。
//! 引擎里所有读写机密的地方都经它，没有别的出口。

use std::collections::HashMap;

use parking_lot::Mutex;

use crate::config::keychain::{self, SecretSlot};
use crate::error::AppError;

pub trait SyncSecretStore: Send + Sync {
    fn get(&self, slot: &SecretSlot) -> Result<Option<String>, AppError>;
    fn set(&self, slot: &SecretSlot, secret: &str) -> Result<(), AppError>;
    /// 不存在也算成功。
    fn delete(&self, slot: &SecretSlot) -> Result<(), AppError>;
}

/// 系统密钥链（桌面）/ Android Keystore。
pub struct KeychainSecrets;

impl SyncSecretStore for KeychainSecrets {
    fn get(&self, slot: &SecretSlot) -> Result<Option<String>, AppError> {
        keychain::get(slot)
    }
    fn set(&self, slot: &SecretSlot, secret: &str) -> Result<(), AppError> {
        keychain::set(slot, secret)
    }
    fn delete(&self, slot: &SecretSlot) -> Result<(), AppError> {
        keychain::delete(slot)
    }
}

/// 内存实现：测试用（也可以当作「不碰系统密钥链」的占位）。
#[derive(Default)]
pub struct MemorySecrets {
    map: Mutex<HashMap<String, String>>,
}

impl MemorySecrets {
    pub fn new() -> Self {
        Self::default()
    }

    /// 当前有多少条（测试断言「用完清干净」用）。
    pub fn len(&self) -> usize {
        self.map.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl SyncSecretStore for MemorySecrets {
    fn get(&self, slot: &SecretSlot) -> Result<Option<String>, AppError> {
        Ok(self.map.lock().get(slot.account().as_ref()).cloned())
    }
    fn set(&self, slot: &SecretSlot, secret: &str) -> Result<(), AppError> {
        self.map
            .lock()
            .insert(slot.account().into_owned(), secret.to_string());
        Ok(())
    }
    fn delete(&self, slot: &SecretSlot) -> Result<(), AppError> {
        self.map.lock().remove(slot.account().as_ref());
        Ok(())
    }
}
