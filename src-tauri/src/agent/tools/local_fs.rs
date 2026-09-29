//! 本机文件系统原语：整读 / 原子整写 / 列目录。
//!
//! 与 [`crate::agent::tools::file_ops::FileBackend`] 的关系：本文件只提供本机
//! 实现所需的**最小原语 + 路径安全**，不实现该 trait —— 这里的 `read` 多回一个
//! `has_bom`、`write` 多收一个 `with_bom`。BOM 是原文件字节的一部分（读端剥掉、
//! 写端还原，见 `commands::sftp::sftp_read_file` 的 `ReadFileResult.has_bom`）：
//! 不带上它，一次「打开-保存」就会静默改掉文件头字节。接入方按 `FileBackend`
//! 的语义调用这三个函数即可。
//!
//! 三条硬纪律：
//! 1. **路径安全整套复用** `sftp_transfer` 里那套，不另写判定：
//!    `util::validate_local_path`（第一道）→ `reject_non_disk_prefix` →
//!    `resolve_against_ancestors` → `blacklisted`。读方向也查黑名单（`~/.ssh`
//!    这类目录里的密钥不能读）；写方向用**原始**路径先判符号链接叶子，再
//!    resolve / 查黑名单（顺序照 `validate_local_download_path`，那是修过的
//!    正确顺序）；`ensure_parent_creatable` 内部先 resolve 再建目录，顺序不能反。
//! 2. 写一律**原子替换**：同目录唯一 tmp（pid + 进程内序号）+ fsync + rename；
//!    目标已存在时先把原文件挪成 `.backup`，替换失败要能滚回来（形状照
//!    `config::persist::atomic_write` 与 `commands::sftp::stream_download_single_file`）。
//!    覆盖已存在文件还要**还原原权限位**：tmp 是新建文件（umask 默认权限），
//!    rename 之后目标会继承它——0600 的密钥/凭据文件被悄悄放宽成 0644 是安全
//!    回归。远端同类路径也专门保了这件事（`commands::sftp::commit_remote_temp_file`
//!    → `restore_remote_permissions`）。
//! 3. 读只接受**普通文件**（`is_file`）：目录、设备、FIFO、套接字一律拒绝。
//!    设备/FIFO 的 `len()` 恒为 0，只挡 `is_dir` 会让 `/dev/zero` 或用户目录里的
//!    FIFO 一路走到 `read_to_end`：前者无界增长打穿内存，后者在 open 上永久阻塞。

use std::path::{Path, PathBuf};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::agent::tools::file_ops::{EntryKind, RawEntry, MAX_READ_FILE_BYTES};
use crate::agent::tools::sftp_transfer::{
    blacklisted, ensure_parent_creatable, reject_non_disk_prefix, resolve_against_ancestors,
    validate_file_name, LocalPathPolicy,
};
use crate::commands::sftp::has_utf8_bom;
use crate::error::AppError;

// ────────────────────────────── 错误文案 ──────────────────────────────

/// `AppError` → 面向用户的单段中文文案。
///
/// 校验器（`util::validate_local_path` / `sftp_transfer` 那几个）把原因写在
/// `AppError::Ssh` / `Agent` 的文案里，`to_string()` 会套一层 "SSH error: " ——
/// 模型看到的应该是「路径不能为空」本身，不是两层壳。
fn app_error_text(err: AppError) -> String {
    match err {
        AppError::Ssh(m) | AppError::Agent(m) => m,
        other => other.to_string(),
    }
}

/// 命中黑名单的统一文案：必须点明是「受保护目录」，模型才知道该换路径而不是重试。
///
/// 展示用**调用方给的原始 path**，不用 `resolved`：Windows 上 `canonicalize`
/// 出来的是 `\\?\C:\...` verbatim 形态，它不是一条能直接喂给 PowerShell
/// 的路径，模型照抄过去只会再撞一次墙。
fn protected_location_error(display_path: &str) -> String {
    format!(
        "{display_path} 位于受保护的系统/敏感目录下（如 ~/.ssh、~/.gnupg、系统目录等），拒绝读写。\
         请改用普通用户目录下的路径（例如用户主目录或下载目录）。"
    )
}

/// 非普通文件的拒绝文案（目录单独指路本机的 `local_list_directory`，不要把它
/// 引到远端的 `list_directory` 上去——那列的是服务器目录）。
fn non_regular_file_error(display_path: &str) -> String {
    format!(
        "{display_path} 不是普通文件（设备、命名管道 FIFO、套接字等一律拒绝整读：\
         它们既没有可预检的大小，读起来要么无界增长要么永久阻塞）。\
         目录请用 local_list_directory；需要读设备/FIFO 的原始数据请改用 local_bash 里的分段命令。"
    )
}

/// [`ensure_parent_creatable`] 的失败是多段英文（`local path resolution failed` /
/// `local mkdir failed`），直接透给模型不符合「面向模型的中文可行动文案」这条纪律。
/// 这里包一层中文：能识别的三种底层原因给出中文说明，认不出的把原文挂在括号里，
/// **不修改** `sftp_transfer` 里的实现（那边是上传/下载共用的，不该为本机改写）。
fn parent_creatable_error_text(display_path: &str, raw: &str) -> String {
    let detail = if raw.contains("protected system location") {
        "该位置位于受保护的系统/敏感目录下".to_string()
    } else if let Some(rest) = raw.strip_prefix("local mkdir failed: ") {
        format!("创建目录失败：{rest}")
    } else if let Some(rest) = raw.strip_prefix("local path resolution failed: ") {
        format!("路径解析失败：{rest}")
    } else {
        raw.to_string()
    };
    format!(
        "无法准备 {display_path} 的上级目录（{detail}）。\
         请确认父目录存在且可写，或在普通用户目录下换一个路径。"
    )
}

fn mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_048_576.0)
}

/// 整读上限的失败文案（本机版）。
///
/// 与远端同一条纪律（`file_ops::MAX_READ_FILE_BYTES`，超限不截断、直接失败），
/// 但指路不同：远端指路 `bash` 的分段读，本机没有远端 shell，改用本机的
/// `local_bash`（Windows 用 PowerShell 的 Get-Content / Select-String）。
fn read_size_limit_error_local(path: &str, size: u64) -> Option<String> {
    (size > MAX_READ_FILE_BYTES).then(|| {
        format!(
            "{path}: 文件大小 {size}（整读上限 {limit}）；拒绝把整个文件读进内存。\
             本机没有远端 shell，请用 local_bash 工具分段读，例如 Windows（PowerShell）：\
             `Get-Content '{path}' -TotalCount 200`、\
             `Get-Content '{path}' | Select-Object -Skip 1000 -First 200`，\
             或用 `Select-String -Path '{path}' -Pattern '关键词'` 定位需要的段落；\
             Linux/macOS 同理用 head -n / sed -n / grep -n。",
            path = path,
            size = mb(size),
            limit = mb(MAX_READ_FILE_BYTES),
        )
    })
}

/// 按 `with_bom` 还原 UTF-8 BOM（复用 [`has_utf8_bom`] 判定，避免重复 BOM）。
///
/// 语义与 `commands::sftp::encode_file_content` 一致（读端剥、写端还原），只是
/// 这里是字节层：调用方给的是二进制内容而非 `&str`。调用方若把读出来的字节
/// 原样写回并带上原文件的 `has_bom`，字节数完全对齐。
fn encode_with_bom(bytes: &[u8], with_bom: bool) -> Vec<u8> {
    if !with_bom || has_utf8_bom(bytes) {
        return bytes.to_vec();
    }
    let mut out = Vec::with_capacity(bytes.len() + 3);
    out.extend_from_slice(b"\xEF\xBB\xBF");
    out.extend_from_slice(bytes);
    out
}

// ─────────────────────────────── 读 ───────────────────────────────

/// 读本机文件。返回 `(字节, 原文件是否带 UTF-8 BOM)`，BOM 已从字节里剥掉。
///
/// 错误文案是面向模型/用户的：不存在、目录、超限、受保护目录各有明确说法。
pub(crate) async fn read(path: &str) -> Result<(Vec<u8>, bool), String> {
    read_with_policy(path, &LocalPathPolicy::default_policy()).await
}

/// [`read`] 的可注入 policy 版本（单测用 `from_blacklist` 造受保护目录，
/// 不依赖真实 home / 系统目录）。
async fn read_with_policy(path: &str, policy: &LocalPathPolicy) -> Result<(Vec<u8>, bool), String> {
    // 第一道：空路径 / NUL / `..` 组件（与人类侧 attachment 同一条校验）。
    let raw = PathBuf::from(crate::util::validate_local_path(path).map_err(app_error_text)?);
    // UNC / verbatim / 设备命名空间：本机语义只覆盖盘符路径（与 upload/download 一致）。
    reject_non_disk_prefix(&raw).map_err(app_error_text)?;

    // 已存在的叶子会被 canonicalize（符号链接被解析成真实目标），所以黑名单
    // 在 resolve **之后**查：`~/.ssh` 被链接指向时照样命中。读方向也必须查
    // ——密钥与凭据不允许被读。
    let resolved = resolve_against_ancestors(&raw).map_err(app_error_text)?;
    if blacklisted(&resolved, &policy.blacklist) {
        return Err(protected_location_error(path));
    }

    // 类型判定看**条目本身**（`symlink_metadata` / lstat，不跟随符号链接）：
    // resolve 对已存在的叶子做过 canonicalize，正常链接已指向真实目标；这一道
    // 同时挡住「断链」与「链接到设备/FIFO」——必须用 lstat 是因为这一步发生在
    // `File::open` 之前，而 FIFO 的 open 会一直等到有写端（阻塞一个阻塞线程池
    // 线程直到工具超时）。只读普通文件，不再只是「非目录」。
    let link_meta = tokio::fs::symlink_metadata(&resolved).await.map_err(|e| {
        format!(
            "本机文件无法读取：{path}（不存在，或没有读取权限；底层错误：{e}）。请确认这是本机（运行 Marcel SSH 的电脑）上存在的绝对路径。"
        )
    })?;
    if link_meta.is_dir() {
        return Err(format!(
            "{path} 是一个目录，不能按文件读取；列出内容请用 local_list_directory。"
        ));
    }
    if !link_meta.is_file() {
        return Err(non_regular_file_error(path));
    }
    if let Some(err) = read_size_limit_error_local(path, link_meta.len()) {
        return Err(err);
    }

    let file = tokio::fs::File::open(&resolved)
        .await
        .map_err(|e| format!("本机文件读取失败：{path}（{e}）"))?;
    // 句柄上的 fstat 再确认一次类型与大小：lstat 与 open 之间路径可能被换掉
    // （本地竞争），而句柄上的信息与随后读到的字节一定是同一个 inode。
    let meta = file
        .metadata()
        .await
        .map_err(|e| format!("本机文件读取失败：{path}（{e}）"))?;
    if !meta.is_file() {
        return Err(non_regular_file_error(path));
    }
    if let Some(err) = read_size_limit_error_local(path, meta.len()) {
        return Err(err);
    }

    // 有界读：上限 +1 字节，超限当场失败（与远端同一条纪律：不截断）。
    // `take` 把内存占用钉死在 2 MB 出头，fstat 之后文件若还在被追加也炸不掉内存。
    let mut data = Vec::new();
    file.take(MAX_READ_FILE_BYTES + 1)
        .read_to_end(&mut data)
        .await
        .map_err(|e| format!("本机文件读取失败：{path}（{e}）"))?;
    // 兜底：读到上限 +1 说明 fstat 之后文件长大了。
    if let Some(err) = read_size_limit_error_local(path, data.len() as u64) {
        return Err(err);
    }

    let has_bom = has_utf8_bom(&data);
    let bytes = if has_bom { data[3..].to_vec() } else { data };
    Ok((bytes, has_bom))
}

// ─────────────────────────────── 写 ───────────────────────────────

/// 原子写本机文件（覆盖已存在文件，失败回滚原文件）。
///
/// `with_bom` 由调用方按 [`read`] 回的 `has_bom` 传入：读过的文件原样写回时
/// 字节不变（见 [`encode_with_bom`]）。
pub(crate) async fn write(path: &str, bytes: &[u8], with_bom: bool) -> Result<(), String> {
    write_with_policy(path, bytes, with_bom, &LocalPathPolicy::default_policy()).await
}

/// [`write`] 的可注入 policy 版本（单测用）。
async fn write_with_policy(
    path: &str,
    bytes: &[u8],
    with_bom: bool,
    policy: &LocalPathPolicy,
) -> Result<(), String> {
    let raw = PathBuf::from(crate::util::validate_local_path(path).map_err(app_error_text)?);
    reject_non_disk_prefix(&raw).map_err(app_error_text)?;

    // 叶子名单独校验：`..` 之类已被上一道拦下，这里再挡 Windows 保留设备名
    // （CON/NUL/COM1...）与尾随空格/点（Win32 会悄悄去掉它们，落到别的文件名上）。
    let file_name = raw
        .file_name()
        .ok_or_else(|| {
            format!(
                "本机路径没有文件名（不能以分隔符结尾）：{}。请给出完整的目标文件路径。",
                raw.display()
            )
        })?
        .to_string_lossy()
        .to_string();
    validate_file_name(&file_name).map_err(|e| {
        format!(
            "文件名「{}」非法（{}）：只接受单个文件名，不能是 Windows 保留名\
             （CON/PRN/AUX/NUL/COM1-9/LPT1-9）、不能含非法字符或以空格/点结尾。",
            file_name,
            app_error_text(e)
        )
    })?;

    // 符号链接判定必须用**原始**路径、且在 resolve **之前**做：resolve 会把
    // 已存在的叶子 canonicalize 成链接目标（普通文件/目录），之后再
    // `symlink_metadata` 永远看不到链接本身。顺序照 `validate_local_download_path`
    // （sftp_transfer.rs 的「符号链接判定必须在 resolve 之前」那一段）。
    if let Ok(meta) = tokio::fs::symlink_metadata(&raw).await {
        if meta.file_type().is_symlink() {
            return Err(format!(
                "{} 是一个符号链接，拒绝写入/覆盖（先删除链接，或换一个普通文件路径）。",
                raw.display()
            ));
        }
        if meta.is_dir() {
            return Err(format!(
                "{} 是一个已存在的目录，拒绝覆盖目录；请换一个目标文件名。",
                raw.display()
            ));
        }
    }

    let resolved = resolve_against_ancestors(&raw).map_err(app_error_text)?;
    if blacklisted(&resolved, &policy.blacklist) {
        return Err(protected_location_error(path));
    }
    // 父目录：内部先 resolve 再建目录（顺序不能反，否则先建出来的目录逃过黑名单）。
    // 失败文案在 sftp_transfer 里是英文（上传/下载共用，那边不为本机改写），
    // 本机侧在这里包一层中文。
    ensure_parent_creatable(&resolved, policy)
        .await
        .map_err(|e| parent_creatable_error_text(path, &e))?;

    let data = encode_with_bom(bytes, with_bom);
    atomic_write_bytes(path, &resolved, &data).await
}

/// 原子写用的同目录临时/备份名序号（理由与 `config::persist::tmp_path_for`
/// 相同：同一路径的两次并发写若共用固定 `<name>.tmp`，会互相截断并抢同一个
/// rename 源，把一次成功保存误判成失败）。
static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `path` 的同目录唯一后缀名（pid 区分进程，进程内序号区分并发调用）。
fn sibling_with_suffix(path: &Path, ext: &str) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from("local-fs"));
    name.push(format!(
        ".marcel-local-{}.{}.{}",
        std::process::id(),
        TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        ext
    ));
    path.with_file_name(name)
}

fn tmp_path_for(path: &Path) -> PathBuf {
    sibling_with_suffix(path, "tmp")
}

fn backup_path_for(path: &Path) -> PathBuf {
    sibling_with_suffix(path, "backup")
}

/// 供文案展示的路径形态：摘掉 Windows 的 `\\?\` / `\\?\UNC\` verbatim 前缀
/// （`canonicalize` 的产物，模型照抄进 PowerShell / 资源管理器打不开）。
/// 只影响文案，不用于任何文件访问；调用方原始 `path` 在手时优先用它。
fn display_path(p: &Path) -> String {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        s.to_string()
    }
}

/// 尽力删除 tmp / backup 残留，失败只记日志。
///
/// Windows 上**只读文件删不掉**（`DeleteFile` 返回 ACCESS_DENIED），而备份文件
/// 恰恰可能是只读的——原文的 readonly 属性正是我们要还原的东西。先清只读位再删；
/// 这个文件本来就是垃圾，丢只读位没有副作用。（Unix 的删除权限由目录决定，
/// 不受文件权限位影响，这一支只是多一次失败重试。）
async fn remove_quietly(path: &Path) {
    let Err(first) = tokio::fs::remove_file(path).await else {
        return;
    };
    #[cfg(windows)]
    {
        // 这条 allow：Windows 的 `Permissions` 只承载 readonly 属性，
        // `set_readonly(false)` + 立刻 `set_permissions` 正是「临时清掉只读位
        // 再删」的正当用法。clippy 默认警的是 Unix 上误以为 `set_readonly(false)`
        // 能还原权限的写法，这里只在 Windows 分支、且马上就 set_permissions。
        #[allow(clippy::permissions_set_readonly_false)]
        if let Ok(meta) = tokio::fs::metadata(path).await {
            let mut perms = meta.permissions();
            if perms.readonly() {
                perms.set_readonly(false);
                if tokio::fs::set_permissions(path, perms).await.is_ok()
                    && tokio::fs::remove_file(path).await.is_ok()
                {
                    return;
                }
            }
        }
    }
    // NotFound 是常态（tmp 还没建出来就失败了），不值得刷屏。
    if first.kind() != std::io::ErrorKind::NotFound {
        log::debug!(
            "[local_fs] 清理临时文件失败 {}: {}",
            display_path(path),
            first
        );
    }
}

/// 测试专用失败注入：让「tmp → 目标」的 rename 对**指定目标路径**失败，用来覆盖
/// 备份/回滚分支（生产构建里这张表被 cfg 掉，[`injected_rename_failure`] 恒 false）。
/// 按目标路径而不是全局开关，避免并行跑的其它用例被误伤。
#[cfg(test)]
static INJECTED_RENAME_FAILURE: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

fn injected_rename_failure(path: &Path) -> bool {
    // 两个 cfg 块互为尾表达式：每个构建里只剩一个，都是一段求值为 bool 的块。
    #[cfg(test)]
    {
        INJECTED_RENAME_FAILURE
            .lock()
            .map(|guard| {
                guard.as_deref().is_some_and(|target| {
                    crate::agent::tools::sftp_transfer::path_key(target)
                        == crate::agent::tools::sftp_transfer::path_key(path)
                })
            })
            .unwrap_or(false)
    }
    #[cfg(not(test))]
    {
        let _ = path;
        false
    }
}

/// 「tmp → 目标」这一步 rename。单独成一个函数只为给回滚分支留注入点。
async fn rename_tmp_into_place(tmp: &Path, path: &Path) -> std::io::Result<()> {
    if injected_rename_failure(path) {
        return Err(std::io::Error::other("injected rename failure (test)"));
    }
    tokio::fs::rename(tmp, path).await
}

/// 原子替换：tmp（唯一名 + fsync）→ rename；目标已存在时先挪成 `.backup`，
/// 替换失败滚回来。形状照 `config::persist::atomic_write`（写 tmp、失败清 tmp、
/// Unix 顺带 sync 父目录）+ `commands::sftp::stream_download_single_file` 的
/// `.backup` 回滚段。
///
/// `caller_path` 只用于文案（调用方给的原始路径，见 [`display_path`] 的说明）。
async fn atomic_write_bytes(caller_path: &str, path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = tmp_path_for(path);
    if let Err(e) = write_tmp_and_sync(&tmp, bytes).await {
        remove_quietly(&tmp).await;
        return Err(format!(
            "本机文件写入失败：{caller_path}（{e}）。请确认目标目录可写、路径不是目录。"
        ));
    }

    // 覆盖已存在文件：先记下原权限位，替换成功后还原（见文件头第 2 条）。
    // `File::create(tmp)` 让 tmp 拿到 umask 默认权限（通常 0644），rename 之后
    // 目标继承的就是它——0600 的 .netrc/.pgpass 会被放宽、0755 的脚本会丢执行位。
    // Windows 上 `Permissions` 只承载 readonly 属性，`set_permissions` 就是
    // 设置/清除它。
    let preserve_perms = tokio::fs::metadata(path)
        .await
        .ok()
        .map(|m| m.permissions());
    let had_existing = preserve_perms.is_some();
    let backup = backup_path_for(path);
    if had_existing {
        remove_quietly(&backup).await;
        if let Err(e) = tokio::fs::rename(path, &backup).await {
            remove_quietly(&tmp).await;
            return Err(format!(
                "备份原文件失败（文件可能被其他程序占用）：{caller_path}（{e}）"
            ));
        }
    }

    if let Err(e) = rename_tmp_into_place(&tmp, path).await {
        if had_existing {
            // 「备份 → 替换」之间失败：目标此刻是空的，必须把原文件滚回来，
            // 否则用户原文件就被藏进了 .backup。重试一次（rename 失败常常是
            // 瞬时占用），仍失败就把备份路径写进错误并**保留备份不删**。
            // 注意滚回是 rename 原 inode 回去，权限位跟着原文件一起回来，
            // 不需要额外还原。
            let mut restored = tokio::fs::rename(&backup, path).await.is_ok();
            if !restored {
                restored = tokio::fs::rename(&backup, path).await.is_ok();
            }
            if !restored {
                remove_quietly(&tmp).await;
                return Err(format!(
                    "保存文件失败：{e}；原文件已备份到 {}，请手动改回原文件名。",
                    display_path(&backup)
                ));
            }
        }
        remove_quietly(&tmp).await;
        return Err(format!("保存文件失败：{caller_path}（{e}）"));
    }

    if let Some(perms) = preserve_perms {
        // 还原在 rename **之后**（形状与远端 `commit_remote_temp_file` →
        // `restore_remote_permissions` 对齐：先提交再 setstat）。失败只告警：
        // 内容已经写成功了，把一次成功保存报成失败会诱发调用方重试；而且这个
        // inode 是我们刚创建的、归自己所有，chmod 正常不会失败。
        if let Err(e) = tokio::fs::set_permissions(path, perms).await {
            log::warn!(
                "[local_fs] 还原本机文件权限失败 {}: {}",
                display_path(path),
                e
            );
        }
        // 备份是原文（Windows 上可能带 readonly 属性），交给 remove_quietly。
        remove_quietly(&backup).await;
    }

    // 目录项也要落盘，否则断电后可能出现「文件还在、内容丢了」的旧目录项。
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }

    Ok(())
}

async fn write_tmp_and_sync(tmp: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut f = tokio::fs::File::create(tmp).await?;
    f.write_all(bytes).await?;
    f.sync_all().await?;
    Ok(())
}

// ────────────────────────────── 列目录 ──────────────────────────────

/// 列出本机目录的原始条目（名字 / 类型 / 大小 / mode），**不跟随符号链接**
/// （链接条目本身就是 `EntryKind::Symlink`）。
pub(crate) async fn list(path: &str) -> Result<Vec<RawEntry>, String> {
    list_with_policy(path, &LocalPathPolicy::default_policy()).await
}

/// [`list`] 的可注入 policy 版本（单测用）。
async fn list_with_policy(path: &str, policy: &LocalPathPolicy) -> Result<Vec<RawEntry>, String> {
    let raw = PathBuf::from(crate::util::validate_local_path(path).map_err(app_error_text)?);
    reject_non_disk_prefix(&raw).map_err(app_error_text)?;
    let resolved = resolve_against_ancestors(&raw).map_err(app_error_text)?;
    if blacklisted(&resolved, &policy.blacklist) {
        return Err(protected_location_error(path));
    }

    let meta = tokio::fs::metadata(&resolved).await.map_err(|e| {
        format!(
            "本机目录无法读取：{path}（不存在，或没有读取权限；底层错误：{e}）。请确认这是本机（运行 Marcel SSH 的电脑）上存在的绝对路径。"
        )
    })?;
    if !meta.is_dir() {
        return Err(format!("{path} 不是目录，无法列出内容。"));
    }

    let dir =
        std::fs::read_dir(&resolved).map_err(|e| format!("本机目录读取失败：{path}（{e}）"))?;
    let mut entries = Vec::new();
    for entry in dir {
        let entry = entry.map_err(|e| format!("本机目录条目读取失败：{e}"))?;
        // 用 symlink_metadata（lstat）而不是 entry.metadata()/fs::metadata：
        // 类型判定必须看条目本身，不能跟到链接目标去。
        let meta = std::fs::symlink_metadata(entry.path()).map_err(|e| {
            format!(
                "本机目录条目 metadata 读取失败：{}（{}）",
                display_path(&entry.path()),
                e
            )
        })?;
        let ft = meta.file_type();
        let kind = if ft.is_symlink() {
            EntryKind::Symlink
        } else if ft.is_dir() {
            EntryKind::Dir
        } else {
            EntryKind::File
        };
        entries.push(RawEntry {
            name: entry.file_name().to_string_lossy().to_string(),
            kind,
            size: meta.len(),
            mode: mode_of(&meta),
        });
    }
    // 顺序保持文件系统给出的原始顺序：展示排序由上层（`file_ops` 的
    // DirectorySortBy）统一负责，后端之间不得有行为分歧。
    Ok(entries)
}

/// POSIX mode（含文件类型位，`format_permissions` 只取低 9 位，与 SFTP 后端同口径）。
#[cfg(unix)]
fn mode_of(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    meta.mode()
}

/// Windows 上 `MetadataExt::file_attributes()` 是属性位（只读/隐藏/系统），
/// 不是 POSIX mode：硬翻译会造出假权限串（把只读文件说成 r--r--r--）。这里按
/// 「后端给不出权限时为 0」的约定返回 0（展示层渲染成 `---------`，即未知权限），
/// 与 `file_ops::RawEntry::mode` 的语义一致。
#[cfg(not(unix))]
fn mode_of(_meta: &std::fs::Metadata) -> u32 {
    0
}

// ────────────────────────────────── tests ──────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// TempDir 在 Windows 落在 %LOCALAPPDATA% 下（默认黑名单内），成功路径必须
    /// 用无黑名单 policy，否则会被默认黑名单误拒（与 sftp_transfer 的测试同理）。
    fn no_bl() -> LocalPathPolicy {
        LocalPathPolicy::no_blacklist()
    }

    /// 覆盖写会落 `.marcel-local-*` 的 tmp / backup 兄弟文件；用例断言它们不残留。
    fn leftover_temp_names(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains(".marcel-local-"))
            .collect()
    }

    /// 造符号链接；平台/权限不允许时返回 false（用例跳过链接断言并打印原因）。
    fn try_symlink_file(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).is_ok()
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_file(target, link).is_ok()
        }
    }

    /// 造目录符号链接；平台/权限不允许时返回 false（用例跳过链接断言并打印原因）。
    fn try_symlink_dir(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).is_ok()
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_dir(target, link).is_ok()
        }
    }

    // ── 读 ──

    /// 只有**普通文件**能整读。断链（符号链接本身不是普通文件）走公开可移植的
    /// 触发方式；建不了链接（Windows 需开发者模式）就跳过。
    #[tokio::test]
    async fn read_rejects_non_regular_file_leaf() {
        let td = TempDir::new().unwrap();
        let link = td.path().join("dangling");
        if try_symlink_file(&td.path().join("never-created"), &link) {
            let err = read_with_policy(link.to_str().unwrap(), &no_bl())
                .await
                .unwrap_err();
            assert!(
                err.contains("不是普通文件"),
                "断链必须按非普通文件拒绝: {err}"
            );
        } else {
            eprintln!("skip: 本机无法创建符号链接（需开发者模式或管理员权限）");
        }
    }

    /// 旧行为保持：指向**普通文件**的符号链接仍然可以读——resolve 已把叶子
    /// canonicalize 成目标，新增的 `is_file` 门槛不该把正常链接一起误杀
    /// （写方向的「拒绝链接叶子」也只针对写，读方向的链接语义不变）。
    #[tokio::test]
    async fn read_follows_symlink_to_regular_file() {
        let td = TempDir::new().unwrap();
        let real = td.path().join("real.txt");
        std::fs::write(&real, b"through the link").unwrap();
        let link = td.path().join("link.txt");
        if !try_symlink_file(&real, &link) {
            eprintln!("skip: 本机无法创建符号链接（需开发者模式或管理员权限）");
            return;
        }
        let (bytes, has_bom) = read_with_policy(link.to_str().unwrap(), &no_bl())
            .await
            .unwrap();
        assert_eq!(bytes, b"through the link");
        assert!(!has_bom);
    }

    /// FIFO 与字符设备（unix）：修复前 `len()` 恒为 0，两道大小预检全部失效，
    /// `read_to_end` 在 `/dev/zero` 上无界增长、在 FIFO 上阻塞在 open。
    /// 两侧都加了超时：实现若退化成阻塞，测试是失败而不是挂死。
    #[cfg(unix)]
    #[tokio::test]
    async fn read_rejects_fifo_and_device_without_blocking() {
        let td = TempDir::new().unwrap();
        let fifo = td.path().join("pipe");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if made {
            let fifo_path = fifo.to_str().unwrap().to_string();
            let policy = no_bl();
            let got = tokio::time::timeout(std::time::Duration::from_secs(5), async move {
                read_with_policy(&fifo_path, &policy).await
            })
            .await
            .expect("FIFO 必须在 open 之前被拒（不能在 open 上永久阻塞）");
            let err = got.unwrap_err();
            assert!(err.contains("不是普通文件"), "FIFO 必须被拒: {err}");
        } else {
            eprintln!("skip: 本机没有 mkfifo");
        }

        if Path::new("/dev/zero").exists() {
            let got = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                read_with_policy("/dev/zero", &no_bl()),
            )
            .await
            .expect("/dev/zero 必须立刻被拒，不能真的去整读");
            let err = got.unwrap_err();
            assert!(err.contains("不是普通文件"), "/dev/zero 必须被拒: {err}");
        }
    }

    /// 超限文案与受保护文案必须展示**调用方原始路径**：Windows 上
    /// `canonicalize` 的 `\\?\C:\...` 形态模型会照抄进 PowerShell，打不开。
    #[tokio::test]
    async fn read_error_messages_show_caller_path_without_verbatim_prefix() {
        let td = TempDir::new().unwrap();
        let big = td.path().join("big.bin");
        std::fs::write(&big, vec![b'x'; (MAX_READ_FILE_BYTES + 1) as usize]).unwrap();
        let err = read_with_policy(big.to_str().unwrap(), &no_bl())
            .await
            .unwrap_err();
        assert!(
            err.contains(big.to_str().unwrap()),
            "超限文案必须原样回显调用方路径: {err}"
        );
        assert!(
            !err.contains(r"\\?\"),
            "文案里不得出现 verbatim 前缀（模型会照抄）: {err}"
        );

        let protected = td.path().join("protected-msg");
        std::fs::create_dir_all(&protected).unwrap();
        let secret = protected.join("id_rsa");
        std::fs::write(&secret, b"PRIVATE KEY").unwrap();
        let policy = LocalPathPolicy::from_blacklist(vec![protected]);
        let err = read_with_policy(secret.to_str().unwrap(), &policy)
            .await
            .unwrap_err();
        assert!(
            err.contains(secret.to_str().unwrap()),
            "受保护文案必须原样回显调用方路径: {err}"
        );
        assert!(
            !err.contains(r"\\?\"),
            "文案里不得出现 verbatim 前缀（模型会照抄）: {err}"
        );
    }

    #[tokio::test]
    async fn read_returns_file_bytes_without_bom() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("plain.txt");
        std::fs::write(&p, b"hello local fs").unwrap();
        let (bytes, has_bom) = read_with_policy(p.to_str().unwrap(), &no_bl())
            .await
            .unwrap();
        assert_eq!(bytes, b"hello local fs");
        assert!(!has_bom);
    }

    #[tokio::test]
    async fn read_strips_utf8_bom_and_reports_it() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("bom.txt");
        let mut content = b"\xEF\xBB\xBF".to_vec();
        content.extend_from_slice("你好".as_bytes());
        std::fs::write(&p, &content).unwrap();
        let (bytes, has_bom) = read_with_policy(p.to_str().unwrap(), &no_bl())
            .await
            .unwrap();
        assert!(has_bom, "带 BOM 的文件必须回 has_bom=true");
        assert_eq!(
            bytes,
            "你好".as_bytes(),
            "BOM 必须从返回字节里剥掉（编辑器不显示它）"
        );
    }

    #[tokio::test]
    async fn read_rejects_file_over_size_limit() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("big.bin");
        std::fs::write(&p, vec![b'x'; (MAX_READ_FILE_BYTES + 1) as usize]).unwrap();
        let err = read_with_policy(p.to_str().unwrap(), &no_bl())
            .await
            .unwrap_err();
        assert!(err.contains("整读上限"), "超限必须报清楚上限: {}", err);
        assert!(
            err.contains("local_bash") && err.contains("Get-Content"),
            "超限要指路本机分段读（local_bash / Get-Content）: {}",
            err
        );
    }

    #[tokio::test]
    async fn read_rejects_blacklisted_directory() {
        let td = TempDir::new().unwrap();
        let protected = td.path().join("protected");
        std::fs::create_dir_all(&protected).unwrap();
        let secret = protected.join("id_rsa");
        std::fs::write(&secret, b"PRIVATE KEY").unwrap();
        let policy = LocalPathPolicy::from_blacklist(vec![protected.clone()]);
        let err = read_with_policy(secret.to_str().unwrap(), &policy)
            .await
            .unwrap_err();
        assert!(
            err.contains("受保护"),
            "读方向命中黑名单也必须被拒（密钥不能读）: {}",
            err
        );
    }

    #[tokio::test]
    async fn read_reports_missing_file() {
        let td = TempDir::new().unwrap();
        let missing = td.path().join("nope.txt");
        let err = read_with_policy(missing.to_str().unwrap(), &no_bl())
            .await
            .unwrap_err();
        assert!(err.contains("不存在"), "文案要说清文件不存在: {}", err);
        assert!(err.contains("本机"), "文案要强调是本机路径: {}", err);
    }

    // ── 写 ──

    #[tokio::test]
    async fn write_creates_new_file_and_missing_parent_dirs() {
        let td = TempDir::new().unwrap();
        let target = td.path().join("sub/deeper/new.txt");
        write_with_policy(target.to_str().unwrap(), b"payload", false, &no_bl())
            .await
            .unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"payload");
        assert!(
            leftover_temp_names(&td.path().join("sub/deeper")).is_empty(),
            "成功路径不得留下 tmp / backup 残渣"
        );
    }

    #[tokio::test]
    async fn write_with_bom_prepends_bom_once() {
        let td = TempDir::new().unwrap();
        let target = td.path().join("bom.txt");
        write_with_policy(target.to_str().unwrap(), b"body", true, &no_bl())
            .await
            .unwrap();
        let raw = std::fs::read(&target).unwrap();
        assert_eq!(raw, b"\xEF\xBB\xBFbody");

        // 已是 BOM 内容 + with_bom=true：不得叠加成两个 BOM（读出来的字节
        // 原样写回是最常见的调用形态）。
        write_with_policy(target.to_str().unwrap(), &raw, true, &no_bl())
            .await
            .unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"\xEF\xBB\xBFbody");
    }

    #[tokio::test]
    async fn write_replaces_existing_file_and_cleans_backup() {
        let td = TempDir::new().unwrap();
        let target = td.path().join("exists.txt");
        std::fs::write(&target, b"old content").unwrap();
        write_with_policy(target.to_str().unwrap(), b"new content", false, &no_bl())
            .await
            .unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new content");
        assert!(
            leftover_temp_names(td.path()).is_empty(),
            "覆盖成功后 .backup 必须清理: {:?}",
            leftover_temp_names(td.path())
        );
    }

    /// 覆盖已存在文件必须保留原权限位：tmp 是新建文件（umask 默认权限），
    /// 不还原就会把 0600 的 .netrc/.pgpass 放宽成 0644、把 0755 的脚本降成 0644。
    #[cfg(unix)]
    #[tokio::test]
    async fn write_preserves_existing_file_mode() {
        use std::os::unix::fs::PermissionsExt;
        let td = TempDir::new().unwrap();
        for mode in [0o600u32, 0o755, 0o640] {
            let target = td.path().join(format!("mode-{mode:o}.sh"));
            std::fs::write(&target, b"old").unwrap();
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode)).unwrap();

            write_with_policy(target.to_str().unwrap(), b"new", false, &no_bl())
                .await
                .unwrap();
            let got = std::fs::metadata(&target).unwrap().permissions().mode() & 0o7777;
            assert_eq!(
                got, mode,
                "覆盖写必须保留原权限位（0o{mode:o}），实际 0o{got:o}"
            );
        }
        assert!(
            leftover_temp_names(td.path()).is_empty(),
            "保留权限的同时不得残留 tmp / backup"
        );
    }

    /// 新建文件不套用任何权限：与平台默认（同一目录里 `std::fs::write` 建出来的
    /// 文件）逐位相同，说明我们只还原「覆盖」场景的权限位，没动新文件的创建语义。
    #[cfg(unix)]
    #[tokio::test]
    async fn write_new_file_keeps_default_mode() {
        use std::os::unix::fs::PermissionsExt;
        let td = TempDir::new().unwrap();
        let reference = td.path().join("platform-default.txt");
        std::fs::write(&reference, b"x").unwrap();

        let target = td.path().join("fresh.txt");
        write_with_policy(target.to_str().unwrap(), b"x", false, &no_bl())
            .await
            .unwrap();

        let got = std::fs::metadata(&target).unwrap().permissions().mode() & 0o7777;
        let expected = std::fs::metadata(&reference).unwrap().permissions().mode() & 0o7777;
        assert_eq!(
            got, expected,
            "新建文件必须是平台默认权限（0o{expected:o}），实际 0o{got:o}"
        );
    }

    /// Windows 的 `Permissions` 只承载 readonly 属性：覆盖写要把它还原，
    /// 且**带 readonly 的备份文件**必须能删掉（`DeleteFile` 对只读文件报
    /// ACCESS_DENIED，得先清只读位），否则每次覆盖都留一个残渣文件。
    #[cfg(windows)]
    #[tokio::test]
    async fn write_preserves_readonly_attribute_and_cleans_backup() {
        let td = TempDir::new().unwrap();
        let target = td.path().join("readonly.txt");
        std::fs::write(&target, b"old").unwrap();
        let mut perms = std::fs::metadata(&target).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&target, perms).unwrap();

        write_with_policy(target.to_str().unwrap(), b"new", false, &no_bl())
            .await
            .unwrap();

        assert!(
            std::fs::metadata(&target).unwrap().permissions().readonly(),
            "只读属性必须还原（否则一次保存就把用户的只读保护悄悄撤销）"
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert!(
            leftover_temp_names(td.path()).is_empty(),
            "只读原文的备份也要清掉: {:?}",
            leftover_temp_names(td.path())
        );
    }

    // ── 写：符号链接的两条顺序 ──

    /// 叶子是符号链接（指向一个普通文件）：必须按**原始**路径判出链接并拒绝，
    /// 不能跟到目标去覆盖——resolve 会把链接 canonicalize 成目标，顺序反了这个
    /// 判定就是死代码（注释里自称「修过的顺序」）。
    #[tokio::test]
    async fn write_rejects_symlink_leaf() {
        let td = TempDir::new().unwrap();
        let real = td.path().join("real.txt");
        std::fs::write(&real, b"original").unwrap();
        let link = td.path().join("link.txt");
        if !try_symlink_file(&real, &link) {
            eprintln!("skip: 本机无法创建符号链接（需开发者模式或管理员权限）");
            return;
        }

        let err = write_with_policy(link.to_str().unwrap(), b"hacked", false, &no_bl())
            .await
            .unwrap_err();
        assert!(err.contains("符号链接"), "叶子链接必须被拒: {err}");
        assert_eq!(
            std::fs::read(&real).unwrap(),
            b"original",
            "拒绝时不得经由链接改到目标文件"
        );
        assert!(
            leftover_temp_names(td.path()).is_empty(),
            "拒绝路径不得落 tmp"
        );
    }

    /// 父目录是符号链接、指向黑名单目录：resolve 解析父目录后必须命中黑名单，
    /// 既不能写进去，也不能经由链接在受保护目录里建出新目录/文件。
    #[tokio::test]
    async fn write_rejects_symlinked_parent_into_blacklist() {
        let td = TempDir::new().unwrap();
        let protected = td.path().join("protected-dir");
        std::fs::create_dir_all(&protected).unwrap();
        let sneaky = td.path().join("sneaky");
        if !try_symlink_dir(&protected, &sneaky) {
            eprintln!("skip: 本机无法创建目录符号链接（需开发者模式或管理员权限）");
            return;
        }
        let policy = LocalPathPolicy::from_blacklist(vec![protected.clone()]);
        let target = sneaky.join("planted.txt");

        let err = write_with_policy(target.to_str().unwrap(), b"x", false, &policy)
            .await
            .unwrap_err();
        assert!(err.contains("受保护"), "链接进受保护目录必须被拒: {err}");
        assert!(
            !protected.join("planted.txt").exists(),
            "拒绝时不得经由链接写进受保护目录"
        );
        assert!(
            leftover_temp_names(&protected).is_empty() && leftover_temp_names(td.path()).is_empty(),
            "拒绝路径不得在受保护目录或链接目录落 tmp"
        );
    }

    /// 失败注入：备份已挪走、替换失败的那一刻，原文件必须滚回来、
    /// tmp / backup 都不得残留（回滚段此前零覆盖）。
    #[tokio::test]
    async fn write_rolls_back_original_when_replacement_fails() {
        let td = TempDir::new().unwrap();
        let target = td.path().join("important.txt");
        std::fs::write(&target, b"precious").unwrap();

        // 只对这一个目标注入「tmp → 目标」rename 失败（按路径匹配，不误伤并行用例）。
        // 注入值用 canonicalize 后的形态：`resolve_against_ancestors` 对已存在的
        // 叶子正是这么解析的（macOS 上 TempDir 在 /var 而 canonicalize 给出 /private/var）。
        *INJECTED_RENAME_FAILURE.lock().unwrap() = Some(std::fs::canonicalize(&target).unwrap());
        let err = write_with_policy(target.to_str().unwrap(), b"replacement", false, &no_bl())
            .await
            .unwrap_err();
        *INJECTED_RENAME_FAILURE.lock().unwrap() = None;

        assert!(err.contains("保存文件失败"), "必须报替换失败: {err}");
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"precious",
            "回滚后原内容必须原封不动"
        );
        assert!(
            leftover_temp_names(td.path()).is_empty(),
            "回滚后不得残留 tmp / backup: {:?}",
            leftover_temp_names(td.path())
        );
    }

    #[tokio::test]
    async fn write_rejects_directory_target() {
        let td = TempDir::new().unwrap();
        let dir = td.path().join("a_directory");
        std::fs::create_dir_all(&dir).unwrap();
        let err = write_with_policy(dir.to_str().unwrap(), b"x", false, &no_bl())
            .await
            .unwrap_err();
        assert!(err.contains("目录"), "拒绝覆盖目录: {}", err);
        assert!(dir.is_dir(), "拒绝时不得动那个目录");
    }

    #[tokio::test]
    async fn write_rejects_parentdir_and_separator_only_path() {
        let td = TempDir::new().unwrap();
        let sneaky = td.path().join("..").join("evil.txt");
        let err = write_with_policy(sneaky.to_str().unwrap(), b"x", false, &no_bl())
            .await
            .unwrap_err();
        assert!(err.contains(".."), "`..` 组件必须被拒: {}", err);

        // 只有分隔符、没有叶子名：「C:\」/「/」
        #[cfg(unix)]
        let root = "/";
        #[cfg(windows)]
        let root = r"C:\";
        let err = write_with_policy(root, b"x", false, &no_bl())
            .await
            .unwrap_err();
        assert!(err.contains("文件名"), "没有叶子名必须被拒: {}", err);
    }

    #[tokio::test]
    async fn write_rejects_blacklisted_directory_without_creating_it() {
        let td = TempDir::new().unwrap();
        let protected = td.path().join("protected-not-created");
        assert!(!protected.exists());
        let policy = LocalPathPolicy::from_blacklist(vec![protected.clone()]);
        let target = protected.join("nested/config");
        let err = write_with_policy(target.to_str().unwrap(), b"x", false, &policy)
            .await
            .unwrap_err();
        assert!(err.contains("受保护"), "受保护目录必须被拒: {}", err);
        assert!(!protected.exists(), "拒绝时不得替 agent 把受保护目录建出来");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn write_rejects_windows_reserved_leaf_name() {
        let td = TempDir::new().unwrap();
        let target = td.path().join("CON.txt");
        let err = write_with_policy(target.to_str().unwrap(), b"x", false, &no_bl())
            .await
            .unwrap_err();
        assert!(err.contains("非法"), "保留设备名必须被拒: {}", err);
    }

    // ── 列目录 ──

    #[tokio::test]
    async fn list_reports_dir_file_and_symlink_kinds() {
        let td = TempDir::new().unwrap();
        std::fs::create_dir_all(td.path().join("sub")).unwrap();
        std::fs::write(td.path().join("file.txt"), b"12345").unwrap();
        let link = td.path().join("link.txt");
        let has_link = try_symlink_file(&td.path().join("file.txt"), &link);

        let entries = list_with_policy(td.path().to_str().unwrap(), &no_bl())
            .await
            .unwrap();
        let kind_of = |name: &str| {
            entries
                .iter()
                .find(|e| e.name == name)
                .unwrap_or_else(|| panic!("entries 缺少 {}: {:?}", name, entries))
                .kind
        };
        assert_eq!(kind_of("sub"), EntryKind::Dir);
        assert_eq!(kind_of("file.txt"), EntryKind::File);
        assert_eq!(
            entries.iter().find(|e| e.name == "file.txt").unwrap().size,
            5,
            "条目大小来自元数据"
        );
        if has_link {
            assert_eq!(
                kind_of("link.txt"),
                EntryKind::Symlink,
                "符号链接条目必须报 Symlink（不跟随到目标文件）"
            );
        } else {
            eprintln!("skip: 本机无法创建符号链接（需开发者模式或管理员权限）");
        }
    }

    #[tokio::test]
    async fn list_rejects_blacklisted_directory() {
        let td = TempDir::new().unwrap();
        let protected = td.path().join("protected");
        std::fs::create_dir_all(&protected).unwrap();
        let policy = LocalPathPolicy::from_blacklist(vec![protected.clone()]);
        let err = list_with_policy(protected.to_str().unwrap(), &policy)
            .await
            .unwrap_err();
        assert!(err.contains("受保护"), "列目录也要查黑名单: {}", err);
    }

    #[tokio::test]
    async fn list_reports_non_directory_and_missing_path() {
        let td = TempDir::new().unwrap();
        let file = td.path().join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        let err = list_with_policy(file.to_str().unwrap(), &no_bl())
            .await
            .unwrap_err();
        assert!(err.contains("不是目录"), "{}", err);

        let missing = td.path().join("no-such-dir");
        let err = list_with_policy(missing.to_str().unwrap(), &no_bl())
            .await
            .unwrap_err();
        assert!(err.contains("无法读取"), "{}", err);
    }

    // ── path_key / blacklisted 的 Windows 形态归一（本文件用法下仍生效） ──

    /// 黑名单条目可能是 canonical 形态（`\\?\C:\...`，Windows）而待检路径是
    /// 普通形态（或反之）：`blacklisted` 必须仍然命中——否则本机读写能靠改形态
    /// 绕过 `~/.ssh` 这类保护。旁系目录不得误命中。
    #[test]
    fn blacklist_prefix_matches_across_canonical_forms() {
        let td = TempDir::new().unwrap();
        let protected = td.path().join("protected");
        std::fs::create_dir_all(&protected).unwrap();
        let canon = std::fs::canonicalize(&protected).unwrap();
        let bl = vec![canon.clone()];

        assert!(blacklisted(&canon.join("f.txt"), &bl));
        assert!(
            blacklisted(&protected.join("f.txt"), &bl),
            "canonical 与普通形态必须能对上（靠 path_key）"
        );

        let sibling = td.path().join("sibling");
        std::fs::create_dir_all(&sibling).unwrap();
        assert!(
            !blacklisted(&sibling.join("f.txt"), &bl),
            "旁系目录不得误命中"
        );

        #[cfg(windows)]
        assert_eq!(
            crate::agent::tools::sftp_transfer::path_key(&canon),
            crate::agent::tools::sftp_transfer::path_key(&protected),
            "Windows 上 canonical(\\\\?\\) 与普通形态必须归一成同一个键"
        );
    }
}
