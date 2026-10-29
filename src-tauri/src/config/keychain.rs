use std::borrow::Cow;

use crate::data::SyncTier;
use crate::error::AppError;

/// Service name used in the system keychain. All entries for this app are scoped
/// under this service to avoid colliding with other apps.
const SERVICE: &str = "com.marcel.ssh";

// ── Android ──────────────────────────────────────────────────────────────────
// keyring v4 的 `v1` 便利模块只覆盖 macOS/Windows/Linux，Android 不在其中
// （v1.rs set_credential_store 对 Android 无分支，Entry::new 返回 NoDefaultStore）。
// 因此 Android 直接用 android-native-keyring-store（SharedPreferences + Android
// Keystore AES 加密，hardware-backed），通过 keyring-core 的 Entry API 操作。
// ndk-context 由 Tauri Android 在启动时初始化（util.rs 已在用 android_context()）。

#[cfg(target_os = "android")]
mod android_store {
    use std::sync::{Arc, OnceLock};

    use keyring_core::{api::CredentialStoreApi, Entry, Error, Result};

    /// 单例 store，避免每次 Entry::new 都重建 vault 连接。
    fn store() -> Result<&'static Arc<android_native_keyring_store::Store>> {
        static STORE: OnceLock<Result<Arc<android_native_keyring_store::Store>>> = OnceLock::new();
        STORE
            .get_or_init(android_native_keyring_store::Store::new)
            .as_ref()
            .map_err(|e| {
                Error::PlatformFailure(
                    std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("Android 密钥存储初始化失败：{e}"),
                    )
                    .into(),
                )
            })
    }

    pub fn entry(service: &str, user: &str) -> Result<Entry> {
        store()?.build(service, user, None)
    }

    /// 把 keyring-core 的 NoEntry 映射为 Ok(None)，其他错误原样冒泡。
    pub fn get_optional_password(entry: &Entry) -> Result<Option<String>> {
        match entry.get_password() {
            Ok(pw) => Ok(Some(pw)),
            Err(Error::NoEntry) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// 删除条目，NoEntry 视为成功（与桌面端 delete_password 语义一致）。
    pub fn delete_if_exists(entry: &Entry) -> Result<()> {
        match entry.delete_credential() {
            Ok(()) | Err(Error::NoEntry) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(target_os = "android")]
use android_store as store_impl;

// ── Desktop ──────────────────────────────────────────────────────────────────

#[cfg(not(target_os = "android"))]
mod desktop_store {
    use keyring::{Entry, Error, Result};

    pub fn entry(service: &str, user: &str) -> Result<Entry> {
        Entry::new(service, user)
    }

    pub fn get_optional_password(entry: &Entry) -> Result<Option<String>> {
        match entry.get_password() {
            Ok(pw) => Ok(Some(pw)),
            Err(Error::NoEntry) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn delete_if_exists(entry: &Entry) -> Result<()> {
        match entry.delete_credential() {
            Ok(()) | Err(Error::NoEntry) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(not(target_os = "android"))]
use desktop_store as store_impl;

// ── 槽位表 ───────────────────────────────────────────────────────────────────

/// 密钥链里每一条凭据的「身份」：**本应用写进系统密钥链的全部条目的唯一来源。**
///
/// 为什么要一张表：以前每个 Key 各写一组 `get_/save_/delete_` 函数，账号名散在十几处字符串
/// 字面量里，「密钥链里到底存了哪些东西、哪些该跟着设备走」没有一个地方能回答。以后跨设备
/// 同步要按档导出 / 导入密钥链条目，就必须能枚举并分档——所以账号名、分档、说明都收在这个
/// 枚举上，`account()` / `tier()` / `describe()` 的 `match` 都不写 `_ =>`，新增变体不补全就
/// 编译不过。
///
/// **账号名一个字都不能改**：它就是老用户密钥链里条目的名字，改了等于所有人的 Key 凭空消失。
/// `tests` 里的快照表钉着这一点。
///
/// 带参数的变体（渠道 / SSH 连接）的参数是渠道 id / 连接 id，**不是机密**，可以进日志与
/// `Debug`；机密本身（`get` 的返回值）永远不要。
///
/// 读写一律走本模块的 [`get`] / [`set`] / [`delete`] / [`has`]，别再另拼账号名去碰密钥链。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SecretSlot {
    /// 旧版单配置的 LLM API Key（账号 `llm_api_key`）。
    ///
    /// 多渠道改造后不再写入，也**不删**（兼容旧版回滚）：仍被读取的地方是旧配置迁移
    /// （`llm::registry::migrate_legacy_settings`）与设置页拉模型列表的兜底。
    LegacyLlmApiKey,
    /// 某个模型渠道的 API Key（账号 `llm_channel_{渠道 id}`）。多渠道下每个渠道独立存放。
    LlmChannel(String),
    /// Brave / Tavily 网页搜索共用的 Key（账号 `web_search_api_key`）。
    ///
    /// 由用户当前选的搜索提供商轮流占用，所以**不能**借给 DeepSeek / 自定义地址 / Tavily 抓取。
    WebSearchApiKey,
    /// DeepSeek 工具类 Key，搜索与网页获取共用（账号 `deepseek_tools_api_key`）。
    ///
    /// 独立槽位，不并进 `WebSearchApiKey`（会把 Brave 的 Key 发给 DeepSeek），也不复用渠道
    /// Key（渠道 Key 随渠道增删，工具类接口不属于任何渠道）。
    DeepseekToolsApiKey,
    /// 用户自配的「自定义搜索 API」的 Key（账号 `custom_search_api_key`）。
    ///
    /// 地址是用户自己填的任意主机，借别家的 Key 给它就是把凭据发给一个完全不认识的服务。
    CustomSearchApiKey,
    /// 用户自配的「自定义网页获取」服务的 Key（账号 `custom_fetch_api_key`）。
    CustomFetchApiKey,
    /// Tavily Extract（网页获取）的 Key（账号 `tavily_fetch_api_key`）。
    ///
    /// 与 `WebSearchApiKey` 分开：那个槽位可能装着 Brave 的 Key。抓取时优先用这里；留空且
    /// 搜索提供商恰好是 Tavily 才沿用搜索那把（见 `http_get`）。
    TavilyFetchApiKey,
    /// TypeSafe（Jev）命令审批引擎的 Key（账号 `jev_api_key`）。全局一份，不分渠道。
    JevApiKey,
    /// 某个 SSH 连接的登录密码（账号 = 连接 id 本身，没有前缀）。
    SshPassword(String),
    /// 某个 SSH 连接的私钥口令（账号 `pk:{连接 id}`）。
    SshKeyPassphrase(String),
    /// 某个连接的跳板机登录密码（账号 `jump:{连接 id}`）。
    JumpPassword(String),
    /// 某个连接的跳板机私钥口令（账号 `jump:pk:{连接 id}`）。
    JumpKeyPassphrase(String),
    /// 私钥库的主密钥（账号 `keys:master`）：32 字节 AES 密钥的 base64，用来加解密
    /// `keys/` 目录里的私钥密文（见 `ssh::key_store`）。
    ///
    /// **设备绑定**：它只在生成它的这台设备上有效，密文换设备解不开，所以是 `Local`，
    /// 以后的机密同步绝不能把它导出去（导过去也没用，反而多一份泄露面）。
    KeyStoreMaster,
}

impl SecretSlot {
    /// 系统密钥链里的账号名（`SERVICE` 下的 user 字段）。**逐字不可改**，见类型文档。
    pub fn account(&self) -> Cow<'static, str> {
        match self {
            SecretSlot::LegacyLlmApiKey => Cow::Borrowed("llm_api_key"),
            SecretSlot::LlmChannel(id) => Cow::Owned(format!("llm_channel_{}", id)),
            SecretSlot::WebSearchApiKey => Cow::Borrowed("web_search_api_key"),
            SecretSlot::DeepseekToolsApiKey => Cow::Borrowed("deepseek_tools_api_key"),
            SecretSlot::CustomSearchApiKey => Cow::Borrowed("custom_search_api_key"),
            SecretSlot::CustomFetchApiKey => Cow::Borrowed("custom_fetch_api_key"),
            SecretSlot::TavilyFetchApiKey => Cow::Borrowed("tavily_fetch_api_key"),
            SecretSlot::JevApiKey => Cow::Borrowed("jev_api_key"),
            SecretSlot::SshPassword(id) => Cow::Owned(id.clone()),
            SecretSlot::SshKeyPassphrase(id) => Cow::Owned(format!("pk:{}", id)),
            SecretSlot::JumpPassword(id) => Cow::Owned(format!("jump:{}", id)),
            SecretSlot::JumpKeyPassphrase(id) => Cow::Owned(format!("jump:pk:{}", id)),
            SecretSlot::KeyStoreMaster => Cow::Borrowed("keys:master"),
        }
    }

    /// 同步分档：用户的各种 Key / 密码是 [`SyncTier::Secret`]（机密同步的单独开关管它）；
    /// 设备绑定的主密钥是 [`SyncTier::Local`]（永不同步）。
    pub fn tier(&self) -> SyncTier {
        match self {
            SecretSlot::LegacyLlmApiKey
            | SecretSlot::LlmChannel(_)
            | SecretSlot::WebSearchApiKey
            | SecretSlot::DeepseekToolsApiKey
            | SecretSlot::CustomSearchApiKey
            | SecretSlot::CustomFetchApiKey
            | SecretSlot::TavilyFetchApiKey
            | SecretSlot::JevApiKey
            | SecretSlot::SshPassword(_)
            | SecretSlot::SshKeyPassphrase(_)
            | SecretSlot::JumpPassword(_)
            | SecretSlot::JumpKeyPassphrase(_) => SyncTier::Secret,
            SecretSlot::KeyStoreMaster => SyncTier::Local,
        }
    }

    /// 一句话中文名（不含参数）。同时拼进密钥链读写失败的错误信息（「保存{名}到密钥链失败」），
    /// 所以保持短、能直接接在动词后面。
    pub fn describe(&self) -> &'static str {
        match self {
            SecretSlot::LegacyLlmApiKey => "旧版 LLM API Key",
            SecretSlot::LlmChannel(_) => "渠道 API Key",
            SecretSlot::WebSearchApiKey => "搜索 API Key",
            SecretSlot::DeepseekToolsApiKey => "DeepSeek API Key",
            SecretSlot::CustomSearchApiKey => "自定义搜索 API Key",
            SecretSlot::CustomFetchApiKey => "自定义抓取 API Key",
            SecretSlot::TavilyFetchApiKey => "Tavily 抓取 API Key",
            SecretSlot::JevApiKey => "TypeSafe API Key",
            SecretSlot::SshPassword(_) => "连接密码",
            SecretSlot::SshKeyPassphrase(_) => "私钥口令",
            SecretSlot::JumpPassword(_) => "跳板机密码",
            SecretSlot::JumpKeyPassphrase(_) => "跳板机私钥口令",
            SecretSlot::KeyStoreMaster => "私钥库主密钥",
        }
    }

    /// 一个已保存连接名下的全部槽位（登录密码 / 私钥口令 / 跳板机密码 / 跳板机私钥口令）。
    ///
    /// 删连接要把它们一并清掉，以后同步按连接导出凭据也走这里——**新增「跟着连接走」的
    /// 变体必须加进来**，否则删连接会在密钥链里留下孤儿条目。
    pub fn for_connection(connection_id: &str) -> [SecretSlot; 4] {
        [
            SecretSlot::SshPassword(connection_id.to_string()),
            SecretSlot::SshKeyPassphrase(connection_id.to_string()),
            SecretSlot::JumpPassword(connection_id.to_string()),
            SecretSlot::JumpKeyPassphrase(connection_id.to_string()),
        ]
    }

    /// 全部**不带参数**的槽位（枚举用：同步引擎按档导出、守护测试查重）。
    ///
    /// 带参数的槽位（渠道 / 连接）由各自的 id 现拼，没法在这里列出；它们的 id 来自
    /// 设置里的渠道清单与已保存的连接。**新增不带参数的变体必须同步加进这里**
    /// （`tests::fixed_slots_cover_every_unit_variant` 会提醒）。
    pub fn fixed_slots() -> Vec<SecretSlot> {
        vec![
            SecretSlot::LegacyLlmApiKey,
            SecretSlot::WebSearchApiKey,
            SecretSlot::DeepseekToolsApiKey,
            SecretSlot::CustomSearchApiKey,
            SecretSlot::CustomFetchApiKey,
            SecretSlot::TavilyFetchApiKey,
            SecretSlot::JevApiKey,
            SecretSlot::KeyStoreMaster,
        ]
    }
}

// ── 读写 ─────────────────────────────────────────────────────────────────────
//
// 平台分流（桌面 keyring / Android）全在上面的 `store_impl`，这里只管「槽位 → 账号名」。
// 错误信息只带槽位的中文名与底层错误，**绝不带机密本身**。

/// 把机密写进槽位（已有则覆盖）。
pub fn set(slot: &SecretSlot, secret: &str) -> Result<(), AppError> {
    let entry = store_impl::entry(SERVICE, &slot.account())
        .map_err(|e| AppError::Config(format!("密钥链初始化失败：{}", e)))?;
    entry
        .set_password(secret)
        .map_err(|e| AppError::Config(format!("保存{}到密钥链失败：{}", slot.describe(), e)))?;
    Ok(())
}

/// 读槽位里的机密；没存过返回 `Ok(None)`。
pub fn get(slot: &SecretSlot) -> Result<Option<String>, AppError> {
    let entry = store_impl::entry(SERVICE, &slot.account())
        .map_err(|e| AppError::Config(format!("密钥链初始化失败：{}", e)))?;
    store_impl::get_optional_password(&entry)
        .map_err(|e| AppError::Config(format!("读取{}失败：{}", slot.describe(), e)))
}

/// 删除槽位里的机密；本来就没有视为成功。
pub fn delete(slot: &SecretSlot) -> Result<(), AppError> {
    let entry = store_impl::entry(SERVICE, &slot.account())
        .map_err(|e| AppError::Config(format!("密钥链初始化失败：{}", e)))?;
    store_impl::delete_if_exists(&entry)
        .map_err(|e| AppError::Config(format!("删除{}失败：{}", slot.describe(), e)))
}

/// 槽位里有没有机密（只回答有无，不把值交给调用方）。
///
/// 读密钥链出错会原样返回 `Err`——「有没有」和「读不读得了」是两件事：
/// 要把出错当作「没有」的地方（设置页 / 启动快照的 `hasXxx` 标记）自己 `unwrap_or(false)`，
/// 要把错误弹给用户的地方（`config_has_password` 等命令）直接 `?`。
pub fn has(slot: &SecretSlot) -> Result<bool, AppError> {
    get(slot).map(|secret| secret.is_some())
}

/// 前端契约里 `hasXxx` 标记所对应的固定槽位是否有值（字段名与语义不变，见
/// `SettingsResponse` / `AppBootstrapData`）。
///
/// 一次成组的同步系统调用（每个槽位一次密钥链读取）：异步上下文里最好放进
/// `spawn_blocking`。读失败一律按「没有」算——与改造前 `.ok().flatten().is_some()` 一致。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FeatureKeyFlags {
    /// 旧版单配置 Key 是否还在（`SecretSlot::LegacyLlmApiKey`）。
    pub legacy_llm_api_key: bool,
    pub web_search_api_key: bool,
    pub deepseek_tools_api_key: bool,
    pub custom_search_api_key: bool,
    pub custom_fetch_api_key: bool,
    pub tavily_fetch_api_key: bool,
    pub jev_api_key: bool,
}

impl FeatureKeyFlags {
    pub fn probe() -> Self {
        let present = |slot: SecretSlot| has(&slot).unwrap_or(false);
        FeatureKeyFlags {
            legacy_llm_api_key: present(SecretSlot::LegacyLlmApiKey),
            web_search_api_key: present(SecretSlot::WebSearchApiKey),
            deepseek_tools_api_key: present(SecretSlot::DeepseekToolsApiKey),
            custom_search_api_key: present(SecretSlot::CustomSearchApiKey),
            custom_fetch_api_key: present(SecretSlot::CustomFetchApiKey),
            tavily_fetch_api_key: present(SecretSlot::TavilyFetchApiKey),
            jev_api_key: present(SecretSlot::JevApiKey),
        }
    }
}

// 这里的测试**只测纯函数**（账号名 / 分档 / 唯一性），绝不调用 `get/set/delete/has`：
// 以前出过单元测试误删本机系统密钥链里真实条目的事故。
#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    /// 每个变体的样本：不带参数的取 `fixed_slots()`，带参数的各给一个。
    /// 新增变体后上面 `account()` 等 `match` 先编译不过，补完回到这里再补样本与快照。
    fn all_samples() -> Vec<SecretSlot> {
        let mut v = SecretSlot::fixed_slots();
        v.push(SecretSlot::LlmChannel("c1".into()));
        v.push(SecretSlot::SshPassword("c1".into()));
        v.push(SecretSlot::SshKeyPassphrase("c1".into()));
        v.push(SecretSlot::JumpPassword("c1".into()));
        v.push(SecretSlot::JumpKeyPassphrase("c1".into()));
        v
    }

    /// 改造前的账号名快照。**这张表是老用户数据的护栏**：要让它变红的改动，
    /// 等于让已有用户的 Key / 密码全部找不到。
    #[test]
    fn account_names_match_pre_slot_table_snapshot() {
        // 全部写死成字面量（不用 format! 复刻实现），这样改了实现里的格式串这里必红
        let snapshot: Vec<(SecretSlot, &str)> = vec![
            (SecretSlot::LegacyLlmApiKey, "llm_api_key"),
            (SecretSlot::LlmChannel("c1".into()), "llm_channel_c1"),
            (SecretSlot::WebSearchApiKey, "web_search_api_key"),
            (SecretSlot::DeepseekToolsApiKey, "deepseek_tools_api_key"),
            (SecretSlot::CustomSearchApiKey, "custom_search_api_key"),
            (SecretSlot::CustomFetchApiKey, "custom_fetch_api_key"),
            (SecretSlot::TavilyFetchApiKey, "tavily_fetch_api_key"),
            (SecretSlot::JevApiKey, "jev_api_key"),
            // SSH 连接密码：账号就是连接 id 本身，没有前缀
            (SecretSlot::SshPassword("c1".into()), "c1"),
            (SecretSlot::SshKeyPassphrase("c1".into()), "pk:c1"),
            (SecretSlot::JumpPassword("c1".into()), "jump:c1"),
            (SecretSlot::JumpKeyPassphrase("c1".into()), "jump:pk:c1"),
            (SecretSlot::KeyStoreMaster, "keys:master"),
        ];
        for (slot, expected) in &snapshot {
            assert_eq!(slot.account(), *expected, "槽位 {:?} 的账号名变了", slot);
        }
        // 真实 id（UUID）下格式也一样：前缀 / 拼接位置不因 id 内容而变
        let uuid = "7b1f0c9e-3a52-4d6e-9a41-2c8d5e0f6a13";
        assert_eq!(
            SecretSlot::LlmChannel(uuid.into()).account(),
            "llm_channel_7b1f0c9e-3a52-4d6e-9a41-2c8d5e0f6a13"
        );
        assert_eq!(SecretSlot::SshPassword(uuid.into()).account(), uuid);
        assert_eq!(
            SecretSlot::JumpKeyPassphrase(uuid.into()).account(),
            "jump:pk:7b1f0c9e-3a52-4d6e-9a41-2c8d5e0f6a13"
        );
        // 快照表本身要覆盖每个变体：样本里有而快照里没有就是漏钉
        assert_eq!(snapshot.len(), all_samples().len());
    }

    #[test]
    fn fixed_slot_accounts_are_unique() {
        let fixed = SecretSlot::fixed_slots();
        let accounts: HashSet<_> = fixed.iter().map(|s| s.account().into_owned()).collect();
        assert_eq!(accounts.len(), fixed.len(), "固定槽位的账号名有重复");
    }

    #[test]
    fn parameterized_slots_do_not_collide_for_the_same_id() {
        // 同一个 id 下的四种连接 / 渠道槽位，账号名必须各不相同
        let samples = all_samples();
        let accounts: HashSet<_> = samples.iter().map(|s| s.account().into_owned()).collect();
        assert_eq!(
            accounts.len(),
            samples.len(),
            "同 id 下不同槽位的账号名撞了"
        );
    }

    #[test]
    fn for_connection_lists_every_connection_bound_slot() {
        let slots = SecretSlot::for_connection("c1");
        // 密码 / 私钥口令 / 跳板机密码 / 跳板机私钥口令，账号名各不相同，且都在样本里
        let accounts: HashSet<_> = slots.iter().map(|s| s.account().into_owned()).collect();
        assert_eq!(accounts.len(), slots.len());
        let samples = all_samples();
        for slot in &slots {
            assert!(samples.contains(slot), "{:?} 不在样本表里", slot);
        }
        // 样本里「跟着连接走」的变体（SshPassword / SshKeyPassphrase / Jump*）一个不少
        let connection_bound = samples
            .iter()
            .filter(|s| {
                matches!(
                    s,
                    SecretSlot::SshPassword(_)
                        | SecretSlot::SshKeyPassphrase(_)
                        | SecretSlot::JumpPassword(_)
                        | SecretSlot::JumpKeyPassphrase(_)
                )
            })
            .count();
        assert_eq!(connection_bound, slots.len());
    }

    #[test]
    fn key_store_master_is_device_bound_local() {
        assert_eq!(SecretSlot::KeyStoreMaster.tier(), SyncTier::Local);
        assert_eq!(SecretSlot::KeyStoreMaster.account(), "keys:master");
    }

    #[test]
    fn every_other_slot_is_secret_tier() {
        for slot in all_samples() {
            if slot == SecretSlot::KeyStoreMaster {
                continue;
            }
            assert_eq!(slot.tier(), SyncTier::Secret, "{:?} 应为 Secret 档", slot);
        }
    }

    #[test]
    fn describe_is_non_empty_and_parameter_free() {
        for slot in all_samples() {
            let text = slot.describe();
            assert!(!text.trim().is_empty(), "{:?} 缺少说明", slot);
            // 说明会进错误信息：不能带参数（连接 id / 渠道 id），更不能带机密
            assert!(!text.contains("c1"), "{:?} 的说明不应含参数", slot);
        }
    }

    /// 穷举 `match`：新增变体时这里先编译不过，逼着人决定它属于「固定」还是「带参数」，
    /// 并确认固定的那类已进 `fixed_slots()`。
    #[test]
    fn fixed_slots_cover_every_unit_variant() {
        let fixed = SecretSlot::fixed_slots();
        for slot in all_samples() {
            let is_unit = match &slot {
                SecretSlot::LegacyLlmApiKey
                | SecretSlot::WebSearchApiKey
                | SecretSlot::DeepseekToolsApiKey
                | SecretSlot::CustomSearchApiKey
                | SecretSlot::CustomFetchApiKey
                | SecretSlot::TavilyFetchApiKey
                | SecretSlot::JevApiKey
                | SecretSlot::KeyStoreMaster => true,
                SecretSlot::LlmChannel(_)
                | SecretSlot::SshPassword(_)
                | SecretSlot::SshKeyPassphrase(_)
                | SecretSlot::JumpPassword(_)
                | SecretSlot::JumpKeyPassphrase(_) => false,
            };
            assert_eq!(
                fixed.contains(&slot),
                is_unit,
                "{:?} 与 fixed_slots() 的登记不一致",
                slot
            );
        }
    }
}
