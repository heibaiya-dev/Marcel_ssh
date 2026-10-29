use serde::Serialize;
use tauri::State;

use crate::config::connections::SavedConnection;
use crate::config::keychain;
use crate::config::settings::AppSettings;
use crate::error::AppError;
use crate::skills::store::Skill;
use crate::AppState;

/// 聚合启动快照数据包：在前端启动首帧一次性下发，消除多路 IPC 往返瀑布流。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppBootstrapData {
    /// 应用全局设置（LLM api_key 已按安全规则排除序列化）
    pub settings: AppSettings,
    /// 密钥链中是否已存储 LLM API Key（多渠道下 = 任一渠道有 key）
    pub has_api_key: bool,
    /// 密钥链中是否已存储 Web Search API Key
    pub has_web_search_api_key: bool,
    /// 密钥链中是否已存储 DeepSeek 工具类 API Key（搜索 + 网页获取共用）
    ///
    /// 与 `has_jev_api_key` 同理：启动快照是前端读到真值的唯一路径，漏发会让设置页
    /// 误报「还没配 Key」。
    pub has_deepseek_tools_api_key: bool,
    /// 密钥链中是否已存储「自定义搜索 API」的 Key（同样只有启动快照这一条读取路径）
    pub has_custom_search_api_key: bool,
    /// 密钥链中是否已存储「自定义网页获取」的 Key（同上，只有启动快照这一条读取路径）
    pub has_custom_fetch_api_key: bool,
    /// 密钥链中是否已存储「Tavily 网页获取」的 Key（同上，只有启动快照这一条读取路径）
    pub has_tavily_fetch_api_key: bool,
    /// 密钥链中是否已存储 TypeSafe（Jev）API Key
    ///
    /// 与 `has_web_search_api_key` 成对：前端只在启动快照这一条路上 hydrate 设置
    /// （随后 `settingsStore.load()` 因 `loaded` 已置真而短路），所以这里漏发就等于
    /// 整个会话都读不到真值——设置页会误报「还没配 Key」，并藏起清除按钮。
    pub has_jev_api_key: bool,
    /// 各渠道密钥是否存在（多渠道模型服务）
    #[serde(default)]
    pub channel_key_status: Vec<crate::commands::settings::ChannelKeyStatus>,
    /// 设置加载时的非致命告警（如旧配置损坏备份）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settings_warning: Option<String>,
    /// 保存的 SSH 连接列表
    pub connections: Vec<SavedConnection>,
    /// 用户与内置 Skill 列表
    pub skills: Vec<Skill>,
}

/// 获取聚合启动快照。
#[tauri::command]
pub async fn app_get_bootstrap(state: State<'_, AppState>) -> Result<AppBootstrapData, AppError> {
    let settings = state.settings.read().await.clone();

    // 密钥链探测是一次成组的同步系统调用（每渠道一条 + 各功能 Key 若干条），
    // 整段挪进 spawn_blocking：别在异步运行时上逐条阻塞。渠道维度只探一次
    // ——`has_api_key` 复用 `channel_key_status` 的结果（旧实现同一段路读两遍）。
    let (channel_key_status, key_flags) = {
        let settings = settings.clone();
        tokio::task::spawn_blocking(move || {
            let channel_key_status =
                crate::commands::settings::compute_channel_key_status(&settings);
            // 各功能 Key 的有无由槽位表统一探测（`FeatureKeyFlags`），这里只负责把它们
            // 映射成前端契约里的字段；`has_api_key` 是「任一渠道有 Key，或旧版单配置 Key 还在」。
            let flags = keychain::FeatureKeyFlags::probe();
            let has_api_key =
                channel_key_status.iter().any(|s| s.has_key) || flags.legacy_llm_api_key;
            (channel_key_status, (has_api_key, flags))
        })
        .await
        .map_err(|e| AppError::Other(format!("密钥链探测任务失败: {}", e)))?
    };
    let (has_api_key, flags) = key_flags;
    let has_web_search_api_key = flags.web_search_api_key;
    let has_deepseek_tools_api_key = flags.deepseek_tools_api_key;
    let has_custom_search_api_key = flags.custom_search_api_key;
    let has_custom_fetch_api_key = flags.custom_fetch_api_key;
    let has_tavily_fetch_api_key = flags.tavily_fetch_api_key;
    let has_jev_api_key = flags.jev_api_key;
    let settings_warning = state.settings_warning.write().take();

    let connections = state.connection_store.read().await.get_all().to_vec();
    let skills = state.skill_store.read().await.list().to_vec();

    Ok(AppBootstrapData {
        settings,
        has_api_key,
        has_web_search_api_key,
        has_deepseek_tools_api_key,
        has_custom_search_api_key,
        has_custom_fetch_api_key,
        has_tavily_fetch_api_key,
        has_jev_api_key,
        channel_key_status,
        settings_warning,
        connections,
        skills,
    })
}
