use base64::Engine;
use serde::Serialize;
use tauri::{AppHandle, State};

use crate::commands::market::{download_first_with_progress, zip_urls_for_branch};
use crate::emit_event;
use crate::error::AppError;
use crate::skills::github::parse_skill_url;
use crate::skills::parser::{
    discover_repo_skills, process_md, process_repo_archive, process_zip, validate_resources,
    DiscoveredSkill, ParsedSkill,
};
use crate::skills::store::{Skill, SkillResource};
use crate::AppState;

#[tauri::command]
pub async fn import_skill_file(
    _state: State<'_, AppState>,
    file_data: String,
    file_name: String,
) -> Result<ParsedSkill, AppError> {
    let lower = file_name.to_lowercase();

    // .md file: base64-encoded text content → parse frontmatter directly
    if lower.ends_with(".md") {
        let content = String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(&file_data)
                .map_err(|e| AppError::Config(format!("base64 解码失败: {}", e)))?,
        )
        .map_err(|e| AppError::Config(format!("不是有效的 UTF-8: {}", e)))?;
        return process_md(&content, &file_name).map_err(AppError::Config);
    }

    // .zip / .skill: unzip → find .md → parse frontmatter
    if lower.ends_with(".zip") || lower.ends_with(".skill") {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&file_data)
            .map_err(|e| AppError::Config(format!("base64 解码失败: {}", e)))?;
        return process_zip(&bytes).map_err(AppError::Config);
    }

    Err(AppError::Config("仅支持 .md / .zip / .skill 文件".into()))
}

/// 仓库内发现的 skill 列表（仓库根地址进入「发现模式」时返回）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillDiscovery {
    /// `owner/repo`，用于界面提示。
    pub repo: String,
    pub skills: Vec<DiscoveredSkill>,
}

/// 仓库 archive 的短时缓存：发现 → 单个导入 → 批量逐个导入都会反复要同一个 zip，
/// 不缓存的话 N 个 skill 就下载 N+1 次。只留最近一份（仓库 zip 可达数十 MB，
/// 多留会白占内存），过期很快 —— 用户改了仓库重新点导入应当拿到新内容。
const ARCHIVE_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(120);

struct CachedArchive {
    key: String,
    at: std::time::Instant,
    bytes: std::sync::Arc<Vec<u8>>,
}

static ARCHIVE_CACHE: tokio::sync::Mutex<Option<CachedArchive>> =
    tokio::sync::Mutex::const_new(None);

/// 下载仓库 archive（复用市场镜像 / 代理兜底 / 大小上限），带短时缓存与进度。
///
/// 进度经 `skill-import-progress` 事件（`importId` / `received` / `total`）推送，
/// 用户可经 `skill_import_cancel(import_id)` 中断（返回 `Cancelled`）。命中缓存时
/// 不下载、不发事件。锁跨下载持有：并发的两个请求（如发现与导入同时发起）会排队
/// 复用同一次下载，而不是各自下载一遍。
async fn download_repo_archive(
    app: &AppHandle,
    state: &AppState,
    src: &crate::skills::github::SkillSource,
    mirror: Option<&str>,
    import_id: Option<&str>,
) -> Result<std::sync::Arc<Vec<u8>>, AppError> {
    let key = format!(
        "{}|{}|{}|{}",
        src.owner,
        src.repo,
        src.branch.as_deref().unwrap_or(""),
        mirror.unwrap_or("")
    );
    let mut guard = ARCHIVE_CACHE.lock().await;
    if let Some(c) = guard.as_ref() {
        if c.key == key && c.at.elapsed() < ARCHIVE_CACHE_TTL {
            return Ok(c.bytes.clone());
        }
    }
    // 没带 import_id 的调用方不关心进度/取消：给一个永不触发的通道即可
    let registration = import_id.map(|id| state.skill_import_cancel.register(id));
    let (_idle_tx, idle_rx) = tokio::sync::watch::channel(false);
    let mut cancel_rx = registration
        .as_ref()
        .map(|r| r.receiver())
        .unwrap_or(idle_rx);

    let urls = zip_urls_for_branch(&src.owner, &src.repo, src.branch.as_deref(), mirror);
    let app_for_emit = app.clone();
    let id_for_emit = import_id.map(str::to_string);
    let on_progress = move |received: u64, total: u64| {
        if let Some(id) = &id_for_emit {
            let _ = emit_event(
                &app_for_emit,
                "skill-import-progress",
                serde_json::json!({ "importId": id, "received": received, "total": total }),
            );
        }
    };
    let bytes = std::sync::Arc::new(
        download_first_with_progress(&urls, &mut cancel_rx, on_progress).await?,
    );
    *guard = Some(CachedArchive {
        key,
        at: std::time::Instant::now(),
        bytes: bytes.clone(),
    });
    Ok(bytes)
}

/// 中断正在进行的 GitHub skill 下载。
#[tauri::command]
pub async fn skill_import_cancel(
    state: State<'_, AppState>,
    import_id: String,
) -> Result<(), AppError> {
    state.skill_import_cancel.cancel(&import_id);
    Ok(())
}

/// 列出 GitHub 仓库（或其中某个子目录）里的所有 skill，供用户多选。
#[tauri::command]
pub async fn skill_discover_from_url(
    app: AppHandle,
    state: State<'_, AppState>,
    url: String,
    mirror: Option<String>,
    import_id: Option<String>,
) -> Result<SkillDiscovery, AppError> {
    let src = parse_skill_url(&url)?;
    let bytes =
        download_repo_archive(&app, &state, &src, mirror.as_deref(), import_id.as_deref()).await?;
    let all = discover_repo_skills(&bytes).map_err(AppError::Config)?;

    // 地址指向子目录时，只在该子树内找
    let skills = match &src.subpath {
        Some(p) => {
            let prefix = format!("{}/", p);
            all.into_iter()
                .filter(|d| d.subpath == *p || d.subpath.starts_with(&prefix))
                .collect::<Vec<_>>()
        }
        None => all,
    };
    if skills.is_empty() {
        return Err(AppError::Config(match &src.subpath {
            Some(p) => format!("{} 下未找到 SKILL.md", p),
            None => "仓库内未找到任何 SKILL.md".into(),
        }));
    }
    Ok(SkillDiscovery {
        repo: src.repo_slug(),
        skills,
    })
}

/// 从 GitHub 地址拉取指定 skill（`subpath` 相对仓库根；省略则用地址里的子路径）。
///
/// 只解析、不入库 —— 前端拿到 `ParsedSkill` 预览确认后再走 `skill_add` 保存。
#[tauri::command]
pub async fn skill_import_from_url(
    app: AppHandle,
    state: State<'_, AppState>,
    url: String,
    subpath: Option<String>,
    mirror: Option<String>,
    import_id: Option<String>,
) -> Result<ParsedSkill, AppError> {
    let src = parse_skill_url(&url)?;
    let target = subpath.or_else(|| src.subpath.clone()).unwrap_or_default();
    let bytes =
        download_repo_archive(&app, &state, &src, mirror.as_deref(), import_id.as_deref()).await?;
    process_repo_archive(&bytes, &target).map_err(AppError::Config)
}

#[tauri::command]
pub async fn skill_list(state: State<'_, AppState>) -> Result<Vec<Skill>, AppError> {
    let store = state.skill_store.read().await;
    Ok(store.list().to_vec())
}

#[tauri::command]
pub async fn skill_add(
    state: State<'_, AppState>,
    name: String,
    description: String,
    prompt: String,
    resources: Option<Vec<SkillResource>>,
) -> Result<Skill, AppError> {
    if name.trim().is_empty() {
        return Err(AppError::Config("Skill 名称不能为空".into()));
    }
    if prompt.trim().is_empty() {
        return Err(AppError::Config("Skill 提示词不能为空".into()));
    }
    let resources = resources.unwrap_or_default();
    validate_resources(&resources).map_err(AppError::Config)?;
    let cloned = state
        .skill_store
        .mutate(|store| {
            let mut skill = Skill::new(name, description, prompt);
            skill.resources = resources;
            // 新 skill 追加到用户排序列表末尾
            skill.position = store.next_position();
            let cloned = skill.clone();
            store.add(skill);
            Ok(cloned)
        })
        .await?;

    Ok(cloned)
}

#[tauri::command]
pub async fn skill_update(
    state: State<'_, AppState>,
    id: String,
    name: Option<String>,
    description: Option<String>,
    prompt: Option<String>,
    // None = 保持原有资源；Some = 整体替换（空数组 = 清空）
    resources: Option<Vec<SkillResource>>,
) -> Result<(), AppError> {
    if crate::skills::builtin::is_builtin_skill_id(&id) {
        return Err(AppError::Config(
            "内置 Skill 不可编辑，只能启用或禁用".into(),
        ));
    }
    if let Some(ref n) = name {
        if n.trim().is_empty() {
            return Err(AppError::Config("skill name cannot be empty".into()));
        }
    }
    if let Some(ref p) = prompt {
        if p.trim().is_empty() {
            return Err(AppError::Config("skill prompt cannot be empty".into()));
        }
    }
    if let Some(ref r) = resources {
        validate_resources(r).map_err(AppError::Config)?;
    }
    state
        .skill_store
        .mutate(|store| {
            if !store.update(&id, name, description, prompt, resources) {
                return Err(AppError::Config(format!("skill not found: {}", id)));
            }
            Ok(())
        })
        .await
}

#[tauri::command]
pub async fn skill_toggle(state: State<'_, AppState>, id: String) -> Result<(), AppError> {
    state
        .skill_store
        .mutate(|store| {
            if !store.toggle(&id) {
                return Err(AppError::Config(format!("skill not found: {}", id)));
            }
            Ok(())
        })
        .await
}

#[tauri::command]
pub async fn skill_delete(state: State<'_, AppState>, id: String) -> Result<(), AppError> {
    if crate::skills::builtin::is_builtin_skill_id(&id) {
        return Err(AppError::Config(
            "内置 Skill 不可删除，可在列表中禁用".into(),
        ));
    }
    state
        .skill_store
        .mutate(|store| {
            if !store.delete(&id) {
                return Err(AppError::Config(format!("skill not found: {}", id)));
            }
            Ok(())
        })
        .await
}

/// 用户 skill 手动排序（拖拽 / 上移下移）。ids 为完整的用户 skill id 顺序，
/// 内置 skill 不参与排序（前端置顶展示，后端拒绝）。
#[tauri::command]
pub async fn skill_reorder(state: State<'_, AppState>, ids: Vec<String>) -> Result<(), AppError> {
    if ids
        .iter()
        .any(|id| crate::skills::builtin::is_builtin_skill_id(id))
    {
        return Err(AppError::Config("内置 Skill 不参与手动排序".into()));
    }
    // 顺序没变 = 成功的 no-op：`apply_user_order` 只在有 skill 的 position 真的变了时才改候选值，
    // 没变 `mutate` 序列化前后相同，不写盘。
    state
        .skill_store
        .mutate(|store| {
            store.apply_user_order(&ids);
            Ok(())
        })
        .await
}
