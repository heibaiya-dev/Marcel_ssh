use std::time::Duration;

use serde::Serialize;
use tauri_plugin_shell::ShellExt;

use crate::error::AppError;

const DEFAULT_RELEASE_URL: &str = "https://github.com/q541810/Marcel_ssh/releases";

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateCheckResult {
    pub has_update: bool,
    pub latest_version: String,
    pub release_url: String,
    /// 本平台安装包直链（Windows = NSIS exe，Android = APK）。缺失时前端
    /// 降级为跳浏览器手动下载。
    pub installer_url: Option<String>,
    /// minisign 签名（`tauri signer sign` 输出的 base64 文本）。仅 Windows 有值。
    pub signature: Option<String>,
    /// 安装包 sha256（hex，小写）。
    pub sha256: Option<String>,
    /// 安装包字节数。
    pub size: Option<u64>,
}

/// 运行平台的安装包类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum InstallKind {
    /// Windows：NSIS 安装包，应用退出后静默安装。
    Exe,
    /// Android：APK，后台下载完成后一键拉起系统安装器（安装需用户在系统
    /// 界面确认，且系统会强制校验签名与已装版本一致）。
    Apk,
    /// 其他桌面平台（macOS/Linux）：只检查并提示，跳浏览器手动下载。
    None,
}

/// 当前平台的更新能力。前端据此决定是否展示「自动下载并安装」开关与
/// 「后台下载」按钮 —— 不能让用户看到一个点了必然失败的入口。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateCapabilities {
    pub install_kind: InstallKind,
    /// 是否支持「后台下载 + 就绪安装」。
    pub silent_download: bool,
}

#[cfg(windows)]
pub const fn install_kind() -> InstallKind {
    InstallKind::Exe
}

#[cfg(target_os = "android")]
pub const fn install_kind() -> InstallKind {
    InstallKind::Apk
}

#[cfg(not(any(windows, target_os = "android")))]
pub const fn install_kind() -> InstallKind {
    InstallKind::None
}

/// latest.json 中与安装包有关的扩展字段。旧 latest.json 没有这些字段 ——
/// 全部 Option，缺失时调用方降级为「跳浏览器」行为，绝不清空或报错。
#[derive(Debug, Clone, Default)]
pub struct ReleaseAssets {
    /// 安装包直链（桌面 = NSIS exe，Android = APK）。
    pub installer_url: Option<String>,
    /// 安装包镜像直链（国内网络 fallback 用；下载从 installer_url 开始逐个
    /// 尝试，内容由 sha256（+ Windows 签名）校验兜底，镜像无需被信任）。
    pub installer_mirrors: Vec<String>,
    /// minisign 签名 base64（`tauri signer sign` 输出）。仅 Windows 使用。
    pub signature: Option<String>,
    /// 安装包 sha256（hex）。
    pub sha256: Option<String>,
    /// 安装包字节数。
    pub size: Option<u64>,
}

/// latest.json 解析结果（已按运行平台选好对象）。
#[derive(Debug, Clone)]
pub struct LatestRelease {
    pub version: String,
    pub release_url: String,
    pub assets: ReleaseAssets,
}

impl LatestRelease {
    /// 本平台静默下载所需的字段是否齐全。
    pub fn download_ready(&self) -> bool {
        self.download_ready_for(install_kind())
    }

    /// 按指定平台判断（拆出来是为了两条分支都能被单测覆盖）：
    /// - Windows：直链 + 大小 + sha256 + minisign 签名，四项齐全才敢静默执行；
    /// - Android：直链 + 大小 + sha256。APK 的来源可信由系统安装器的签名校验
    ///   兜底（签名不符无法覆盖安装），sha256 负责传输完整性；
    /// - 其他平台：不支持后台下载。
    pub fn download_ready_for(&self, kind: InstallKind) -> bool {
        let base = self.assets.installer_url.is_some()
            && self.assets.sha256.is_some()
            && self.assets.size.is_some();
        match kind {
            InstallKind::Exe => base && self.assets.signature.is_some(),
            InstallKind::Apk => base,
            InstallKind::None => false,
        }
    }
}

/// Pick the latest version info for the running platform from `latest.json`.
///
/// Desktop reads the top-level `version` / `release_url`. Android reads the
/// nested `android` object; when the field is missing (old `latest.json`
/// without platform separation) it falls back to the top level, which is
/// equivalent since historic releases shipped both platforms together.
fn pick_latest(latest: &serde_json::Value, is_android: bool) -> (String, String) {
    let read = |v: &serde_json::Value, key: &str, default: &str| -> String {
        v.get(key)
            .and_then(|x| x.as_str())
            .unwrap_or(default)
            .to_string()
    };

    let target = if is_android {
        latest
            .get("android")
            .filter(|v| v.is_object())
            .unwrap_or(latest)
    } else {
        latest
    };

    (
        read(target, "version", ""),
        read(target, "release_url", DEFAULT_RELEASE_URL),
    )
}

/// 资产字段的来源对象：**严格按平台**。
///
/// 桌面 = 顶层；Android = `android` 对象，且**不回退顶层** —— 顶层的
/// `installer_url`/`sha256`/`size` 描述的是桌面 NSIS 包，回退会让安卓客户端
/// 拿到一个装不上的 exe（版本号还会和它自相矛盾）。平台分离前的旧
/// latest.json 里 `android` 对象不含资产字段 → 返回 None → 降级为跳浏览器，
/// 与老客户端行为一致。
fn asset_target(latest: &serde_json::Value, is_android: bool) -> Option<&serde_json::Value> {
    if is_android {
        latest.get("android").filter(|v| v.is_object())
    } else {
        Some(latest)
    }
}

/// 读取某个对象内的安装包资产字段（缺失/空串一律视为缺失）。
fn pick_assets(target: &serde_json::Value) -> ReleaseAssets {
    let read_str = |key: &str| -> Option<String> {
        target
            .get(key)
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
    };
    let mirrors = target
        .get("installer_mirrors")
        .and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default();
    ReleaseAssets {
        installer_url: read_str("installer_url"),
        installer_mirrors: mirrors,
        signature: read_str("signature"),
        sha256: read_str("sha256"),
        size: target.get("size").and_then(|x| x.as_u64()),
    }
}

/// 从一份已解析的 latest.json 拼出本平台的发布信息：版本对象 + 按平台取资产。
///
/// 每份检查源的结果都经它拼装，保证「版本号从哪个对象读」与「资产字段从哪个对象读」
/// 永远是同一份口径 —— 分开拼装时最容易出的错，就是版本号读了 `android` 而资产字段
/// 回退到了顶层（安卓会因此拿到一个装不上的 exe）。
fn release_from(latest: &serde_json::Value, is_android: bool) -> LatestRelease {
    let (version, release_url) = pick_latest(latest, is_android);
    let assets = asset_target(latest, is_android)
        .map(pick_assets)
        .unwrap_or_default();
    LatestRelease {
        version,
        release_url,
        assets,
    }
}

/// Open a URL in the system browser.
///
/// Uses `ShellExt::open` so Android hits the shell plugin Intent path
/// (`ACTION_VIEW`). The JS `plugin:shell|open` command always goes through
/// the desktop `open` crate (xdg-open), which is a no-op on Android.
#[tauri::command]
pub async fn open_external_url(app: tauri::AppHandle, url: String) -> Result<(), AppError> {
    let trimmed = url.trim();
    if !(trimmed.starts_with("https://") || trimmed.starts_with("http://")) {
        return Err(AppError::Other(format!("不允许打开的链接: {}", trimmed)));
    }
    #[allow(deprecated)]
    app.shell()
        .open(trimmed, None)
        .map_err(|e| AppError::Other(format!("打开浏览器失败: {}", e)))
}

/// 检查源列表：GitHub raw 为主，jsDelivr 各入口为国内网络 fallback。
/// **并发拉取全部源、取版本最高者**（不是「第一个成功者生效」，原因见
/// `check_for_update`）。
///
/// **为什么列多个 jsDelivr 域名**：jsDelivr 是一组指向不同边缘节点的入口域名
/// —— `cdn` 走 Cloudflare、`fastly` 走 Fastly、`gcore` 走 GCore。它们返回同一份
/// 缓存内容，但**在部分国内网络上只有个别入口通**。实测（2026-10-11，国内线路）：
/// `cdn` 与 `raw.githubusercontent` 双双超时，`fastly` 1.4s、`gcore` 0.6s 正常。
/// 只挂一个 jsDelivr 入口等于「有兜底但兜底本身也会挂」。
/// （jsDelivr 官方文档声明根端点是 `cdn.jsdelivr.net`，另几个域名未列入文档，
/// 但实测可用；作为 extra 层放在主端点之后，主端点恢复时不会受影响。）
///
/// **为什么用 `@main` 而不是 tag**：jsDelivr 对**分支引用缓存 12 小时且 purge
/// 无效**（官方 purge API 只对 semver 版本标签生效）。发版后 latest.json 的
/// 改动要等最多 12 小时才被用户看到 —— 实测发 v1.7.1 时 `@main` 仍返回 12
/// 小时前的旧内容（无镜像字段），而 `@v1.7.1` 立刻是新内容。raw 主源不走
/// CDN、无缓存，所以覆盖快的是它；jsDelivr 作为兜底，晚几个小时拿到新版本
/// 可以接受。**注意各入口的缓存互相独立**：同一时刻可能有的已是新版、有的还是
/// 12 小时前的旧版（实测 2026-10-18 发 v1.8.0 时 `fastly`/`gcore` 已更新而
/// `cdn`/`testingcf` 仍是 v1.7.2）——所以必须并发取最高版本，不能「第一个成功
/// 者生效」，否则只要第一个入口是旧缓存，后面的新内容就永远轮不到。
pub const CHECK_URLS: &[&str] = &[
    "https://raw.githubusercontent.com/q541810/Marcel_ssh/main/latest.json",
    "https://cdn.jsdelivr.net/gh/q541810/Marcel_ssh@main/latest.json",
    "https://fastly.jsdelivr.net/gh/q541810/Marcel_ssh@main/latest.json",
    "https://gcore.jsdelivr.net/gh/q541810/Marcel_ssh@main/latest.json",
    "https://testingcf.jsdelivr.net/gh/q541810/Marcel_ssh@main/latest.json",
];

/// 拉取并解析 latest.json（command 与后台调度器共用入口）。
///
/// **并发拉取全部检查源，取版本最高的那份。** 检查源之间不是「同一份内容的多个
/// 副本」：jsDelivr 的分支缓存由各个边缘节点独立维护，同一时刻可能有的已是新版、
/// 有的还是 12 小时前的旧版。所以只能全拉回来再比版本 —— 逐源尝试、第一个成功者
/// 生效的写法，在国内线路上（raw 常不可达）会停在第一个 jsDelivr 入口的旧缓存上
/// 直接返回旧版本，后面明明有新版的入口根本轮不到，用户得等到该边缘节点缓存过期。
///
/// 取「版本最高」即真值：旧源只可能报出**更旧**的版本（`latest.json` 是我们自己
/// 仓库里的文件，不存在「某些源报出更新的版本」这种需要分辨真伪的情况）。就算镜像
/// 被完全接管也换不来一次安装：桌面包另有 sha256 + minisign 验签、安卓包由系统
/// 安装器强制校验签名，镜像与实际内容从来不需要被信任 —— 同
/// `commands/market.rs` 的 `market_list`。
pub async fn check_for_update() -> Result<LatestRelease, AppError> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(15))
        .user_agent(concat!("Marcel-SSH/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| AppError::Update(format!("无法创建 HTTP 客户端: {}", e)))?;

    // 测试钩子：本地模拟更新源时用环境变量覆盖检查地址，配合
    // `scripts/update-test-source.mjs` 在开发机上跑通整条更新链路。
    //
    // 只在 debug 构建里编译进去：正式版不留任何「替换更新源」的入口 ——
    // 环境变量的值不设限（任何 http(s) 地址），能改本机环境变量的程序就能借它
    // 把客户端指到伪造的 latest.json（Windows 侧安装包仍有 minisign 验签兜底、
    // 安卓侧系统安装器强制校验签名，但没必要留着这个面）。测试请用
    // `pnpm tauri dev` / `pnpm tauri android dev`。
    #[cfg(debug_assertions)]
    let check_urls: Vec<String> = match std::env::var("MARCEL_LATEST_JSON_URL") {
        Ok(url) if url.starts_with("http://") || url.starts_with("https://") => vec![url],
        _ => CHECK_URLS.iter().map(|s| s.to_string()).collect(),
    };
    #[cfg(not(debug_assertions))]
    let check_urls: Vec<String> = CHECK_URLS.iter().map(|s| s.to_string()).collect();

    let is_android = cfg!(target_os = "android");

    // 并发拉全部源，单个源失败不影响其它源（`join_all` 保留输入顺序，
    // 所以「最后错误」仍是确定的那一条，不随完成先后抖动）。
    let fetches = check_urls.iter().map(|url| {
        let client = client.clone();
        async move {
            let resp = client.get(url).send().await.map_err(|e| {
                log::warn!("更新检查源不可达 {}: {}", url, e);
                format!("{} {}", url, e)
            })?;
            if !resp.status().is_success() {
                log::warn!("更新检查源返回 {} {}", url, resp.status());
                return Err(format!("{} 返回 {}", url, resp.status()));
            }
            let latest: serde_json::Value = resp.json().await.map_err(|e| {
                log::warn!("更新检查源响应解析失败 {}: {}", url, e);
                format!("{} 解析失败 {}", url, e)
            })?;
            Ok(release_from(&latest, is_android))
        }
    });

    let (best, last_err) = select_freshest(futures::future::join_all(fetches).await);
    if let Some(best) = best {
        return Ok(best);
    }

    // 全部源都失败：报「试了几个 + 最后一个原因」，让用户知道这不是单源故障，
    // 也别把某个域名当成唯一依赖（它随时可能被网络掐掉）。
    let detail = last_err.unwrap_or_else(|| "原因不明".into());
    Err(AppError::Update(format!(
        "无法检查更新：已尝试 {} 个更新源均不可达（最后错误：{}）。请检查网络连接后重试",
        check_urls.len(),
        detail
    )))
}

/// 从并发拉回的候选里挑出**版本最高**的那份，并带回「最后一个失败原因」
/// （全部失败时用它给用户报，见 `check_for_update`）。
///
/// 版本号解析不出的候选（不该出现，比如源被替换成别的内容）：只在还没拿到任何
/// 结果时暂时采用，之后任何可解析版本都能顶掉它 —— 与 `has_update` 的宽容口径
/// 一致，不让一个畸形响应把正常结果挤掉；若所有候选都解析不出，行为退化成
/// 「第一个成功者生效」，等价于旧逻辑。
fn select_freshest(
    outcomes: Vec<Result<LatestRelease, String>>,
) -> (Option<LatestRelease>, Option<String>) {
    let mut best: Option<LatestRelease> = None;
    let mut best_version: Option<semver::Version> = None;
    let mut last_err: Option<String> = None;
    for outcome in outcomes {
        match outcome {
            Ok(release) => {
                let parsed = semver::Version::parse(&release.version).ok();
                let take = match (&best_version, &parsed) {
                    (Some(cur), Some(new)) => new > cur,
                    (None, Some(_)) => true,
                    (_, None) => best.is_none(),
                };
                if take {
                    best_version = parsed;
                    best = Some(release);
                }
            }
            Err(e) => last_err = Some(e),
        }
    }
    (best, last_err)
}

fn has_update(current_version: &str, latest_version: &str) -> bool {
    let cur = semver::Version::parse(current_version).ok();
    let lat = semver::Version::parse(latest_version).ok();
    match (cur, lat) {
        (Some(c), Some(l)) => l > c,
        _ => false,
    }
}

#[tauri::command]
pub async fn check_update(app: tauri::AppHandle) -> Result<UpdateCheckResult, AppError> {
    let current_version = app.package_info().version.to_string();
    let latest = check_for_update().await?;
    Ok(UpdateCheckResult {
        has_update: has_update(&current_version, &latest.version),
        latest_version: latest.version,
        release_url: latest.release_url,
        installer_url: latest.assets.installer_url,
        signature: latest.assets.signature,
        sha256: latest.assets.sha256,
        size: latest.assets.size,
    })
}

// ── 无感更新 command（跨平台） ───────────────────────────────────
// 状态机与下载器在 `crate::updater`（全部平台编译）：Windows 装 NSIS、
// Android 装 APK、其他桌面平台只检查不下载。平台差异由 updater 内部与
// `update_capabilities` 表达，前端 API 与文案分支保持两端一致。

/// 当前平台支持的更新能力（UI 据此决定是否展示自动更新开关与后台下载按钮）。
#[tauri::command]
pub async fn update_capabilities() -> Result<UpdateCapabilities, AppError> {
    let kind = install_kind();
    Ok(UpdateCapabilities {
        install_kind: kind,
        silent_download: kind != InstallKind::None,
    })
}

/// 当前更新器状态快照（前端挂载时兜底拉取，防丢事件）。
#[tauri::command]
pub async fn get_update_state(app: tauri::AppHandle) -> Result<serde_json::Value, AppError> {
    let state = crate::updater::get_update_state_impl(&app).await?;
    serde_json::to_value(state).map_err(|e| AppError::Other(format!("序列化更新状态失败: {}", e)))
}

/// 手动触发后台下载（设置页/药丸「立即下载」调用；手机端会绕过流量限制）。
#[tauri::command]
pub async fn start_update_download(app: tauri::AppHandle) -> Result<(), AppError> {
    crate::updater::start_update_download_impl(&app).await
}

/// 立即安装已就绪的更新：Windows 退出并静默安装后自动重启；Android 拉起
/// 系统安装器（系统界面确认，装完由系统重启应用）。
#[tauri::command]
pub async fn install_update_now(app: tauri::AppHandle) -> Result<(), AppError> {
    crate::updater::install_update_now_impl(&app).await
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn desktop_reads_top_level() {
        let latest = json!({
            "version": "0.8.1",
            "release_url": "https://github.com/q541810/Marcel_ssh/releases/tag/v0.8.1",
            "android": {
                "version": "0.8.0",
                "release_url": "https://github.com/q541810/Marcel_ssh/releases/tag/v0.8.0"
            }
        });
        let (v, url) = pick_latest(&latest, false);
        assert_eq!(v, "0.8.1");
        assert_eq!(
            url,
            "https://github.com/q541810/Marcel_ssh/releases/tag/v0.8.1"
        );
    }

    #[test]
    fn android_reads_android_field() {
        let latest = json!({
            "version": "0.8.1",
            "release_url": "https://github.com/q541810/Marcel_ssh/releases/tag/v0.8.1",
            "android": {
                "version": "0.8.0",
                "release_url": "https://github.com/q541810/Marcel_ssh/releases/tag/v0.8.0"
            }
        });
        let (v, url) = pick_latest(&latest, true);
        assert_eq!(v, "0.8.0");
        assert_eq!(
            url,
            "https://github.com/q541810/Marcel_ssh/releases/tag/v0.8.0"
        );
    }

    #[test]
    fn android_falls_back_to_top_level_when_android_field_missing() {
        let latest = json!({
            "version": "0.8.0",
            "release_url": "https://github.com/q541810/Marcel_ssh/releases/tag/v0.8.0"
        });
        let (v, url) = pick_latest(&latest, true);
        assert_eq!(v, "0.8.0");
        assert_eq!(
            url,
            "https://github.com/q541810/Marcel_ssh/releases/tag/v0.8.0"
        );
    }

    #[test]
    fn android_falls_back_to_top_level_when_android_field_not_object() {
        let latest = json!({
            "version": "0.8.0",
            "release_url": "https://github.com/q541810/Marcel_ssh/releases/tag/v0.8.0",
            "android": "0.8.0"
        });
        let (v, _url) = pick_latest(&latest, true);
        assert_eq!(v, "0.8.0");
    }

    #[test]
    fn missing_fields_use_defaults() {
        let latest = json!({});
        let (v, url) = pick_latest(&latest, false);
        assert_eq!(v, "");
        assert_eq!(url, DEFAULT_RELEASE_URL);
        let (v, url) = pick_latest(&latest, true);
        assert_eq!(v, "");
        assert_eq!(url, DEFAULT_RELEASE_URL);
    }

    /// 桌面：顶层扩展字段齐全 → 四项全解析出来，且具备静默下载条件。
    #[test]
    fn desktop_assets_all_present() {
        let latest = json!({
            "version": "1.5.0",
            "release_url": "https://example.com/tag/v1.5.0",
            "installer_url": "https://example.com/download/Marcel+SSH_1.5.0_x64-setup.exe",
            "signature": "untrusted comment: sig\nABC=\n",
            "sha256": "abc123",
            "size": 12345
        });
        let release = release_from(&latest, false);
        assert_eq!(
            release.assets.installer_url.as_deref(),
            Some("https://example.com/download/Marcel+SSH_1.5.0_x64-setup.exe")
        );
        assert_eq!(
            release.assets.signature.as_deref(),
            Some("untrusted comment: sig\nABC=\n")
        );
        assert_eq!(release.assets.sha256.as_deref(), Some("abc123"));
        assert_eq!(release.assets.size, Some(12345));
        assert!(release.download_ready_for(InstallKind::Exe));
    }

    /// 旧 latest.json（无扩展字段）→ 全 None、两端都不具备下载条件，
    /// 调用方据此降级为跳浏览器，不报错。
    #[test]
    fn assets_missing_in_old_format() {
        let latest = json!({
            "version": "1.5.0",
            "release_url": "https://example.com/tag/v1.5.0"
        });
        for is_android in [false, true] {
            let release = release_from(&latest, is_android);
            assert!(release.assets.installer_url.is_none());
            assert!(release.assets.signature.is_none());
            assert!(release.assets.sha256.is_none());
            assert!(release.assets.size.is_none());
            assert!(!release.download_ready_for(InstallKind::Exe));
            assert!(!release.download_ready_for(InstallKind::Apk));
        }
    }

    /// 空字符串字段视为缺失（发布侧误写空值时降级而非下载空 URL）。
    #[test]
    fn assets_empty_string_treated_as_missing() {
        let latest = json!({
            "installer_url": "",
            "signature": "",
            "sha256": "",
            "size": 1
        });
        let release = release_from(&latest, false);
        assert!(release.assets.installer_url.is_none());
        assert!(release.assets.signature.is_none());
        assert!(release.assets.sha256.is_none());
    }

    /// 平台差异：Windows 必须四字段齐全（含签名）才敢静默执行；
    /// Android 依赖系统安装器校验 APK 签名，只需直链 + 大小 + sha256。
    #[test]
    fn download_ready_platform_rules() {
        let with_sig = json!({
            "installer_url": "https://example.com/x.exe",
            "signature": "sig",
            "sha256": "hash",
            "size": 5
        });
        let release = release_from(&with_sig, false);
        assert!(release.download_ready_for(InstallKind::Exe));
        assert!(release.download_ready_for(InstallKind::Apk));

        // 缺签名：Windows 不可静默，Android 仍可
        let no_sig = json!({
            "installer_url": "https://example.com/x.apk",
            "sha256": "hash",
            "size": 5
        });
        let release = release_from(&no_sig, false);
        assert!(!release.download_ready_for(InstallKind::Exe));
        assert!(release.download_ready_for(InstallKind::Apk));

        // 缺 sha256：两端都不可（完整性校验是硬门槛）
        let no_hash = json!({
            "installer_url": "https://example.com/x.apk",
            "signature": "sig",
            "size": 5
        });
        let release = release_from(&no_hash, false);
        assert!(!release.download_ready_for(InstallKind::Exe));
        assert!(!release.download_ready_for(InstallKind::Apk));

        // 不支持后台下载的平台永远不可
        let full = release_from(&with_sig, false);
        assert!(!full.download_ready_for(InstallKind::None));
    }

    /// 回归：Android **绝不**继承顶层的桌面安装包字段。
    /// 只发桌面不发安卓时，顶层是 NSIS exe（版本 1.5.0），`android` 对象仍是
    /// 旧版本且无资产字段 —— 安卓必须只看到「无更新」，不能拿到 exe 直链。
    #[test]
    fn android_assets_never_inherit_desktop_package() {
        let latest = json!({
            "version": "1.5.0",
            "release_url": "https://example.com/tag/v1.5.0",
            "installer_url": "https://example.com/Marcel+SSH_1.5.0_x64-setup.exe",
            "signature": "desktop-sig",
            "sha256": "desktop-hash",
            "size": 15467892,
            "android": {
                "version": "1.4.0",
                "release_url": "https://example.com/tag/v1.4.0"
            }
        });
        let release = release_from(&latest, true);
        assert_eq!(release.version, "1.4.0");
        assert!(release.assets.installer_url.is_none());
        assert!(release.assets.sha256.is_none());
        assert!(release.assets.signature.is_none());
        assert!(!release.download_ready_for(InstallKind::Apk));
    }

    /// 安卓资产字段写在 `android` 对象内 → 正常解析，且不带 minisign 签名。
    #[test]
    fn android_assets_read_from_android_object() {
        let latest = json!({
            "version": "1.5.0",
            "release_url": "https://example.com/tag/v1.5.0",
            "installer_url": "https://example.com/Marcel+SSH_1.5.0_x64-setup.exe",
            "sha256": "desktop-hash",
            "size": 15467892,
            "android": {
                "version": "1.5.0",
                "release_url": "https://example.com/tag/v1.5.0",
                "installer_url": "https://example.com/Marcel-SSH_1.5.0_universal-release.apk",
                "sha256": "apk-hash",
                "size": 45678901,
                "installer_mirrors": ["https://mirror.example/app.apk"]
            }
        });
        let release = release_from(&latest, true);
        assert_eq!(release.version, "1.5.0");
        assert_eq!(
            release.assets.installer_url.as_deref(),
            Some("https://example.com/Marcel-SSH_1.5.0_universal-release.apk")
        );
        assert_eq!(release.assets.sha256.as_deref(), Some("apk-hash"));
        assert_eq!(release.assets.size, Some(45678901));
        assert_eq!(release.assets.installer_mirrors.len(), 1);
        assert!(release.assets.signature.is_none());
        assert!(release.download_ready_for(InstallKind::Apk));
    }

    /// 桌面只读顶层：即使 `android` 对象里写了资产字段也不受影响。
    #[test]
    fn desktop_assets_ignore_android_object() {
        let latest = json!({
            "version": "1.5.0",
            "release_url": "https://example.com/tag/v1.5.0",
            "installer_url": "https://example.com/desktop.exe",
            "sha256": "desktop-hash",
            "size": 1,
            "signature": "desktop-sig",
            "android": {
                "version": "1.5.0",
                "installer_url": "https://example.com/android.apk",
                "sha256": "apk-hash",
                "size": 2
            }
        });
        let release = release_from(&latest, false);
        assert_eq!(
            release.assets.installer_url.as_deref(),
            Some("https://example.com/desktop.exe")
        );
        assert_eq!(release.assets.sha256.as_deref(), Some("desktop-hash"));
    }

    /// installer_mirrors 数组解析：合法 URL 保留，空串过滤，缺失/非数组 → 空。
    #[test]
    fn assets_mirrors_parsed() {
        let latest = json!({
            "installer_url": "https://github.com/x.exe",
            "installer_mirrors": ["https://mirror1.example/x.exe", "", "https://mirror2.example/x.exe"],
        });
        let release = release_from(&latest, false);
        assert_eq!(release.assets.installer_mirrors.len(), 2);
        assert_eq!(
            release.assets.installer_mirrors[0],
            "https://mirror1.example/x.exe"
        );
        assert_eq!(
            release.assets.installer_mirrors[1],
            "https://mirror2.example/x.exe"
        );

        assert!(pick_assets(&json!({})).installer_mirrors.is_empty());
        assert!(pick_assets(&json!({ "installer_mirrors": "bad" }))
            .installer_mirrors
            .is_empty());
    }

    /// 平台资产对象选择：Android 无 `android` 对象时返回 None（不回退顶层）。
    #[test]
    fn asset_target_is_strict_per_platform() {
        let latest = json!({ "installer_url": "https://example.com/x.exe" });
        assert!(asset_target(&latest, false).is_some());
        assert!(asset_target(&latest, true).is_none());
        // 非对象的 android 字段同样视为缺失
        let latest = json!({ "android": "1.5.0" });
        assert!(asset_target(&latest, true).is_none());
    }

    #[test]
    fn semver_update_detection() {
        assert!(has_update("1.4.0", "1.5.0"));
        assert!(has_update("1.4.0", "2.0.0"));
        assert!(!has_update("1.5.0", "1.4.0"));
        assert!(!has_update("1.4.0", "1.4.0"));
        // 非标准版本号一律视为无更新（与旧客户端行为一致）。
        assert!(!has_update("1.4.0", ""));
        assert!(!has_update("dev", "1.5.0"));
    }

    /// 检查源不能退回「一个 jsDelivr 入口」。
    ///
    /// 真实故障（2026-10-11，v1.7.0 发版当天撞上）：国内线路上
    /// `raw.githubusercontent` 与 `cdn.jsdelivr.net` 双双超时，用户点「检查更新」
    /// 直接报错——兜底源自己也在那台被掐的线路上，等于没有兜底。同一网络下
    /// `fastly` / `gcore` 入口却正常。只留单入口的回归没人会在 CI 里发现，
    /// 只能等真实用户再撞一次，所以在这里钉死。
    #[test]
    fn check_urls_cover_multiple_jsdelivr_edges() {
        assert!(
            CHECK_URLS.len() >= 4,
            "检查源过少（{} 个），国内网络很容易全挂",
            CHECK_URLS.len()
        );
        for edge in [
            "cdn.jsdelivr.net",
            "fastly.jsdelivr.net",
            "gcore.jsdelivr.net",
        ] {
            assert!(
                CHECK_URLS.iter().any(|u| u.contains(edge)),
                "缺少 jsDelivr 入口 {}：它和别的入口走不同边缘节点，缺一个国内就可能全线报错",
                edge
            );
        }
        assert!(
            CHECK_URLS
                .iter()
                .any(|u| u.contains("raw.githubusercontent.com")),
            "GitHub raw 主源必须保留"
        );
        // 全部必须是 https，且指向本项目的 latest.json（raw 用 `/main/`，
        // jsDelivr 用 `@main` 两种合法写法）。
        for url in CHECK_URLS {
            assert!(url.starts_with("https://"), "检查源必须 https: {}", url);
            assert!(
                url.ends_with("/latest.json"),
                "检查源指向了别的文件: {}",
                url
            );
            assert!(
                url.contains("q541810/Marcel_ssh"),
                "检查源指向了别的仓库: {}",
                url
            );
        }
    }

    /// jsDelivr 源必须用 `@main` 分支引用，不要改成 tag。
    ///
    /// 选 `@main` 是刻意的：jsDelivr 对分支引用缓存 12 小时且 purge API 无效
    /// （只对 semver tag 生效），对 tag 引用则能立即 purge。看起来 tag 更"新鲜"，
    /// 但 tag 名要人工维护、且必须先 push tag 再让 latest.json 生效，顺序错了
    /// 客户端就会拿到一个还不存在的 tag → 404 → 检查更新失败。分支引用没有这个
    /// 顺序陷阱：latest.json 提交到 main 即生效，最多是内容晚 12 小时被 CDN
    /// 更新到，而 raw 主源无缓存、几乎总是最新。
    #[test]
    fn check_urls_prefer_main_branch_over_tag() {
        for url in CHECK_URLS {
            if !url.contains("jsdelivr.net") {
                continue;
            }
            assert!(
                url.contains("/gh/q541810/Marcel_ssh@main/"),
                "jsDelivr 源必须引用 main 分支而不是 tag（tag 需人工维护且有发布顺序陷阱）: {}",
                url
            );
        }
    }

    /// 回归（2026-10-18，发 v1.8.0 当天撞上）：**旧缓存的源排在前面时，
    /// 不能把新版本挡掉。**
    ///
    /// 实测五个检查源同一时刻返回的版本并不一致：raw / `fastly` / `gcore` 已是
    /// 1.8.0，而 `cdn`（第 1 个 jsDelivr 入口）与 `testingcf` 仍是 12 小时前的
    /// 1.7.2。旧逻辑「第一个成功者生效」在国内线路上（raw 常不可达）就停在
    /// `cdn` 的 1.7.2 上，用户要等该边缘节点的缓存过期才收得到更新。
    #[test]
    fn freshest_source_wins_regardless_of_order() {
        let stale = release_from(&json!({ "version": "1.7.2" }), false);
        let fresh = release_from(&json!({ "version": "1.8.0" }), false);

        // 真实情况：旧的排在新源前面 —— 也必须拿到新的
        let (best, _) = select_freshest(vec![Ok(stale.clone()), Ok(fresh.clone())]);
        assert_eq!(best.unwrap().version, "1.8.0");

        let (best, _) = select_freshest(vec![Ok(fresh.clone()), Ok(stale.clone())]);
        assert_eq!(best.unwrap().version, "1.8.0");

        // 夹在中间也一样（并发完成顺序不定）
        let (best, _) = select_freshest(vec![
            Ok(stale),
            Ok(fresh),
            Ok(release_from(&json!({ "version": "1.7.0" }), false)),
        ]);
        assert_eq!(best.unwrap().version, "1.8.0");
    }

    /// 比较走 semver 而不是字符串序：1.10.0 必须大于 1.9.0。
    #[test]
    fn freshest_compares_versions_as_semver() {
        let (best, _) = select_freshest(vec![
            Ok(release_from(&json!({ "version": "1.9.0" }), false)),
            Ok(release_from(&json!({ "version": "1.10.0" }), false)),
        ]);
        assert_eq!(best.unwrap().version, "1.10.0");
    }

    /// 单源失败、畸形版本号都不该让正常结果丢失；全失败才返回 None。
    #[test]
    fn freshest_tolerates_failures_and_malformed_versions() {
        // 有失败源 + 正常源 → 正常源生效，最后一个原因同时带回
        let (best, err) = select_freshest(vec![
            Err("raw 不可达".to_string()),
            Ok(release_from(&json!({ "version": "1.8.0" }), false)),
        ]);
        assert_eq!(best.unwrap().version, "1.8.0");
        assert_eq!(err.as_deref(), Some("raw 不可达"));

        // 畸形版本号先到、正常版本后到 → 正常的顶掉它
        let (best, _) = select_freshest(vec![
            Ok(release_from(&json!({ "version": "dev" }), false)),
            Ok(release_from(&json!({ "version": "1.8.0" }), false)),
        ]);
        assert_eq!(best.unwrap().version, "1.8.0");

        // 全都解析不出 → 退化成「第一个成功者生效」，仍要给出一份结果
        let (best, _) = select_freshest(vec![Ok(release_from(&json!({}), false))]);
        assert_eq!(best.unwrap().version, "");

        // 全部失败 → 无结果，只带回最后一个原因
        let (best, err) = select_freshest(vec![
            Err("a 超时".to_string()),
            Err("b 返回 404".to_string()),
        ]);
        assert!(best.is_none());
        assert_eq!(err.as_deref(), Some("b 返回 404"));
    }

    /// 每个候选各按平台取版本：桌面源与安卓源混着比时，安卓只比 `android` 里的
    /// 版本号，不能拿顶层的比。
    #[test]
    fn freshest_respects_platform_target_per_candidate() {
        let doc = json!({
            "version": "1.8.0",
            "android": { "version": "1.7.5" }
        });
        let (best, _) = select_freshest(vec![Ok(release_from(&doc, true))]);
        assert_eq!(best.unwrap().version, "1.7.5");
        let (best, _) = select_freshest(vec![Ok(release_from(&doc, false))]);
        assert_eq!(best.unwrap().version, "1.8.0");
    }
}
