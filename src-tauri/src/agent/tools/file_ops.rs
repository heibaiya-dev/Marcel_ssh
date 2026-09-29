//! File-system tools (read / write / edit / list).
//!
//! 分页 / 字节预算 / 匹配替换 / 展示元数据等**纯逻辑**与「文件从哪来」无关；
//! 真正的 IO 只有三件事（整读 / 整写 / 列目录），全部收在 [`FileBackend`] 后面。
//! 当前实现是 [`SftpBackend`]（经 SSH 会话的 SFTP 子系统，binary-safe）；
//! 本机文件工具实现同一个 trait 即可复用这里的全部纯逻辑。

use async_trait::async_trait;
use russh_sftp::protocol::OpenFlags;
use serde_json::json;
use tokio::io::AsyncWriteExt;

use crate::agent::risk::Disposition;
use crate::agent::tools::{truncate_output, AgentTool, ToolContext, ToolOutput};
use crate::error::AppError;

pub(crate) const MAX_READ_BYTES: usize = 16_000;
pub(crate) const MAX_LIST_BYTES: usize = 8_000;
pub(crate) const MAX_FILE_WRITE_BYTES: usize = 1_000_000;
/// 单次整读的文件上限（`read_file` / `edit_file` / 审批预览共用）。
///
/// 依据：`SftpSession::read` 在 russh-sftp 2.3.0 里就是 `open` + `read_to_end`
/// （src/client/session.rs），整个文件一次性进内存；人类编辑器路径
/// （`commands::sftp::sftp_read_file`）的 `MAX_EDITOR_FILE_SIZE` 同样是 2 MiB，
/// 这里取同值，免得同一个文件在两套路径上得到互相矛盾的结论。
/// 超限**不截断**而是直接失败并指路 bash —— 静默截断会让模型以为读全了。
pub(crate) const MAX_READ_FILE_BYTES: u64 = 2 * 1024 * 1024;
pub(crate) const DEFAULT_READ_MAX_LINES: usize = 200;
pub(crate) const MAX_READ_MAX_LINES: usize = 2_000;
pub(crate) const DEFAULT_LIST_LIMIT: usize = 200;
pub(crate) const MAX_LIST_LIMIT: usize = 2_000;

pub(crate) struct ReadView {
    pub(crate) body: String,
    pub(crate) total_lines: usize,
    pub(crate) start_line: usize,
    pub(crate) end_line: usize,
    pub(crate) returned_lines: usize,
    pub(crate) next_line: Option<usize>,
    pub(crate) truncated: bool,
    pub(crate) lossy_utf8: bool,
}

#[derive(Clone)]
pub(crate) struct DirectoryEntryView {
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) size: u64,
    pub(crate) permissions: u32,
    pub(crate) permissions_text: String,
}

pub(crate) struct DirectoryView {
    pub(crate) body: String,
    pub(crate) total_entries: usize,
    pub(crate) returned_entries: usize,
    pub(crate) offset: usize,
    pub(crate) limit: usize,
    pub(crate) next_offset: Option<usize>,
    pub(crate) entries: Vec<DirectoryEntryView>,
}

#[derive(Clone, Copy)]
pub(crate) enum DirectorySortBy {
    Name,
    Size,
    Type,
}

// ───────────────────── Shared SFTP helpers ─────────────────────

fn format_size_mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_048_576.0)
}

/// 超限则返回给模型的失败文案（`None` = 可以整读）。
pub(crate) fn read_size_limit_error(path: &str, size: u64) -> Option<String> {
    (size > MAX_READ_FILE_BYTES).then(|| {
        format!(
            "{}: file is {} (limit {}); refusing to load the whole file into memory. \
             Read it in parts with bash instead — e.g. `head -n 200 {}`, \
             `sed -n '1000,1200p' {}`, or `grep -n` to locate the section you need.",
            path,
            format_size_mb(size),
            format_size_mb(MAX_READ_FILE_BYTES),
            path,
            path
        )
    })
}

/// sidecar 临时路径要求路径里必须有 `/`；裸相对文件名（`notes.txt`）补成 `./notes.txt`。
/// 两者对 SFTP 服务端解析到同一位置，只是为了能算出同目录的临时文件名。
pub(crate) fn with_dot_slash(path: &str) -> String {
    if path.contains('/') {
        path.to_string()
    } else {
        format!("./{}", path)
    }
}

// ───────────────────── File access backend ─────────────────────

/// 目录条目的原始类型（`DirectoryEntryView::kind` 由它派生）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum EntryKind {
    Dir,
    File,
    Symlink,
}

impl EntryKind {
    /// 与列表渲染 / 条目元数据一致的 kind 字符串。
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            EntryKind::Dir => "directory",
            EntryKind::File => "file",
            EntryKind::Symlink => "symlink",
        }
    }
}

/// 后端返回的原始目录条目（名字 / 类型 / 大小 / mode），不含任何展示加工。
#[derive(Clone, Debug)]
pub(crate) struct RawEntry {
    pub(crate) name: String,
    pub(crate) kind: EntryKind,
    pub(crate) size: u64,
    /// 原始 mode（后端给不出权限时为 `0`）。
    pub(crate) mode: u32,
}

/// 文件访问后端：远端 SFTP 与本机文件系统共用的最小接口。
///
/// 只有这三个方法——分页 / 字节预算 / 匹配替换 / 展示元数据全在后端之上的
/// 纯逻辑里，后端之间不得有行为分歧（错误文案即面向模型的输出）。
#[async_trait]
pub(crate) trait FileBackend: Send + Sync {
    /// 读整个文件（含大小预检与错误文案）。
    async fn read(&self, path: &str) -> Result<Vec<u8>, String>;
    /// 原子写整个文件（临时文件 + rename；目标已存在时按后端语义处理）。
    async fn write(&self, path: &str, bytes: &[u8]) -> Result<(), String>;
    /// 列目录（返回原始条目：名字 / 类型 Dir|File|Symlink / 大小 / mode）。
    async fn list(&self, path: &str) -> Result<Vec<RawEntry>, String>;
}

/// 远端后端：经 SSH 会话的 SFTP 子系统读写。
pub(crate) struct SftpBackend<'a> {
    ssh: &'a crate::ssh::connection::SshManager,
    session_id: &'a str,
}

impl<'a> SftpBackend<'a> {
    pub(crate) fn new(ssh: &'a crate::ssh::connection::SshManager, session_id: &'a str) -> Self {
        Self { ssh, session_id }
    }
}

#[async_trait]
impl FileBackend for SftpBackend<'_> {
    /// 读取远端文件全文。
    ///
    /// 读之前先用 `metadata` 预检大小：`SftpSession::read` 是 `open` + `read_to_end`，
    /// 整个文件进内存，移动端拿到超大文件会被 LMK 杀掉。
    async fn read(&self, path: &str) -> Result<Vec<u8>, String> {
        let sftp = self
            .ssh
            .open_sftp(self.session_id)
            .await
            .map_err(|e| format!("SFTP unavailable: {}", e))?;
        let metadata = sftp
            .metadata(path)
            .await
            .map_err(|e| format!("SFTP stat failed: {}", e))?;
        if let Some(err) = read_size_limit_error(path, metadata.len()) {
            return Err(err);
        }
        let data = sftp
            .read(path)
            .await
            .map_err(|e| format!("SFTP read failed: {}", e))?;
        // 兜底：stat 与 read 之间文件可能被追加，服务端报的大小也可能失真
        if let Some(err) = read_size_limit_error(path, data.len() as u64) {
            return Err(err);
        }
        Ok(data)
    }

    /// 写入远端文件：先写同目录 sidecar，再原子提交（rename）到目标。
    ///
    /// 之前这里是 `CREATE | TRUNCATE | WRITE` 直开目标文件：写出错 / 超时 / 任务被取消时
    /// 远端原文件已被截断成半成品，无法恢复。人类编辑器
    /// （`commands::sftp::sftp_write_file`）与上传走的都是 sidecar + rename，
    /// 这里复用同一对共享实现（`remote_sidecar_path` / `commit_remote_temp_file`）。
    /// 取舍：目标若是符号链接，会被整体替换成普通文件（与人类编辑器路径行为一致）。
    async fn write(&self, path: &str, bytes: &[u8]) -> Result<(), String> {
        let sftp = self
            .ssh
            .open_sftp(self.session_id)
            .await
            .map_err(|e| format!("SFTP unavailable: {}", e))?;
        let target = with_dot_slash(path);
        let temp_path = crate::commands::sftp::remote_sidecar_path(&target, "edit")
            .map_err(|e| format!("temp path failed: {}", e))?;
        let mut file = sftp
            .open_with_flags(
                &temp_path,
                OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
            )
            .await
            .map_err(|e| format!("open failed: {}", e))?;
        if let Err(e) = file.write_all(bytes).await {
            drop(file);
            let _ = sftp.remove_file(&temp_path).await;
            return Err(format!("write failed: {}", e));
        }
        if let Err(e) = file.flush().await {
            drop(file);
            let _ = sftp.remove_file(&temp_path).await;
            return Err(format!("flush failed: {}", e));
        }
        drop(file);
        if let Err(e) =
            crate::commands::sftp::commit_remote_temp_file(&sftp, &temp_path, &target, true).await
        {
            // commit 内部多数分支已清理临时文件，这里兜底（含 rename 失败那条）
            let _ = sftp.remove_file(&temp_path).await;
            return Err(format!("commit failed: {}", e));
        }
        Ok(())
    }

    async fn list(&self, path: &str) -> Result<Vec<RawEntry>, String> {
        let sftp = self
            .ssh
            .open_sftp(self.session_id)
            .await
            .map_err(|e| format!("SFTP unavailable: {}", e))?;
        let mut dir = sftp
            .read_dir(path)
            .await
            .map_err(|e| format!("SFTP list failed: {}", e))?;

        let mut entries = Vec::new();
        while let Some(entry) = dir.next() {
            let metadata = entry.metadata();
            let name = entry.file_name();
            let size = metadata.len();
            let mode = metadata.permissions.unwrap_or(0);
            let kind = if metadata.is_dir() {
                EntryKind::Dir
            } else if metadata.is_symlink() {
                EntryKind::Symlink
            } else {
                EntryKind::File
            };
            entries.push(RawEntry {
                name,
                kind,
                size,
                mode,
            });
        }
        Ok(entries)
    }
}

// ────────────────────────────── Parse helpers ──────────────────────────────

pub(crate) struct EditParams {
    pub(crate) path: String,
    pub(crate) old_content: String,
    pub(crate) new_content: String,
    pub(crate) replace_all: bool,
}

pub(crate) fn parse_edit_params(params: &serde_json::Value) -> Result<EditParams, String> {
    let path = params
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'path' parameter")?;
    let old_content = params
        .get("old_content")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'old_content' parameter")?;
    let new_content = params
        .get("new_content")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'new_content' parameter")?;
    let replace_all = params
        .get("replace_all")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if path.is_empty() {
        return Err("empty path".into());
    }
    if old_content.is_empty() {
        return Err("old_content must not be empty".into());
    }
    Ok(EditParams {
        path: path.to_string(),
        old_content: old_content.to_string(),
        new_content: new_content.to_string(),
        replace_all,
    })
}

pub(crate) const EDIT_DISPLAY_MAX_BYTES: usize = 16_000;
pub(crate) const EDIT_DISPLAY_CONTEXT_LINES: usize = 30;

fn line_number_at_byte(content: &str, byte_pos: usize) -> usize {
    let pos = byte_pos.min(content.len());
    content[..pos].lines().count() + 1
}

/// Non-overlapping left-to-right match starts (same as `str::replace`).
fn match_byte_positions(content: &str, target: &str) -> Vec<usize> {
    if target.is_empty() {
        return Vec::new();
    }
    let mut positions = Vec::new();
    let mut search_from = 0;
    while search_from <= content.len() {
        if let Some(rel) = content[search_from..].find(target) {
            let abs = search_from + rel;
            positions.push(abs);
            search_from = abs + target.len();
        } else {
            break;
        }
    }
    positions
}

fn match_line_positions(content: &str, target: &str) -> Vec<usize> {
    match_byte_positions(content, target)
        .into_iter()
        .map(|p| line_number_at_byte(content, p))
        .collect()
}

fn extract_window_around(
    content: &str,
    center_line_1: usize,
    span_lines: usize,
    context_lines: usize,
) -> (usize, String) {
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return (1, String::new());
    }
    let idx = center_line_1
        .saturating_sub(1)
        .min(lines.len().saturating_sub(1));
    let start = idx.saturating_sub(context_lines);
    let end = (idx + span_lines.max(1) + context_lines).min(lines.len());
    (start + 1, lines[start..end].join("\n"))
}

fn extract_display_content(
    content: &str,
    line_position: usize,
    target_line_count: usize,
    context_lines: usize,
    max_bytes: usize,
) -> String {
    if content.len() <= max_bytes {
        return content.to_string();
    }
    let lines: Vec<&str> = content.lines().collect();
    let line_idx = line_position.saturating_sub(1);
    let start = line_idx.saturating_sub(context_lines);
    let end = (line_idx + target_line_count + context_lines).min(lines.len());
    lines[start..end].join("\n")
}

/// 匹配失败时的统一文案。行尾差异已自动处理，剩下的只可能是内容本身不一致，
/// 所以强调「逐字符一致（含缩进）」，不再让模型只是"重读一遍"却毫无线索。
pub(crate) const EDIT_NOT_FOUND: &str =
    "old_content not found in file. Read the file again to refresh, then copy the exact \
     characters (indentation included; LF/CRLF differences are handled automatically).";

/// 命中处文件实际使用的行尾（只区分 LF / CRLF，孤立的 `\r` 不参与转换）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum LineEnding {
    Lf,
    Crlf,
}

impl LineEnding {
    /// 把文本换行统一成该风格（先归一成 LF 再展开，幂等）。
    fn normalize(self, text: &str) -> String {
        let lf = text.replace("\r\n", "\n");
        match self {
            LineEnding::Lf => lf,
            LineEnding::Crlf => lf.replace('\n', "\r\n"),
        }
    }
}

/// 实际落地的替换对：`old` 是命中的那份，`new` 与它行尾一致。
pub(crate) struct ResolvedEditText {
    pub(crate) old: String,
    pub(crate) new: String,
    /// 是否靠行尾互换才命中（模型给的行尾与文件不一致）。
    pub(crate) line_ending_converted: bool,
}

/// 解析真正用于匹配/替换的 (old, new)：先按模型给的原样精确匹配，失败再试行尾互换。
///
/// 必须补这一步的原因：读取视图用 `str::lines()` 分行，CRLF 的 `\r` 在给模型看之前
/// 就被吃掉了，模型照抄读到的 `old_content` 只有 LF，对 CRLF 文件**永远**匹配不上，
/// 报错还让它 "Read the file again" —— 重读依旧是 LF，死循环。
///
/// 命中转换后的候选时 `new_content` 一并转换，免得写回去的文件一半 CRLF 一半 LF。
/// 取舍：宁可跟随文件既有行尾，也不保留模型在归一化视图里写出的行尾；只有原样
/// 匹配失败（即文件里根本不存在模型给的那串字节）才会走到这里。
pub(crate) fn resolve_edit_text(
    current: &str,
    old_content: &str,
    new_content: &str,
) -> Option<ResolvedEditText> {
    if current.contains(old_content) {
        return Some(ResolvedEditText {
            old: old_content.to_string(),
            new: new_content.to_string(),
            line_ending_converted: false,
        });
    }
    for eol in [LineEnding::Crlf, LineEnding::Lf] {
        let candidate = eol.normalize(old_content);
        if candidate != old_content && current.contains(&candidate) {
            return Some(ResolvedEditText {
                old: candidate,
                new: eol.normalize(new_content),
                line_ending_converted: true,
            });
        }
    }
    None
}

/// 在已解析好 (old, new) 的前提下执行替换（计数语义与非重叠左到右切分一致）。
pub(crate) fn apply_edit(
    current: &str,
    resolved: &ResolvedEditText,
    replace_all: bool,
) -> Result<(String, usize), String> {
    let occurrences = current.matches(resolved.old.as_str()).count();
    if occurrences == 0 {
        return Err(EDIT_NOT_FOUND.into());
    }
    if occurrences > 1 && !replace_all {
        return Err(format!(
            "old_content matches {} times; pass replace_all=true or supply more context",
            occurrences
        ));
    }
    let updated = if replace_all {
        current.replace(resolved.old.as_str(), resolved.new.as_str())
    } else {
        current.replacen(resolved.old.as_str(), resolved.new.as_str(), 1)
    };
    if updated.len() > MAX_FILE_WRITE_BYTES {
        return Err(format!(
            "result exceeds size limit ({} bytes; limit {})",
            updated.len(),
            MAX_FILE_WRITE_BYTES
        ));
    }
    Ok((updated, occurrences))
}

/// `resolve_edit_text` + `apply_edit` 的组合体，供单测直接驱动整条匹配逻辑。
/// 生产路径要保留命中的文本对去渲染 diff 元数据，所以分两步调用，不走这里。
#[cfg(test)]
fn try_replace(
    current: &str,
    old_content: &str,
    new_content: &str,
    replace_all: bool,
) -> Result<(String, usize), String> {
    let resolved = resolve_edit_text(current, old_content, new_content)
        .ok_or_else(|| EDIT_NOT_FOUND.to_string())?;
    apply_edit(current, &resolved, replace_all)
}

/// Summary + message for failed edit (same strings as `EditFileTool::execute`).
///
/// `pub` 而不是 `pub(crate)`：`AgentTool` 的两个默认方法（`target_exists` /
/// `preview_write`）是 `pub trait` 的一部分，本机工具要复用它；否则 trait 里
/// 一出现这个名字就要挂 `#[allow(private_interfaces)]`。
pub struct EditPreviewError {
    pub summary: String,
    pub message: String,
}

pub(crate) fn build_edit_display_metadata(
    path: &str,
    current: &str,
    updated: &str,
    old_content: &str,
    new_content: &str,
    occurrences: usize,
) -> serde_json::Value {
    let byte_positions = match_byte_positions(current, old_content);
    let match_lines = match_line_positions(current, old_content);
    let line_position = match_lines.first().copied().unwrap_or(1);
    let line_count = updated.lines().count();
    let old_line_span = old_content.lines().count().max(1);
    let new_line_span = new_content.lines().count().max(1);
    let delta_bytes = new_content.len() as isize - old_content.len() as isize;

    let small_enough =
        current.len() <= EDIT_DISPLAY_MAX_BYTES && updated.len() <= EDIT_DISPLAY_MAX_BYTES;

    if small_enough {
        return json!({
            "path": path,
            "occurrences": occurrences,
            "old_bytes": current.len(),
            "new_bytes": updated.len(),
            "line_position": line_position,
            "line_count": line_count,
            "match_line_positions": match_lines,
            "before": current,
            "after": updated,
            // Compat for older FileChangeView fallback
            "file_content": updated,
        });
    }

    // Large file: per-match context windows on before/after.
    let mut hunks = Vec::new();
    for (i, &byte_pos) in byte_positions.iter().enumerate() {
        let before_line = line_number_at_byte(current, byte_pos);
        let (start_line, before_snip) = extract_window_around(
            current,
            before_line,
            old_line_span,
            EDIT_DISPLAY_CONTEXT_LINES,
        );
        let updated_byte = (byte_pos as isize + i as isize * delta_bytes).max(0) as usize;
        let updated_byte = updated_byte.min(updated.len());
        let after_line = line_number_at_byte(updated, updated_byte);
        let (_after_start, after_snip) = extract_window_around(
            updated,
            after_line,
            new_line_span,
            EDIT_DISPLAY_CONTEXT_LINES,
        );
        // Cap each snip so a single huge line cannot blow the event payload.
        let before_snip = if before_snip.len() > EDIT_DISPLAY_MAX_BYTES {
            extract_display_content(
                &before_snip,
                1,
                old_line_span,
                EDIT_DISPLAY_CONTEXT_LINES,
                EDIT_DISPLAY_MAX_BYTES,
            )
        } else {
            before_snip
        };
        let after_snip = if after_snip.len() > EDIT_DISPLAY_MAX_BYTES {
            extract_display_content(
                &after_snip,
                1,
                new_line_span,
                EDIT_DISPLAY_CONTEXT_LINES,
                EDIT_DISPLAY_MAX_BYTES,
            )
        } else {
            after_snip
        };
        hunks.push(json!({
            "startLine": start_line,
            "before": before_snip,
            "after": after_snip,
        }));
    }

    let first_after = hunks
        .first()
        .and_then(|h| h.get("after"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    json!({
        "path": path,
        "occurrences": occurrences,
        "old_bytes": current.len(),
        "new_bytes": updated.len(),
        "line_position": line_position,
        "line_count": line_count,
        "match_line_positions": match_lines,
        "hunks": hunks,
        "file_content": first_after,
    })
}

/// Read remote file and validate the edit would succeed (no write).
/// Used before human approval so the dialog can show full-file context diff.
pub(crate) async fn preview_edit_for_approval(
    ssh: &crate::ssh::connection::SshManager,
    session_id: &str,
    params: &serde_json::Value,
) -> Result<serde_json::Value, EditPreviewError> {
    let edit = parse_edit_params(params).map_err(|e| EditPreviewError {
        summary: "edit_file".into(),
        message: e,
    })?;

    let current_bytes = SftpBackend::new(ssh, session_id)
        .read(&edit.path)
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

pub struct ReadFileTool;

impl ReadFileTool {
    pub fn new() -> Self {
        Self
    }
}
impl Default for ReadFileTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AgentTool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read a text file from the remote server with line numbers and pagination. \
         Use start_line and max_lines to continue through long files. Non-UTF-8 \
         bytes are replaced and reported in metadata. Files over 2 MB are rejected \
         outright — read those in parts with bash (head / sed / grep)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Absolute path to the file" },
                "start_line": { "type": "integer", "description": "1-based first line to return (default: 1)", "default": 1 },
                "max_lines": { "type": "integer", "description": "Maximum lines to return (default: 200, max: 2000)", "default": 200 },
                "show_line_numbers": { "type": "boolean", "description": "Prefix each returned line with its line number (default: true)", "default": true }
            },
            "required": ["path"]
        })
    }

    fn disposition(&self) -> Disposition {
        Disposition::Allow
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: &ToolContext,
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

        let bytes = match SftpBackend::new(&ctx.ssh, &ctx.session_id).read(path).await {
            Ok(data) => data,
            Err(e) => {
                return Ok(ToolOutput::fail(format!("read {}", path), e));
            }
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

// ────────────────────────────── WriteFileTool ──────────────────────────────

pub struct WriteFileTool;
impl WriteFileTool {
    pub fn new() -> Self {
        Self
    }
}
impl Default for WriteFileTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AgentTool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Write UTF-8 text to a file on the remote server via SFTP, creating or \
         overwriting it. Maximum size: 1 MB per call; split larger writes. \
         Prefer absolute paths. Pass plain text content (not base64-encoded)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path":    { "type": "string", "description": "Absolute path to the file" },
                "content": { "type": "string", "description": "UTF-8 content to write" }
            },
            "required": ["path", "content"]
        })
    }

    fn disposition(&self) -> Disposition {
        Disposition::Approval
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput, AppError> {
        let path = params
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Agent("Missing 'path' parameter".into()))?;
        let content = params
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Agent("Missing 'content' parameter".into()))?;
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
        match SftpBackend::new(&ctx.ssh, &ctx.session_id)
            .write(path, bytes)
            .await
        {
            Ok(()) => {
                let lines = content.lines().count();
                Ok(ToolOutput::ok(
                    format!("write {} ({} lines)", path, lines),
                    format!("wrote {} bytes to {}", bytes.len(), path),
                )
                .with_metadata(json!({
                    "path": path,
                    "bytes_sent": bytes.len(),
                })))
            }
            Err(e) => Ok(ToolOutput::fail(format!("write {}", path), e)),
        }
    }
}

// ────────────────────────────── EditFileTool ──────────────────────────────

pub struct EditFileTool;
impl EditFileTool {
    pub fn new() -> Self {
        Self
    }
}
impl Default for EditFileTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AgentTool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Precisely edit a file on the remote server by replacing an exact occurrence \
         of `old_content` with `new_content`. Fails if `old_content` is missing or \
         appears more than once (unless `replace_all` is true). Always read the file \
         first to obtain `old_content` verbatim. LF/CRLF differences are tolerated \
         and the file's existing line endings are preserved."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path":        { "type": "string", "description": "Absolute path to the file" },
                "old_content": { "type": "string", "description": "Exact text currently in the file" },
                "new_content": { "type": "string", "description": "Replacement text" },
                "replace_all": { "type": "boolean", "description": "Replace all occurrences (default: false)", "default": false }
            },
            "required": ["path", "old_content", "new_content"]
        })
    }

    fn disposition(&self) -> Disposition {
        Disposition::Approval
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput, AppError> {
        let edit = match parse_edit_params(&params) {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::fail("edit_file", e)),
        };

        let current_bytes = match SftpBackend::new(&ctx.ssh, &ctx.session_id)
            .read(&edit.path)
            .await
        {
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

        match SftpBackend::new(&ctx.ssh, &ctx.session_id)
            .write(&edit.path, updated.as_bytes())
            .await
        {
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

// ────────────────────────────── ListDirectoryTool ──────────────────────────────

pub struct ListDirectoryTool;
impl ListDirectoryTool {
    pub fn new() -> Self {
        Self
    }
}
impl Default for ListDirectoryTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AgentTool for ListDirectoryTool {
    fn name(&self) -> &str {
        "list_directory"
    }

    fn description(&self) -> &str {
        "List a remote directory with pagination, sorting, and structured metadata. \
         Defaults to the current directory and returns directories first by name."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory path; absolute paths are preferred, default: '.'", "default": "." },
                "offset": { "type": "integer", "description": "Number of sorted entries to skip (default: 0)", "default": 0 },
                "limit": { "type": "integer", "description": "Maximum entries to return (default: 200, max: 2000)", "default": 200 },
                "sort_by": { "type": "string", "enum": ["name", "size", "type"], "description": "Sort entries by name, size, or type (default: name)", "default": "name" }
            },
            "required": []
        })
    }

    fn disposition(&self) -> Disposition {
        Disposition::Allow
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput, AppError> {
        let path = params.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let offset = params.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let limit = params
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_LIST_LIMIT as u64)
            .clamp(1, MAX_LIST_LIMIT as u64) as usize;
        let sort_by = parse_directory_sort_by(params.get("sort_by").and_then(|v| v.as_str()));

        match SftpBackend::new(&ctx.ssh, &ctx.session_id).list(path).await {
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

/// 后端原始条目 → 展示条目。kind 字符串与权限文本在这里定型，两个后端共用。
pub(crate) fn entry_view(raw: RawEntry) -> DirectoryEntryView {
    DirectoryEntryView {
        name: raw.name,
        kind: raw.kind.as_str().to_string(),
        size: raw.size,
        permissions: raw.mode,
        permissions_text: format_permissions(raw.mode),
    }
}

pub(crate) fn format_permissions(mode: u32) -> String {
    let perms = [
        if mode & 0o400 != 0 { 'r' } else { '-' },
        if mode & 0o200 != 0 { 'w' } else { '-' },
        if mode & 0o100 != 0 { 'x' } else { '-' },
        if mode & 0o040 != 0 { 'r' } else { '-' },
        if mode & 0o020 != 0 { 'w' } else { '-' },
        if mode & 0o010 != 0 { 'x' } else { '-' },
        if mode & 0o004 != 0 { 'r' } else { '-' },
        if mode & 0o002 != 0 { 'w' } else { '-' },
        if mode & 0o001 != 0 { 'x' } else { '-' },
    ];
    let mut result = String::with_capacity(9);
    result.extend(perms.iter());
    result
}

/// 拼装读取视图，`[next: ...]` 指针钉在**实际写出**的位置上。
///
/// 旧实现先按 `max_lines` 切页、再 `truncate_output` 截 body：长行文件
/// （minified JS、宽表）16 KB 预算只装得下 ~148 行，却提示 `start_line=201 to continue`，
/// 中间那几十行对模型**静默消失**（metadata 不进模型上下文）。
/// 现在改成写之前卡预算：谁的预算不够就从谁开始停，指针 = 第一条没整行写出的行。
pub(crate) fn build_read_view(
    bytes: &[u8],
    start_line: usize,
    max_lines: usize,
    show_line_numbers: bool,
) -> ReadView {
    let lossy_utf8 = std::str::from_utf8(bytes).is_err();
    let text = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = text.lines().collect();
    let total_lines = lines.len();

    let start_index = start_line.saturating_sub(1).min(total_lines);
    let end_index = start_index.saturating_add(max_lines).min(total_lines);

    let mut body = String::new();
    if lossy_utf8 {
        body.push_str("[warning: file contains non-UTF-8 bytes; invalid bytes were replaced]\n\n");
    }

    // 已在 body 里整段写出的行数（首行被截断时也记 1 行）。续读指针恒为
    // `start_index + emitted + 1`，即第一条没整行写出的行 —— 翻页首尾相接、不漏行。
    let mut emitted = 0usize;
    let mut single_line_cut = false;
    for (i, line) in lines[start_index..end_index].iter().enumerate() {
        let line_no = start_index + i + 1;
        let rendered = if show_line_numbers {
            format!("{:>6}: {}\n", line_no, line)
        } else {
            format!("{}\n", line)
        };
        if body.len() + rendered.len() <= MAX_READ_BYTES {
            body.push_str(&rendered);
            emitted += 1;
            continue;
        }
        if emitted == 0 {
            // 首行自己就吃掉整个预算（minified bundle）：给出前缀并**明说该行被切断**。
            // 按行翻页取不回同一行的后半段，只能靠 bash，所以不能只留个截断标记。
            let budget = MAX_READ_BYTES.saturating_sub(body.len());
            body.push_str(&truncate_output(rendered, budget));
            body.push_str(&format!(
                "\n[line {} is {} bytes; only its first {} bytes are shown — read the rest \
                 of this line with bash (sed / cut)]\n",
                line_no,
                line.len(),
                budget
            ));
            emitted = 1;
            single_line_cut = true;
        }
        break;
    }

    if emitted == 0 {
        // 空体必须给一句解释，否则模型会把「没输出」当成「文件是空的」
        if total_lines == 0 {
            body.push_str("[file is empty]\n");
        } else {
            body.push_str(&format!(
                "[start_line {} is past the end of the file ({} lines)]\n",
                start_line, total_lines
            ));
        }
    }

    let next_line = (start_index + emitted < total_lines).then_some(start_index + emitted + 1);
    let truncated = single_line_cut || next_line.is_some();
    if let Some(next) = next_line {
        body.push_str(&format!(
            "\n[next: call read_file with start_line={} to continue]",
            next
        ));
    }

    ReadView {
        body,
        total_lines,
        start_line,
        end_line: start_index + emitted,
        returned_lines: emitted,
        next_line,
        truncated,
        lossy_utf8,
    }
}

pub(crate) fn parse_directory_sort_by(sort_by: Option<&str>) -> DirectorySortBy {
    match sort_by
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("size") => DirectorySortBy::Size,
        Some("type") => DirectorySortBy::Type,
        _ => DirectorySortBy::Name,
    }
}

pub(crate) fn build_directory_view(
    mut entries: Vec<DirectoryEntryView>,
    offset: usize,
    limit: usize,
    sort_by: DirectorySortBy,
) -> DirectoryView {
    entries.sort_by(|a, b| match sort_by {
        DirectorySortBy::Name => directory_kind_rank(&a.kind)
            .cmp(&directory_kind_rank(&b.kind))
            .then_with(|| {
                a.name
                    .to_ascii_lowercase()
                    .cmp(&b.name.to_ascii_lowercase())
            }),
        DirectorySortBy::Size => b.size.cmp(&a.size).then_with(|| {
            a.name
                .to_ascii_lowercase()
                .cmp(&b.name.to_ascii_lowercase())
        }),
        DirectorySortBy::Type => directory_kind_rank(&a.kind)
            .cmp(&directory_kind_rank(&b.kind))
            .then_with(|| {
                a.name
                    .to_ascii_lowercase()
                    .cmp(&b.name.to_ascii_lowercase())
            }),
    });

    let total_entries = entries.len();
    let start = offset.min(total_entries);
    let end = start.saturating_add(limit).min(total_entries);

    // 与 read_file 同理：先在 8 KB 预算内挑出真正写得出的行，`next_offset` 只推进到那里。
    // 旧实现无条件按 limit 算下一页（如 offset=200），而预算只装得下前 ~120 行，
    // 中间那批条目对模型**静默消失**。
    let mut body = String::new();
    body.push_str("TYPE       PERMISSIONS     SIZE NAME\n");
    let mut emitted = 0usize;
    for entry in &entries[start..end] {
        let line = format!(
            "{:<10} {} {:>8} {}\n",
            entry.kind, entry.permissions_text, entry.size, entry.name
        );
        if body.len() + line.len() > MAX_LIST_BYTES {
            if emitted == 0 {
                // 单行就超预算（异常长的文件名）：至少截一段出去，保证 offset 能前进
                body.push_str(&truncate_output(
                    line,
                    MAX_LIST_BYTES.saturating_sub(body.len()),
                ));
                emitted = 1;
            }
            break;
        }
        body.push_str(&line);
        emitted += 1;
    }

    let page_entries = entries[start..start + emitted].to_vec();
    let next_offset = (start + emitted < total_entries).then_some(start + emitted);
    if emitted == 0 {
        // 空目录 / offset 越界：body 只剩表头，得说一句，别让模型去猜是哪种
        if total_entries == 0 {
            body.push_str("[directory is empty]\n");
        } else {
            body.push_str(&format!(
                "[offset {} is past the end of the listing ({} entries)]\n",
                start, total_entries
            ));
        }
    }
    if let Some(next) = next_offset {
        body.push_str(&format!(
            "\n[next: call list_directory with offset={} to continue]",
            next
        ));
    }

    DirectoryView {
        body,
        total_entries,
        returned_entries: page_entries.len(),
        offset: start,
        limit,
        next_offset,
        entries: page_entries,
    }
}

fn directory_kind_rank(kind: &str) -> u8 {
    match kind {
        "directory" => 0,
        "file" => 1,
        "symlink" => 2,
        _ => 3,
    }
}

pub(crate) fn directory_entries_metadata(entries: &[DirectoryEntryView]) -> Vec<serde_json::Value> {
    entries
        .iter()
        .map(|entry| {
            json!({
                "name": &entry.name,
                "kind": &entry.kind,
                "size": entry.size,
                "permissions": entry.permissions,
                "permissions_text": &entry.permissions_text
            })
        })
        .collect()
}

// ────────────────────────────── tests ──────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tools::base64;

    // ── edit display metadata ──

    #[test]
    fn match_byte_positions_non_overlapping() {
        let positions = match_byte_positions("aa aa aa", "aa");
        assert_eq!(positions, vec![0, 3, 6]);
    }

    #[test]
    fn build_metadata_small_file_has_before_after() {
        let current = "x\nfoo\ny\nfoo\nz\n";
        let (updated, n) = try_replace(current, "foo", "bar", true).unwrap();
        assert_eq!(n, 2);
        let meta = build_edit_display_metadata("/t", current, &updated, "foo", "bar", n);
        assert_eq!(meta["occurrences"], 2);
        assert_eq!(meta["before"], current);
        assert_eq!(meta["after"], updated);
        let lines = meta["match_line_positions"].as_array().unwrap();
        assert_eq!(lines.len(), 2);
        assert!(meta.get("hunks").is_none());
    }

    #[test]
    fn build_metadata_large_file_emits_hunks() {
        let pad = "line\n".repeat(4000); // ~20k
        let current = format!("{pad}TARGET\nmiddle\nTARGET\n{pad}");
        let (updated, n) = try_replace(&current, "TARGET", "DONE", true).unwrap();
        assert_eq!(n, 2);
        assert!(current.len() > EDIT_DISPLAY_MAX_BYTES);
        let meta = build_edit_display_metadata("/big", &current, &updated, "TARGET", "DONE", n);
        let hunks = meta["hunks"].as_array().expect("hunks");
        assert_eq!(hunks.len(), 2);
        assert!(hunks[0]["before"].as_str().unwrap().contains("TARGET"));
        assert!(hunks[0]["after"].as_str().unwrap().contains("DONE"));
        assert!(meta.get("before").is_none());
    }

    // ── try_replace tests ──

    #[test]
    fn try_replace_single_occurrence() {
        let result = try_replace("hello world", "world", "there", false);
        assert!(result.is_ok());
        let (updated, occurrences) = result.unwrap();
        assert_eq!(updated, "hello there");
        assert_eq!(occurrences, 1);
    }

    #[test]
    fn try_replace_all() {
        let result = try_replace("a a a", "a", "b", true);
        let (updated, occurrences) = result.unwrap();
        assert_eq!(updated, "b b b");
        assert_eq!(occurrences, 3);
    }

    #[test]
    fn try_replace_not_found() {
        let result = try_replace("hello", "world", "x", false);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[test]
    fn try_replace_multiple_without_replace_all() {
        let result = try_replace("hello hello", "hello", "x", false);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("matches 2 times"));
    }

    #[test]
    fn try_replace_result_too_large() {
        // File is just under the limit, replacing a small sentinel pushes it over
        let sentinel = "START";
        let pad = "a".repeat((MAX_FILE_WRITE_BYTES - 20).max(1));
        let current = format!("{}{}END", sentinel, pad);
        assert!(current.len() < MAX_FILE_WRITE_BYTES);
        let result = try_replace(&current, sentinel, &"b".repeat(30), false);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("exceeds size limit"));
    }

    #[test]
    fn try_replace_new_content_can_be_empty() {
        let result = try_replace("hello world", " world", "", false);
        let (updated, occurrences) = result.unwrap();
        assert_eq!(updated, "hello");
        assert_eq!(occurrences, 1);
    }

    // ── parse_edit_params tests ──

    fn edit_json(path: &str, old: &str, new: &str) -> serde_json::Value {
        json!({"path": path, "old_content": old, "new_content": new})
    }

    #[test]
    fn parse_edit_params_all_fields() {
        let params = edit_json("/a.txt", "old", "new");
        let p = parse_edit_params(&params).unwrap();
        assert_eq!(p.path, "/a.txt");
        assert_eq!(p.old_content, "old");
        assert_eq!(p.new_content, "new");
        assert!(!p.replace_all);
    }

    #[test]
    fn parse_edit_params_replace_all_true() {
        let mut params = edit_json("/a.txt", "old", "new");
        params["replace_all"] = json!(true);
        let p = parse_edit_params(&params).unwrap();
        assert!(p.replace_all);
    }

    #[test]
    fn parse_edit_params_missing_path() {
        let params = json!({"old_content": "x", "new_content": "y"});
        assert!(parse_edit_params(&params).is_err());
    }

    #[test]
    fn parse_edit_params_missing_old_content() {
        let params = json!({"path": "/a", "new_content": "y"});
        assert!(parse_edit_params(&params).is_err());
    }

    #[test]
    fn parse_edit_params_empty_path() {
        let params = edit_json("", "old", "new");
        assert!(parse_edit_params(&params).is_err());
    }

    #[test]
    fn parse_edit_params_empty_old_content() {
        let params = edit_json("/a.txt", "", "new");
        assert!(parse_edit_params(&params).is_err());
    }

    // ── existing tests ──

    #[test]
    fn cmd_helpers_quote_path() {
        let c = base64::cmd_encode_file("'/etc/foo bar'");
        assert!(c.contains("'/etc/foo bar'"));
        let w = base64::cmd_decode_to_file("'/tmp/x'", "AAAA");
        assert!(w.contains("MARCEL_B64_EOF"));
        assert!(w.contains("AAAA"));
    }

    #[test]
    fn build_read_view_adds_line_numbers_and_next_hint() {
        let bytes = b"one\ntwo\nthree\nfour\n";
        let view = build_read_view(bytes, 2, 2, true);

        assert_eq!(view.total_lines, 4);
        assert_eq!(view.start_line, 2);
        assert_eq!(view.end_line, 3);
        assert_eq!(view.returned_lines, 2);
        assert_eq!(view.next_line, Some(4));
        assert!(view.truncated);
        assert!(view.body.contains("     2: two"), "{}", view.body);
        assert!(view.body.contains("     3: three"), "{}", view.body);
        assert!(view.body.contains("start_line=4"), "{}", view.body);
    }

    #[test]
    fn build_read_view_reports_lossy_utf8() {
        let bytes = [0xff, b'\n', b'o', b'k'];
        let view = build_read_view(&bytes, 1, 10, true);

        assert!(view.lossy_utf8);
        assert!(view.body.contains("non-UTF-8"), "{}", view.body);
    }

    #[test]
    fn build_directory_view_sorts_directories_first_and_paginates() {
        let entries = vec![
            DirectoryEntryView {
                name: "z.txt".to_string(),
                kind: "file".to_string(),
                size: 10,
                permissions: 0o100644,
                permissions_text: format_permissions(0o100644),
            },
            DirectoryEntryView {
                name: "app".to_string(),
                kind: "directory".to_string(),
                size: 0,
                permissions: 0o040755,
                permissions_text: format_permissions(0o040755),
            },
            DirectoryEntryView {
                name: "a.txt".to_string(),
                kind: "file".to_string(),
                size: 1,
                permissions: 0o100644,
                permissions_text: format_permissions(0o100644),
            },
        ];

        let view = build_directory_view(entries, 0, 2, DirectorySortBy::Name);

        assert_eq!(view.total_entries, 3);
        assert_eq!(view.returned_entries, 2);
        assert_eq!(view.next_offset, Some(2));
        assert_eq!(view.entries[0].name, "app");
        assert_eq!(view.entries[1].name, "a.txt");
        assert!(view.body.contains("TYPE"), "{}", view.body);
        assert!(view.body.contains("offset=2"), "{}", view.body);
    }

    #[test]
    fn directory_entries_metadata_contains_structured_entries() {
        let entries = vec![DirectoryEntryView {
            name: "file.txt".to_string(),
            kind: "file".to_string(),
            size: 42,
            permissions: 0o100644,
            permissions_text: format_permissions(0o100644),
        }];

        let metadata = directory_entries_metadata(&entries);

        assert_eq!(metadata[0]["name"], "file.txt");
        assert_eq!(metadata[0]["kind"], "file");
        assert_eq!(metadata[0]["size"], 42);
        assert_eq!(metadata[0]["permissions_text"], "rw-r--r--");
    }

    // ── 读取上限预检 ──

    #[test]
    fn read_size_limit_error_accepts_small_and_rejects_large() {
        assert!(read_size_limit_error("/var/log/app.log", 0).is_none());
        assert!(read_size_limit_error("/var/log/app.log", MAX_READ_FILE_BYTES).is_none());

        let err = read_size_limit_error("/var/log/app.log", MAX_READ_FILE_BYTES + 1).unwrap();
        assert!(err.contains("/var/log/app.log"), "{}", err);
        assert!(err.contains("bash"), "{}", err);
        assert!(err.contains("head -n 200"), "{}", err);
    }

    // ── sidecar 路径 ──

    #[test]
    fn sidecar_path_stays_next_to_target() {
        // 与人类编辑器同款：临时文件与目标同目录，rename 才是同文件系统的原子替换
        let sidecar = crate::commands::sftp::remote_sidecar_path("./notes.txt", "edit").unwrap();
        assert!(
            sidecar.starts_with("./.notes.txt.marcel-edit-"),
            "{}",
            sidecar
        );

        let abs =
            crate::commands::sftp::remote_sidecar_path("/etc/ssh/sshd_config", "edit").unwrap();
        assert!(
            abs.starts_with("/etc/ssh/.sshd_config.marcel-edit-"),
            "{}",
            abs
        );

        // 裸相对名（无 '/'）算不出 sidecar → 调用点先用 with_dot_slash 补上
        assert!(crate::commands::sftp::remote_sidecar_path("notes.txt", "edit").is_err());
        assert!(
            crate::commands::sftp::remote_sidecar_path(&with_dot_slash("notes.txt"), "edit")
                .is_ok()
        );
    }

    #[test]
    fn with_dot_slash_normalizes_relative_names() {
        assert_eq!(with_dot_slash("notes.txt"), "./notes.txt");
        assert_eq!(with_dot_slash("./notes.txt"), "./notes.txt");
        assert_eq!(with_dot_slash("/etc/foo"), "/etc/foo");
        assert_eq!(with_dot_slash("sub/foo.txt"), "sub/foo.txt");
    }

    // ── CRLF / LF 匹配 ──

    #[test]
    fn resolve_edit_text_prefers_exact_match() {
        // 模型给的 old 本身就是 CRLF → 原样匹配，不做任何转换
        let current = "a\r\nfoo\r\nb\r\n";
        let r = resolve_edit_text(current, "foo\r\n", "bar\r\n").unwrap();
        assert_eq!(r.old, "foo\r\n");
        assert_eq!(r.new, "bar\r\n");
        assert!(!r.line_ending_converted);
    }

    #[test]
    fn resolve_edit_text_falls_back_to_crlf_for_lf_old() {
        // read_file 用 str::lines() 分行，CRLF 的 \r 在给模型看之前就没了：
        // 模型照抄的 old_content 只有 LF，必须能命中 CRLF 文件
        let current = "a\r\nfoo\r\nb\r\n";
        let r = resolve_edit_text(current, "a\nfoo\n", "x\n").unwrap();
        assert!(r.line_ending_converted);
        assert_eq!(r.old, "a\r\nfoo\r\n");
        // new_content 一并转换，写回去不会一半 CRLF 一半 LF
        assert_eq!(r.new, "x\r\n");

        let (updated, n) = try_replace(current, "a\nfoo\n", "x\n", false).unwrap();
        assert_eq!(n, 1);
        assert_eq!(updated, "x\r\nb\r\n");
    }

    #[test]
    fn try_replace_matches_crlf_old_in_lf_file() {
        let current = "a\nfoo\nb\n";
        let (updated, n) = try_replace(current, "a\r\nfoo\r\n", "x\r\ny\r\n", false).unwrap();
        assert_eq!(n, 1);
        assert_eq!(updated, "x\ny\nb\n");
    }

    #[test]
    fn try_replace_all_after_eol_conversion_counts_all() {
        let current = "foo\r\nmid\r\nfoo\r\n";
        let (updated, n) = try_replace(current, "foo\n", "bar\n", true).unwrap();
        assert_eq!(n, 2);
        assert_eq!(updated, "bar\r\nmid\r\nbar\r\n");

        // 多处匹配仍按转换后的候选计数并拒绝
        let err = try_replace(current, "foo\n", "bar\n", false).unwrap_err();
        assert!(err.contains("matches 2 times"), "{}", err);
    }

    #[test]
    fn try_replace_still_reports_missing_content() {
        let err = try_replace("hello\nworld\n", "world\n\n", "x\n", false).unwrap_err();
        assert!(err.contains("not found"), "{}", err);
        // 无换行的 old 不受行尾转换影响
        assert!(try_replace("hello", "HELLO", "x", false).is_err());
    }

    // ── 字节预算下的续读指针 ──

    #[test]
    fn build_read_view_retargets_next_line_when_byte_budget_cuts_page() {
        // 300 行 × ~100 字节 ≈ 30 KB，一页 200 行远超 16 KB 输出预算
        let mut src = String::new();
        for i in 1..=300 {
            src.push_str(&format!("line{}-{}\n", i, "x".repeat(90)));
        }

        let view = build_read_view(src.as_bytes(), 1, 200, true);

        assert!(
            view.returned_lines > 0 && view.returned_lines < 200,
            "returned {}",
            view.returned_lines
        );
        assert_eq!(view.end_line, view.returned_lines);
        // 指针必须指向第一条没整行写出的行，而不是请求页的末尾（否则中间的行静默丢失）
        assert_eq!(view.next_line, Some(view.returned_lines + 1));
        assert!(view.truncated);
        assert!(view.body.contains(&format!(
            "{:>6}: line{}",
            view.returned_lines, view.returned_lines
        )));
        assert!(!view.body.contains(&format!(
            "{:>6}: line{}",
            view.returned_lines + 1,
            view.returned_lines + 1
        )));

        // 从指针处续读，第一条就是紧接着的下一行（首尾相接，不跳段）
        let next = view.next_line.unwrap();
        let cont = build_read_view(src.as_bytes(), next, 3, true);
        assert_eq!(cont.start_line, next);
        assert!(
            cont.body.contains(&format!("line{}", next)),
            "{}",
            cont.body
        );
    }

    #[test]
    fn build_read_view_marks_cut_single_long_line_and_advances() {
        let mut src = "a".repeat(30_000);
        src.push_str("\nsecond\n");

        let view = build_read_view(src.as_bytes(), 1, 10, true);

        assert_eq!(view.total_lines, 2);
        assert_eq!(view.returned_lines, 1);
        assert_eq!(view.end_line, 1);
        // 长行被切在预算处，并且**明说**后半段要用 bash 取
        assert!(view.body.contains("only its first"), "{}", view.body);
        assert!(view.body.contains("30000 bytes"), "{}", view.body);
        assert!(view.truncated);
        // 指针绕过这一行，避免"再读一次还是同一行"的死循环
        assert_eq!(view.next_line, Some(2));
        assert!(
            view.body.len() < MAX_READ_BYTES + 300,
            "{}",
            view.body.len()
        );

        let cont = build_read_view(src.as_bytes(), 2, 10, true);
        assert!(cont.body.contains("second"), "{}", cont.body);
    }

    #[test]
    fn build_read_view_explains_empty_and_past_eof() {
        let empty = build_read_view(b"", 1, 10, true);
        assert_eq!(empty.total_lines, 0);
        assert_eq!(empty.returned_lines, 0);
        assert_eq!(empty.next_line, None);
        assert!(!empty.truncated);
        assert!(empty.body.contains("file is empty"), "{}", empty.body);

        // 起点越界：不能只给一段空体让模型以为文件是空的
        let past = build_read_view(b"a\nb\nc\n", 99, 10, true);
        assert_eq!(past.total_lines, 3);
        assert_eq!(past.returned_lines, 0);
        assert_eq!(past.end_line, 3);
        assert_eq!(past.next_line, None);
        assert!(
            past.body.contains("past the end of the file"),
            "{}",
            past.body
        );
        assert!(past.body.contains("99"), "{}", past.body);
    }

    fn dir_entries(count: usize, name_len: usize) -> Vec<DirectoryEntryView> {
        (0..count)
            .map(|i| DirectoryEntryView {
                name: format!("file-{:04}-{}", i, "n".repeat(name_len)),
                kind: "file".to_string(),
                size: 1,
                permissions: 0o100644,
                permissions_text: format_permissions(0o100644),
            })
            .collect()
    }

    #[test]
    fn build_directory_view_retargets_next_offset_when_byte_budget_cuts_page() {
        let view = build_directory_view(dir_entries(300, 40), 0, 200, DirectorySortBy::Name);

        assert!(
            view.returned_entries > 0 && view.returned_entries < 200,
            "returned {}",
            view.returned_entries
        );
        assert_eq!(view.entries.len(), view.returned_entries);
        // 指针只推进到真正写出的条目数，而不是请求页的末尾
        assert_eq!(view.next_offset, Some(view.returned_entries));
        assert!(view
            .body
            .contains(&format!("offset={}", view.returned_entries)));
        let last = view.entries.last().unwrap();
        assert!(view.body.contains(&last.name), "{}", view.body);
        assert!(!view
            .body
            .contains(&format!("file-{:04}-", view.returned_entries)));

        // 续读页的第一条正好是没写出的那一条（不跳段）
        let next = build_directory_view(
            dir_entries(300, 40),
            view.next_offset.unwrap(),
            5,
            DirectorySortBy::Name,
        );
        assert_eq!(next.offset, view.returned_entries);
        assert_eq!(
            next.entries[0].name,
            format!("file-{:04}-{}", view.returned_entries, "n".repeat(40))
        );
    }
}
