//! 本机（用户这台电脑）文件工具：`local_read_file` / `local_write_file` /
//! `local_edit_file` / `local_list_directory`。
//!
//! ## 与远端四个工具的关系：同一套纯逻辑，只换数据来源
//!
//! 分页与字节预算（[`file_ops::build_read_view`]）、目录渲染与排序
//! （[`file_ops::build_directory_view`] / [`file_ops::entry_view`]）、匹配替换
//! （[`file_ops::resolve_edit_text`] / [`file_ops::apply_edit`]）与展示元数据
//! （[`file_ops::build_edit_display_metadata`]）全部直接调远端同款函数，IO 走
//! [`local_fs`]（Wave1 落地：`LocalPathPolicy` 路径安全 + 原子写 + 备份回滚 +
//! 大小预检）。所以产出格式（工具卡 summary + 正文 + metadata）与远端版逐字
//! 同构，模型在两侧看到的行为一致；**不在这里再写一遍任何校验或渲染**。
//!
//! BOM 是两侧唯一的形态差异：本机读拿得到原文件的 `has_bom`（`local_read_file`
//! 放进 metadata），写回时 `local_write_file` 有 `bom` 参数、`local_edit_file`
//! 自动带上原文件的那一位 —— 一次「读 → 改 → 写」不会静默改掉文件头字节。
//!
//! ## 与 dispatcher 的分工：接进同一套参数语义，机器相关的那几问由工具自己回答
//!
//! 四个工具在声明表（`tools/mod.rs`）里的参数语义：
//! - `local_read_file` 声明 `ToolSemantics::reads_path`：路径是**本机**的，但
//!   「读过了」这件事与机器无关 —— dispatcher 成功执行后按 `normalize_path` 记账，
//!   `local_edit_file` / `local_write_file` 的「写前必须已读」检查才有数据。不声明
//!   的话模型读一万遍也记不上账，每次本机编辑都会先被拦下。
//! - `local_write_file` / `local_edit_file` 分别声明 `overwrites_path` / `edits_path`：
//!   写前必须已读、受 `confirm_edit_file` 约束、编辑要预演 —— 这些判定与机器无关，
//!   照常生效。
//! - `local_list_directory` 保持 `NONE`：列目录不构成「读过这个文件」。
//!
//! dispatcher 里**机器相关**的几问，本机侧由工具自己回答（`AgentTool` 的
//! `target_exists` / `preview_write`，契约见 `tools/mod.rs`），不再让 dispatcher
//! 拿本机路径去问 SFTP：
//! - 存在性（`PathWrite::Overwrite` 的写前检查）→ [`local_target_exists`]
//!   （本机 stat，与远端 `remote_file_exists` 同口径），dispatcher 拿到答案就不调
//!   `remote_file_exists`；
//! - 审批前预演 → [`preview_local_edit`]，本机整读 + 与远端同一套纯逻辑
//!   （[`file_ops::resolve_edit_text`] / [`file_ops::apply_edit`] /
//!   [`build_edit_display_metadata`]），产出与远端 `edit_file` **逐字同形**的 diff
//!   metadata（前端 `FileChangeView` 认的是同一组字段）。
//!
//! 仍然按远端（POSIX）表算的只有一项：**受保护路径提权**
//! （`policy.is_protected_path`）。它只升不降 —— 对本机 Unix 系统目录给出保守的
//! 强制审批，对本机 Windows 盘符路径不命中（无副作用）。本机路径的合法性由
//! [`local_fs`] 的 `LocalPathPolicy` 全权负责（黑名单、符号链接判定顺序、
//! Windows 保留名、UNC/ADS 拒绝、写失败回滚）。
//!
//! 可用性只由声明表的 `ToolRoles::LocalSubOnly` 收敛：外层主 agent 与远端子 agent
//! 的 registry 里都没有这四个工具。「已读」表按任务隔离，本机子 agent 的 registry
//! 里没有任何远端文件工具，所以一张表里不会混进另一台机器的路径。
//!
//! 这四个工具**不碰 `ctx`**（不读写 SSH 会话、不落远程记账），所以不像
//! `local_bash` 那样需要 `ctx.local_side` 兜底 —— 它们无论被谁拿着，都只会读这台
//! 电脑上那个路径。

use async_trait::async_trait;
use serde_json::json;

use crate::agent::risk::Disposition;
use crate::agent::tools::file_ops::{
    apply_edit, build_directory_view, build_edit_display_metadata, build_read_view,
    directory_entries_metadata, entry_view, parse_directory_sort_by, parse_edit_params,
    resolve_edit_text, DirectoryEntryView, EditPreviewError, DEFAULT_LIST_LIMIT,
    DEFAULT_READ_MAX_LINES, EDIT_NOT_FOUND, MAX_FILE_WRITE_BYTES, MAX_LIST_LIMIT,
    MAX_READ_MAX_LINES,
};
use crate::agent::tools::local_fs;
use crate::agent::tools::{AgentTool, ToolContext, ToolOutput};
use crate::error::AppError;

// ─────────────── dispatcher 的本机侧答案（存在性 / 审批前预演） ───────────────

/// [`AgentTool::target_exists`] 的本体（本机版）：目标现在存在吗？
///
/// 与远端 `remote_file_exists`（SFTP stat）同口径：**跟随符号链接**，任何错误都按
/// 「不存在」处理（新建放行；权限 / 断链 / 非法路径的失败由写自己报，不在这里双重
/// 误拦新建）。它只服务 `PathWrite::Overwrite` 的「覆盖已存在文件前必须已读」那一步，
/// **不做**路径安全判定 —— 那是 [`local_fs::write`] 的事。
async fn local_target_exists(path: &str) -> bool {
    if path.trim().is_empty() {
        return false;
    }
    tokio::fs::metadata(path).await.is_ok()
}

/// [`AgentTool::preview_write`] 的本体（`local_edit_file` 版）：审批前的预演。
///
/// 与 `file_ops::preview_edit_for_approval` 逐句同构，只把数据源从 SFTP 换成
/// [`local_fs::read`]（BOM 被 read 剥掉，与 `execute` 的读法一致）；产出的 metadata
/// 与远端 `edit_file` 同形（同一个 [`build_edit_display_metadata`]），失败文案与
/// `LocalEditFileTool::execute` 逐字一致 —— 预演失败就是这次编辑注定失败。
async fn preview_local_edit(
    params: &serde_json::Value,
) -> Result<serde_json::Value, EditPreviewError> {
    let edit = parse_edit_params(params).map_err(|e| EditPreviewError {
        summary: "edit_file".into(),
        message: e,
    })?;

    let (current_bytes, _has_bom) =
        local_fs::read(&edit.path)
            .await
            .map_err(|e| EditPreviewError {
                summary: format!("edit {}", edit.path),
                message: e,
            })?;

    let current = String::from_utf8(current_bytes).map_err(|_| EditPreviewError {
        summary: format!("edit {}", edit.path),
        message: "file is not valid UTF-8; edit_file requires text files".into(),
    })?;

    let resolved =
        resolve_edit_text(&current, &edit.old_content, &edit.new_content).ok_or_else(|| {
            EditPreviewError {
                summary: format!("edit {}", edit.path),
                message: EDIT_NOT_FOUND.into(),
            }
        })?;

    let (updated, occurrences) =
        apply_edit(&current, &resolved, edit.replace_all).map_err(|e| EditPreviewError {
            summary: format!("edit {}", edit.path),
            message: e,
        })?;

    Ok(build_edit_display_metadata(
        &edit.path,
        &current,
        &updated,
        &resolved.old,
        &resolved.new,
        occurrences,
    ))
}

/// [`AgentTool::preview_write`] 的本体（`local_write_file` 版）。
///
/// `write_file` 的审批面板现行显示原始 JSON（声明表没给它
/// `preview_before_approval`），所以这条回答目前不会被 dispatcher 用到；保留它是
/// 为了**任何情况下都不回落远端预览** —— `None` 会让 dispatcher 去读服务器上的
/// 同名路径，把服务器的文件内容当成这台电脑的 `before`，比没有预览更糟。
///
/// 形状与编辑预览同源（同一个 [`build_edit_display_metadata`]，前端 `FileChangeView`
/// 字段一致）：`before` = 目标当前内容，`after` = 本次要写入的内容，`occurrences` = 1。
/// 与 `execute` 不同，读旧内容失败**不算**这次写失败（新建、无读权限、非 UTF-8、
/// 超过读取上限都可能仍然可写）：读不到就不给 `before`。
async fn preview_local_write(
    params: &serde_json::Value,
) -> Result<serde_json::Value, EditPreviewError> {
    let path = params
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if path.is_empty() {
        return Err(EditPreviewError {
            summary: "write_file".into(),
            message: "Missing 'path' parameter".into(),
        });
    }
    let content = params
        .get("content")
        .and_then(|v| v.as_str())
        .ok_or_else(|| EditPreviewError {
            summary: format!("write {}", path),
            message: "Missing 'content' parameter".into(),
        })?;
    if content.len() > MAX_FILE_WRITE_BYTES {
        return Err(EditPreviewError {
            summary: format!("write {}", path),
            message: format!(
                "content too large: {} bytes (limit {} bytes). Split the write.",
                content.len(),
                MAX_FILE_WRITE_BYTES
            ),
        });
    }

    // 旧内容只用于 `before`：读不到就当没有（新建文件 before 为空串），
    // 不因此判这次写失败 —— 写的成功条件与「能不能读旧内容」无关。
    let before = match local_fs::read(&path).await {
        Ok((bytes, _)) => String::from_utf8(bytes).unwrap_or_default(),
        Err(_) => String::new(),
    };
    Ok(build_edit_display_metadata(
        &path, &before, content, &before, content, 1,
    ))
}

// ────────────────────────── local_read_file ──────────────────────────

pub struct LocalReadFileTool;

impl LocalReadFileTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for LocalReadFileTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AgentTool for LocalReadFileTool {
    fn name(&self) -> &str {
        "local_read_file"
    }

    fn description(&self) -> &str {
        "Read a text file from the user's own computer (this computer, where Marcel SSH \
         runs) with line numbers and pagination. Use start_line and max_lines to \
         continue through long files. Non-UTF-8 bytes are replaced and reported in \
         metadata; a UTF-8 BOM (if present) is stripped from the returned text and \
         reported as `has_bom` so you can pass it back when writing the file again. \
         Files over 2 MB are rejected outright — read those in parts with local_bash \
         (PowerShell `Get-Content` / `Select-String`; `head` / `sed` / `grep` on \
         macOS/Linux)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Absolute path on this computer (e.g. C:\\Users\\you\\notes.txt or /Users/you/notes.txt)" },
                "start_line": { "type": "integer", "description": "1-based first line to return (default: 1)", "default": 1 },
                "max_lines": { "type": "integer", "description": "Maximum lines to return (default: 200, max: 2000)", "default": 200 },
                "show_line_numbers": { "type": "boolean", "description": "Prefix each returned line with its line number (default: true)", "default": true }
            },
            "required": ["path"]
        })
    }

    fn disposition(&self) -> Disposition {
        // 与远端 read_file 一致：只读，不需要审批。
        Disposition::Allow
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolOutput, AppError> {
        let path = params
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Agent("Missing 'path' parameter".into()))?;
        if path.is_empty() {
            return Ok(ToolOutput::fail("read_file", "empty path"));
        }
        let start_line = params
            .get("start_line")
            .and_then(|v| v.as_u64())
            .unwrap_or(1)
            .max(1) as usize;
        let max_lines = params
            .get("max_lines")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_READ_MAX_LINES as u64)
            .clamp(1, MAX_READ_MAX_LINES as u64) as usize;
        let show_line_numbers = params
            .get("show_line_numbers")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let (bytes, has_bom) = match local_fs::read(path).await {
            Ok(data) => data,
            Err(e) => return Ok(ToolOutput::fail(format!("read {}", path), e)),
        };

        let n = bytes.len();
        let read_view = build_read_view(&bytes, start_line, max_lines, show_line_numbers);
        let summary = format!(
            "read {} (lines {}-{} of {}, {} bytes)",
            path, read_view.start_line, read_view.end_line, read_view.total_lines, n
        );
        Ok(
            ToolOutput::ok(summary, read_view.body).with_metadata(json!({
                "path": path,
                "bytes": n,
                // 原文件带 UTF-8 BOM：local_write_file 原样回传（bom: true）就不会
                // 静默丢掉文件头那三个字节。
                "has_bom": has_bom,
                "total_lines": read_view.total_lines,
                "start_line": read_view.start_line,
                "end_line": read_view.end_line,
                "returned_lines": read_view.returned_lines,
                "next_line": read_view.next_line,
                "truncated": read_view.truncated,
                "lossy_utf8": read_view.lossy_utf8,
                "show_line_numbers": show_line_numbers
            })),
        )
    }
}

// ────────────────────────── local_write_file ──────────────────────────

pub struct LocalWriteFileTool;

impl LocalWriteFileTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for LocalWriteFileTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AgentTool for LocalWriteFileTool {
    fn name(&self) -> &str {
        "local_write_file"
    }

    fn description(&self) -> &str {
        "Write UTF-8 text to a file on the user's own computer (this computer, where \
         Marcel SSH runs), creating or overwriting it. Maximum size: 1 MB per call; \
         split larger writes. Always use this computer's absolute path (e.g. \
         C:\\Users\\you\\notes.txt or /Users/you/notes.txt), never the server's path. \
         Pass plain text content (not base64-encoded). Set `bom: true` only when the \
         file should start with a UTF-8 BOM — pass back the `has_bom` you got from \
         `local_read_file` when re-writing a file you read."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path":    { "type": "string", "description": "Absolute path on this computer" },
                "content": { "type": "string", "description": "UTF-8 content to write" },
                "bom":     { "type": "boolean", "description": "Prepend a UTF-8 BOM (default: false). Pass the `has_bom` value reported by local_read_file when re-writing that file.", "default": false }
            },
            "required": ["path", "content"]
        })
    }

    fn disposition(&self) -> Disposition {
        // 与远端 write_file 一致：写盘要用户点头（真实档位仍会按设置与路径再取严）。
        Disposition::Approval
    }

    /// 「目标存在吗」问本机：`PathWrite::Overwrite` 的写前检查不能拿本机路径去
    /// stat 服务器（见 [`local_target_exists`]）。
    async fn target_exists(&self, _ctx: &ToolContext, params: &serde_json::Value) -> Option<bool> {
        let path = params.get("path").and_then(|v| v.as_str()).unwrap_or("");
        Some(local_target_exists(path).await)
    }

    /// 审批前预演（本机版，见 [`preview_local_write`]）。当前声明表没给本工具开
    /// `preview_before_approval`，但答案必须在 —— `None` 会回落成远端预览。
    async fn preview_write(
        &self,
        _ctx: &ToolContext,
        params: &serde_json::Value,
    ) -> Option<Result<serde_json::Value, EditPreviewError>> {
        Some(preview_local_write(params).await)
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolOutput, AppError> {
        let path = params
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Agent("Missing 'path' parameter".into()))?;
        let content = params
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Agent("Missing 'content' parameter".into()))?;
        // 缺省 false：与人类编辑器的 `sftp_write_file`（`bom: Option<bool>`）同口径
        // ——旧行为（不带 BOM）不变，只有明确要求才加。
        let bom = params.get("bom").and_then(|v| v.as_bool()).unwrap_or(false);
        if path.is_empty() {
            return Ok(ToolOutput::fail("write_file", "empty path"));
        }
        if content.len() > MAX_FILE_WRITE_BYTES {
            return Ok(ToolOutput::fail(
                format!("write {}", path),
                format!(
                    "content too large: {} bytes (limit {} bytes). Split the write.",
                    content.len(),
                    MAX_FILE_WRITE_BYTES
                ),
            ));
        }

        let bytes = content.as_bytes();
        match local_fs::write(path, bytes, bom).await {
            Ok(()) => {
                let lines = content.lines().count();
                Ok(ToolOutput::ok(
                    format!("write {} ({} lines)", path, lines),
                    format!("wrote {} bytes to {}", bytes.len(), path),
                )
                .with_metadata(json!({
                    "path": path,
                    "bytes_sent": bytes.len(),
                    "bom": bom,
                })))
            }
            Err(e) => Ok(ToolOutput::fail(format!("write {}", path), e)),
        }
    }
}

// ────────────────────────── local_edit_file ──────────────────────────

pub struct LocalEditFileTool;

impl LocalEditFileTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for LocalEditFileTool {
    fn default() -> Self {
        Self::new()
    }
}

/// `private_interfaces` 豁免：与 `AgentTool` trait 上的那处同因（`preview_write`
/// 的 Err 载荷 `file_ops::EditPreviewError` 是 `pub(crate)`）。
#[allow(private_interfaces)]
#[async_trait]
impl AgentTool for LocalEditFileTool {
    fn name(&self) -> &str {
        "local_edit_file"
    }

    fn description(&self) -> &str {
        "Precisely edit a file on the user's own computer (this computer, where Marcel \
         SSH runs) by replacing an exact occurrence of `old_content` with \
         `new_content`. Fails if `old_content` is missing or appears more than once \
         (unless `replace_all` is true). Always read the file first with \
         `local_read_file` to obtain `old_content` verbatim. LF/CRLF differences are \
         tolerated; the file's existing line endings and its UTF-8 BOM (if any) are \
         preserved."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path":        { "type": "string", "description": "Absolute path on this computer" },
                "old_content": { "type": "string", "description": "Exact text currently in the file" },
                "new_content": { "type": "string", "description": "Replacement text" },
                "replace_all": { "type": "boolean", "description": "Replace all occurrences (default: false)", "default": false }
            },
            "required": ["path", "old_content", "new_content"]
        })
    }

    fn disposition(&self) -> Disposition {
        // 与远端 edit_file 一致：改盘要用户点头（真实档位仍会按设置与路径再取严）。
        Disposition::Approval
    }

    /// 「目标存在吗」问本机（本工具走 `PathWrite::Edit`，dispatcher 目前不问它；
    /// 答案仍然给本机版，任何将来问到它的路径都不会拿到服务器的结论）。
    async fn target_exists(&self, _ctx: &ToolContext, params: &serde_json::Value) -> Option<bool> {
        let path = params.get("path").and_then(|v| v.as_str()).unwrap_or("");
        Some(local_target_exists(path).await)
    }

    /// 审批前预演（本机版）：审批面板据此渲染与远端 `edit_file` 同形的 diff
    /// （见 [`preview_local_edit`]）。
    async fn preview_write(
        &self,
        _ctx: &ToolContext,
        params: &serde_json::Value,
    ) -> Option<Result<serde_json::Value, EditPreviewError>> {
        Some(preview_local_edit(params).await)
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolOutput, AppError> {
        let edit = match parse_edit_params(&params) {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::fail("edit_file", e)),
        };

        // BOM 位随原文一起带出来，写回时原样还回去：只改中间几行不该动文件头字节。
        let (current_bytes, has_bom) = match local_fs::read(&edit.path).await {
            Ok(data) => data,
            Err(e) => return Ok(ToolOutput::fail(format!("edit {}", edit.path), e)),
        };

        let current = match String::from_utf8(current_bytes) {
            Ok(s) => s,
            Err(_) => {
                return Ok(ToolOutput::fail(
                    format!("edit {}", edit.path),
                    "file is not valid UTF-8; edit_file requires text files",
                ));
            }
        };

        let resolved = match resolve_edit_text(&current, &edit.old_content, &edit.new_content) {
            Some(r) => r,
            None => {
                return Ok(ToolOutput::fail(
                    format!("edit {}", edit.path),
                    EDIT_NOT_FOUND,
                ))
            }
        };

        let (updated, occurrences) = match apply_edit(&current, &resolved, edit.replace_all) {
            Ok(result) => result,
            Err(e) => return Ok(ToolOutput::fail(format!("edit {}", edit.path), e)),
        };

        let metadata = build_edit_display_metadata(
            &edit.path,
            &current,
            &updated,
            &resolved.old,
            &resolved.new,
            occurrences,
        );

        match local_fs::write(&edit.path, updated.as_bytes(), has_bom).await {
            Ok(()) => Ok(ToolOutput::ok(
                format!(
                    "edit {} ({} replacement{})",
                    edit.path,
                    occurrences,
                    if occurrences == 1 { "" } else { "s" }
                ),
                format!(
                    "replaced {} occurrence(s) in {} ({} -> {} bytes){}",
                    occurrences,
                    edit.path,
                    current.len(),
                    updated.len(),
                    if resolved.line_ending_converted {
                        " [line endings normalized to the file's existing style]"
                    } else {
                        ""
                    }
                ),
            )
            .with_metadata(metadata)),
            Err(e) => Ok(ToolOutput::fail(format!("edit {}", edit.path), e)),
        }
    }
}

// ────────────────────────── local_list_directory ──────────────────────────

pub struct LocalListDirectoryTool;

impl LocalListDirectoryTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for LocalListDirectoryTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AgentTool for LocalListDirectoryTool {
    fn name(&self) -> &str {
        "local_list_directory"
    }

    fn description(&self) -> &str {
        "List a directory on the user's own computer (this computer, where Marcel SSH \
         runs) with pagination, sorting, and structured metadata. Requires this \
         computer's absolute path (e.g. C:\\Users\\you\\project or \
         /Users/you/project) — there is no remote current directory to fall back to. \
         Returns directories first by name by default."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Absolute directory path on this computer (e.g. C:\\Users\\you\\project or /Users/you/project)" },
                "offset": { "type": "integer", "description": "Number of sorted entries to skip (default: 0)", "default": 0 },
                "limit": { "type": "integer", "description": "Maximum entries to return (default: 200, max: 2000)", "default": 200 },
                "sort_by": { "type": "string", "enum": ["name", "size", "type"], "description": "Sort entries by name, size, or type (default: name)", "default": "name" }
            },
            "required": ["path"]
        })
    }

    fn disposition(&self) -> Disposition {
        // 与远端 list_directory 一致：只读，不需要审批。
        Disposition::Allow
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolOutput, AppError> {
        // 与远端 list_directory 的差别：`path` 必填。远端省略时落到 SSH 会话的
        // 工作目录，本机没有那个"当前目录"语义（省略只会落到应用进程的 CWD，
        // 对模型是随机的），所以宁可让它明确给出这台电脑上的绝对路径。
        let path = params
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Agent("Missing 'path' parameter".into()))?;
        if path.is_empty() {
            return Ok(ToolOutput::fail("list_directory", "empty path"));
        }
        let offset = params.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let limit = params
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_LIST_LIMIT as u64)
            .clamp(1, MAX_LIST_LIMIT as u64) as usize;
        let sort_by = parse_directory_sort_by(params.get("sort_by").and_then(|v| v.as_str()));

        match local_fs::list(path).await {
            Ok(entries) => {
                let entries: Vec<DirectoryEntryView> =
                    entries.into_iter().map(entry_view).collect();
                let view = build_directory_view(entries, offset, limit, sort_by);
                Ok(ToolOutput::ok(
                    format!(
                        "list {} ({} of {} entries)",
                        path, view.returned_entries, view.total_entries
                    ),
                    view.body,
                )
                .with_metadata(json!({
                    "path": path,
                    "total_entries": view.total_entries,
                    "returned_entries": view.returned_entries,
                    "offset": view.offset,
                    "limit": view.limit,
                    "next_offset": view.next_offset,
                    "entries": directory_entries_metadata(&view.entries)
                })))
            }
            Err(e) => Ok(ToolOutput::fail(format!("list {}", path), e)),
        }
    }
}

// ────────────────────────── tests ──────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 名字与档位逐项对齐远端同名工具（护栏：改名/改档位必须是有意的）。
    /// 读 / 列目录 `Allow`，写 / 编辑 `Approval`（与远端 write_file / edit_file 同档）。
    #[test]
    fn names_and_dispositions_match_the_remote_counterparts() {
        assert_eq!(LocalReadFileTool::new().name(), "local_read_file");
        assert_eq!(LocalWriteFileTool::new().name(), "local_write_file");
        assert_eq!(LocalEditFileTool::new().name(), "local_edit_file");
        assert_eq!(LocalListDirectoryTool::new().name(), "local_list_directory");

        assert_eq!(LocalReadFileTool::new().disposition(), Disposition::Allow);
        assert_eq!(
            LocalListDirectoryTool::new().disposition(),
            Disposition::Allow
        );
        assert_eq!(
            LocalWriteFileTool::new().disposition(),
            Disposition::Approval
        );
        assert_eq!(
            LocalEditFileTool::new().disposition(),
            Disposition::Approval
        );
    }

    /// 参数名必须与远端同款（模型在两侧看到同一套字段名），只有本机特有的
    /// `bom` 是多出来的。
    #[test]
    fn schemas_keep_the_remote_parameter_names() {
        let props = |tool: &dyn AgentTool| -> Vec<String> {
            let mut keys: Vec<String> = tool
                .parameters_schema()
                .get("properties")
                .and_then(|p| p.as_object())
                .map(|p| p.keys().cloned().collect())
                .unwrap_or_default();
            keys.sort();
            keys
        };
        let required = |tool: &dyn AgentTool| -> Vec<String> {
            let mut keys: Vec<String> = tool
                .parameters_schema()
                .get("required")
                .and_then(|r| r.as_array())
                .map(|r| {
                    r.iter()
                        .filter_map(|v| v.as_str())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            keys.sort();
            keys
        };

        assert_eq!(
            props(&LocalReadFileTool::new()),
            vec!["max_lines", "path", "show_line_numbers", "start_line"]
        );
        assert_eq!(required(&LocalReadFileTool::new()), vec!["path"]);

        assert_eq!(
            props(&LocalWriteFileTool::new()),
            vec!["bom", "content", "path"]
        );
        assert_eq!(
            required(&LocalWriteFileTool::new()),
            vec!["content", "path"]
        );
        assert_eq!(
            LocalWriteFileTool::new().parameters_schema()["properties"]["bom"]["default"],
            serde_json::json!(false),
            "bom 缺省 false：与人类编辑器的 sftp_write_file 同口径（旧行为不变）"
        );

        assert_eq!(
            props(&LocalEditFileTool::new()),
            vec!["new_content", "old_content", "path", "replace_all"]
        );
        assert_eq!(
            required(&LocalEditFileTool::new()),
            vec!["new_content", "old_content", "path"]
        );

        assert_eq!(
            props(&LocalListDirectoryTool::new()),
            vec!["limit", "offset", "path", "sort_by"]
        );
        assert_eq!(
            required(&LocalListDirectoryTool::new()),
            vec!["path"],
            "本机没有 SSH 会话工作目录，list 不能靠缺省路径"
        );
    }

    /// 四个工具都必须说明作用侧与「这是本机路径」——由 `agent::tools` 的
    /// `acting_tools_state_their_side` 统一核对；这里额外钉住 BOM 的往返说明。
    #[test]
    fn descriptions_explain_the_local_round_trip() {
        let read_tool = LocalReadFileTool::new();
        let write_tool = LocalWriteFileTool::new();
        let edit_tool = LocalEditFileTool::new();
        let read = read_tool.description();
        let write = write_tool.description();
        let edit = edit_tool.description();
        assert!(read.contains("has_bom"), "读要说明 has_bom 会回传: {read}");
        assert!(
            write.contains("has_bom"),
            "写要说明用读到的 has_bom 回传: {write}"
        );
        assert!(
            edit.contains("BOM"),
            "编辑要说明原文件 BOM 会被保留: {edit}"
        );
    }

    // ── dispatcher 的本机侧答案：target_exists / preview_write ──

    /// 预演 / 存在性用例的落盘目录。
    ///
    /// **不能**直接用 `TempDir::new()`：Windows 上它落在 `%LOCALAPPDATA%`、macOS 上
    /// 落在 `/var/folders` —— 两处都在 `local_fs` 默认 policy 的黑名单内，而
    /// `local_fs::read` 用的是默认 policy，会当场拒绝（`local_fs` 自己的用例靠
    /// 测试专用 `no_blacklist()` 绕过，本文件拿不到那个入口）。
    /// 所以在 **home 下**建（home 本身不在黑名单里，只有它的 `.ssh` / `.config`
    /// 等子目录在），并用 crate 内可见的黑名单判定兜底：万一这个位置也在黑名单里
    /// （例如以 root 跑、home 就是 `/root`），跳过用例，而不是让一条环境相关的
    /// 失败挂在头上。
    fn filesystem_fixture() -> Option<tempfile::TempDir> {
        let home = dirs::home_dir()?;
        let td = tempfile::Builder::new()
            .prefix(".marcel-local-file-ops-test-")
            .tempdir_in(&home)
            .ok()?;
        let probe = td.path().join("probe.txt");
        std::fs::write(&probe, b"probe").ok()?;
        if crate::agent::tools::sftp_transfer::blacklisted(
            &probe,
            &crate::agent::tools::sftp_transfer::default_blacklist(),
        ) {
            eprintln!("skip: 测试目录落在本机黑名单内：{}", td.path().display());
            return None;
        }
        Some(td)
    }

    /// 预演成功 / 失败的取用助手：`EditPreviewError` 没有 `Debug`（那是 `file_ops`
    /// 里的类型，本文件不改），`Result::expect` / `expect_err` 用不了。
    fn preview_ok(
        result: Result<serde_json::Value, EditPreviewError>,
        what: &str,
    ) -> serde_json::Value {
        match result {
            Ok(v) => v,
            Err(e) => panic!("{what}：应成功却失败（{}）", e.message),
        }
    }

    fn preview_err(
        result: Result<serde_json::Value, EditPreviewError>,
        what: &str,
    ) -> EditPreviewError {
        match result {
            Ok(v) => panic!("{what}：应失败却成功了（{v}）"),
            Err(e) => e,
        }
    }

    /// 存在性问的是**本机磁盘**（dispatcher 的 `PathWrite::Overwrite` 写前检查
    /// 据此判断「覆盖已有文件」还是「新建」）。判错一头会让写前检查失效，另一头
    /// 会把新建误拦成覆盖。
    #[tokio::test]
    async fn target_exists_answers_from_the_local_disk() {
        let Some(td) = filesystem_fixture() else {
            return;
        };
        let file = td.path().join("exists.txt");
        std::fs::write(&file, b"hello").unwrap();

        assert!(local_target_exists(file.to_str().unwrap()).await);
        assert!(!local_target_exists(td.path().join("nope.txt").to_str().unwrap()).await);
        assert!(!local_target_exists("").await, "空路径按不存在处理");
        assert!(!local_target_exists("   ").await);
        // 目录也算「存在」（与远端 SFTP stat 同口径）：这里只回答存在性，
        // 「往目录上写」的失败由 write 自己报。
        assert!(local_target_exists(td.path().to_str().unwrap()).await);
    }

    /// 编辑预演与远端 `edit_file` 的预览**同形**：同一个
    /// `build_edit_display_metadata`，只有数据源换成本机文件。dispatcher 把这份
    /// metadata 交给审批面板，前端 `FileChangeView` 认的就是
    /// `before` / `after` / `occurrences` / `path` 这一组字段。
    #[tokio::test]
    async fn preview_edit_shape_matches_the_remote_edit_preview() {
        let Some(td) = filesystem_fixture() else {
            return;
        };
        let file = td.path().join("edit-me.txt");
        let path = file.to_str().unwrap().to_string();
        std::fs::write(&file, "alpha\nbeta\ngamma\n").unwrap();

        let meta = preview_ok(
            preview_local_edit(&json!({
                "path": path,
                "old_content": "beta",
                "new_content": "BETA",
            }))
            .await,
            "编辑预演",
        );

        assert_eq!(meta["path"], path);
        assert_eq!(meta["occurrences"], 1);
        assert_eq!(meta["before"], "alpha\nbeta\ngamma\n");
        assert_eq!(meta["after"], "alpha\nBETA\ngamma\n");
        assert_eq!(meta["file_content"], meta["after"]);
        assert_eq!(meta["line_position"], 2);
        assert_eq!(meta["match_line_positions"], json!([2]));
        assert_eq!(meta["old_bytes"], "alpha\nbeta\ngamma\n".len());
        assert_eq!(meta["new_bytes"], "alpha\nBETA\ngamma\n".len());
        assert!(
            meta.get("hunks").is_none(),
            "小文件给全文 before/after，不切 hunks"
        );
    }

    /// 预演失败 = 这次编辑注定失败：文案与 `LocalEditFileTool::execute` 的失败文案
    /// 逐字一致（也就与远端 `preview_edit_for_approval` 一致），dispatcher 走同一
    /// 条降级分支把它回给模型 —— 不能让审批因为「预演不了」而变成一次注定失败的批准。
    #[tokio::test]
    async fn preview_edit_failures_carry_the_execute_message() {
        let Some(td) = filesystem_fixture() else {
            return;
        };
        let file = td.path().join("edit-me.txt");
        let path = file.to_str().unwrap().to_string();
        std::fs::write(&file, "alpha\n").unwrap();

        let err = preview_err(
            preview_local_edit(&json!({
                "path": path.clone(),
                "old_content": "does-not-exist",
                "new_content": "x",
            }))
            .await,
            "old_content 不匹配",
        );
        assert_eq!(err.message, EDIT_NOT_FOUND);
        assert_eq!(err.summary, format!("edit {path}"));

        let err = preview_err(
            preview_local_edit(&json!({
                "path": td.path().join("gone.txt").to_str().unwrap(),
                "old_content": "a",
                "new_content": "b",
            }))
            .await,
            "目标读不到",
        );
        assert!(err.message.contains("无法读取"), "实际是 {}", err.message);

        // 非 UTF-8 与远端同一条拒绝。
        let binary = td.path().join("binary.bin");
        std::fs::write(&binary, [0xff, 0xfe, 0x00]).unwrap();
        let err = preview_err(
            preview_local_edit(&json!({
                "path": binary.to_str().unwrap(),
                "old_content": "a",
                "new_content": "b",
            }))
            .await,
            "非 UTF-8",
        );
        assert!(
            err.message.contains("not valid UTF-8"),
            "实际是 {}",
            err.message
        );

        // 参数不合格同样在这里被拦下（summary 与 execute 的第一句一致）。
        let err = preview_err(
            preview_local_edit(&json!({ "path": path })).await,
            "缺 old_content",
        );
        assert_eq!(err.summary, "edit_file");
    }

    /// 写预演：新建 → `before` 为空、`after` 是新内容；覆盖既有文件 → `before`
    /// 是原内容。形状同样是 `build_edit_display_metadata` 的产物。
    #[tokio::test]
    async fn preview_write_shows_before_and_after() {
        let Some(td) = filesystem_fixture() else {
            return;
        };

        let fresh = td.path().join("brand-new.txt");
        let meta = preview_ok(
            preview_local_write(&json!({
                "path": fresh.to_str().unwrap(),
                "content": "hello\n",
            }))
            .await,
            "新建的预演",
        );
        assert_eq!(meta["before"], "");
        assert_eq!(meta["after"], "hello\n");
        assert_eq!(meta["file_content"], "hello\n");
        assert_eq!(meta["occurrences"], 1);
        assert_eq!(meta["old_bytes"], 0);

        let existing = td.path().join("existing.txt");
        std::fs::write(&existing, "old\n").unwrap();
        let meta = preview_ok(
            preview_local_write(&json!({
                "path": existing.to_str().unwrap(),
                "content": "new\n",
            }))
            .await,
            "覆盖的预演",
        );
        assert_eq!(meta["before"], "old\n");
        assert_eq!(meta["after"], "new\n");
        assert_eq!(meta["old_bytes"], 4);
    }

    /// 写预演与编辑预演刻意不同的一条：**旧内容读不到不算这次写失败**（新建、
    /// 非 UTF-8、无读权限、超过读取上限都可能仍然可写），只是没有 `before`。
    #[tokio::test]
    async fn preview_write_tolerates_unreadable_previous_content() {
        let Some(td) = filesystem_fixture() else {
            return;
        };
        let binary = td.path().join("binary.bin");
        std::fs::write(&binary, [0xff, 0xfe, 0x00]).unwrap();

        let meta = preview_ok(
            preview_local_write(&json!({
                "path": binary.to_str().unwrap(),
                "content": "text now\n",
            }))
            .await,
            "非 UTF-8 原文",
        );
        assert_eq!(meta["before"], "");
        assert_eq!(meta["after"], "text now\n");

        // 参数/大小不合格仍然是失败（与 execute 的失败条件一致）。
        let err = preview_err(
            preview_local_write(&json!({ "path": binary.to_str().unwrap() })).await,
            "缺 content",
        );
        assert_eq!(err.summary, format!("write {}", binary.display()));

        let err = preview_err(
            preview_local_write(&json!({
                "path": binary.to_str().unwrap(),
                "content": "x".repeat(MAX_FILE_WRITE_BYTES + 1),
            }))
            .await,
            "超限",
        );
        assert!(
            err.message.contains("content too large"),
            "实际是 {}",
            err.message
        );
    }
}
