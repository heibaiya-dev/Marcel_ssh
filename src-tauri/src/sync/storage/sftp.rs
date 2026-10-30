//! SFTP 存储适配器：把用户**自己的某台已保存服务器**当同步存储。
//!
//! ## 连接与安全边界
//!
//! - 连接配置复用 `commands::ssh::build_saved_connection_config`（`ssh_reconnect` 与 MCP 无头
//!   连接也走它）：密码 / 私钥口令只经 `keychain::get(&SecretSlot::…)` 取，私钥走私钥库，
//!   不经前端、不进日志；跳板机配置一并带上。
//! - 用 GUI 那份 `SshManager` 的 `connect_headless`，**主机密钥照常校验**：与已记录的指纹不一致
//!   就拒绝（`trust_new_host_key` 恒为 false，绝不自动信任新指纹），映射成
//!   [`StorageError::HostKey`]。首次连接未知主机的 TOFU 行为与应用里其它无头连接一致——这里选的
//!   本来就是一台用户已经连过、点过确认的已保存服务器。
//! - 连接**惰性建立**（第一次真正用到才连），断线自动重连一次，**空闲 2 分钟自动断开**（不占着
//!   服务器的会话名额；同步一轮才几秒，一分钟一轮之间完全可以断开重连）。
//!
//! ## SFTP 通道用法（见 AGENTS.md「SFTP 会话复用」）
//!
//! - 列目录 / 建目录 / 删除 / 改名 / 取家目录这类短操作走 `with_shared_sftp`（复用连接上缓存的
//!   元数据会话，失效透明重试一次）；
//! - 读写文件**正文**走 `open_sftp` 的独占通道——这条通道只属于本适配器（适配器内所有 get / put
//!   复用同一条，省掉每个文件三个往返），不和文件管理器抢共享会话，也不把大传输塞进共享会话。
//!
//! ## 原子写入
//!
//! `put` 先写同目录下的隐藏临时文件（`.<名字>.marcel-sync-<uuid>`），写完关闭再改名到目标。
//! 多数 SFTP 服务器（OpenSSH 的 v3 `rename`）**目标已存在时改名会失败**，所以覆盖写分两步兜底：
//! 先把旧文件挪成 sidecar、再改名新文件，改名失败就把旧文件挪回去——任何时刻都不会丢掉旧内容。
//! 中途崩溃最多残留以 `.` 开头的 sidecar，`list` 看不见它们，`delete` 目录时会连带清掉。
//!
//! ## 根目录
//!
//! `root` 是相对路径时相对登录用户的家目录（`realpath(".")`），支持 `~/x`、绝对路径。解析出的
//! 绝对路径通过 [`SftpStorage::resolved_root`] 暴露（运行时要把它加进 Agent 的受保护路径，
//! 防止 Agent 把同步目录删了）。

use std::collections::HashSet;
use std::future::Future;
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex as PlMutex;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::{OpenFlags, StatusCode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;
use tokio::time::Instant;

use super::{is_hidden_name, rel_segments, RemoteEntry, StorageError, SyncStorage};
use crate::config::connections::ConnectionStore;
use crate::data::JsonStore;
use crate::error::AppError;
use crate::ssh::connection::SshManager;
use crate::ssh::sftp_shared::session_unusable;

/// 空闲多久后断开连接。
pub const IDLE_DISCONNECT: Duration = Duration::from_secs(120);
/// 建立 SSH 连接（含握手、认证）的总时限。`connect_headless` 本身没有连接超时，不套这一层，
/// 连一台关机的服务器要等操作系统的 TCP 超时（Linux 上两分钟），同步引擎就卡在这一轮里。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

// ── 连接来源 ─────────────────────────────────────────────────────────────

/// 「按已保存连接 id 建一条无头 SSH 会话」。
///
/// 抽成 trait 是为了让适配器能对着内存里的 SFTP 服务器做端到端测试（测试实现直接登记一条回环
/// 会话）；生产实现是 [`SavedConnectionConnector`]。
#[async_trait]
pub trait SftpConnector: Send + Sync {
    /// 返回 `SshManager` 里已登记的会话 id。
    async fn connect(&self, connection_id: &str) -> Result<String, AppError>;
}

/// 生产实现：读连接表、复用 `build_saved_connection_config` 装配配置，再 `connect_headless`。
pub struct SavedConnectionConnector {
    ssh: SshManager,
    connections: JsonStore<ConnectionStore>,
}

impl SavedConnectionConnector {
    /// `ssh` 必须是 GUI 那份（known_hosts 同一份）；`connections` 是 `AppState.connection_store`。
    pub fn new(ssh: SshManager, connections: JsonStore<ConnectionStore>) -> Self {
        Self { ssh, connections }
    }
}

#[async_trait]
impl SftpConnector for SavedConnectionConnector {
    async fn connect(&self, connection_id: &str) -> Result<String, AppError> {
        // 先 clone 出来再放锁：`build_saved_connection_config` 会读密钥链，不该持着读锁做。
        let saved = {
            let store = self.connections.read().await;
            store.get_by_id(connection_id).cloned()
        }
        .ok_or_else(|| {
            AppError::Config(
                "同步用的服务器连接已经被删除了，请在同步设置里重新选择存储位置。".into(),
            )
        })?;
        let mut config =
            crate::commands::ssh::build_saved_connection_config(&saved, connection_id, false)?;
        // 同步用的无头会话**不带** connection_id：它不是用户打开的会话，不该被「按连接找会话」
        // 的逻辑（多机操控的目标解析、Agent 的会话选择）当成这台服务器的终端会话挑中。凭据早在
        // 上面按 connection_id 装配完了，这里清掉不影响认证。
        config.connection_id = None;
        self.ssh.connect_headless(config).await
    }
}

// ── 错误映射（纯函数，单测直接钉） ────────────────────────────────────────

/// 建连失败 → 存储错误。**不会**把任何凭据写进文案：`AppError` 本身就不含密码，这里只挑展示。
pub(crate) fn map_connect_error(e: AppError) -> StorageError {
    match e {
        AppError::HostKeyMismatch {
            host,
            port,
            stored_fingerprint,
            presented_fingerprint,
            ..
        } => StorageError::HostKey(format!(
            "服务器 {host}:{port} 的主机密钥和本机之前记录的不一致（记录的是 {stored_fingerprint}，现在是 {presented_fingerprint}）。\
             可能是服务器重装过系统，也可能有人在冒充它。请先在终端里连接这台服务器并确认新的指纹，再回来同步。"
        )),
        AppError::HostKeyVerification(m) => {
            StorageError::HostKey(format!("服务器主机密钥校验失败：{m}"))
        }
        AppError::KeyAuth { message, .. } => StorageError::Auth(message),
        AppError::Config(m) => {
            if m.contains("密码") || m.contains("私钥") || m.contains("密钥") {
                StorageError::Auth(format!("{m}。请先在连接设置里保存好这台服务器的登录凭据。"))
            } else {
                StorageError::Other(m)
            }
        }
        AppError::Ssh(m) => {
            if m.contains("认证") {
                StorageError::Auth(m)
            } else {
                StorageError::Network(format!(
                    "无法连接到 SFTP 服务器：{}",
                    m.strip_prefix("连接失败: ").unwrap_or(&m)
                ))
            }
        }
        other => StorageError::Other(other.to_string()),
    }
}

/// 失败的文本里有没有「没权限」「没空间」的迹象（`io::Error` 与 SFTP 状态消息只剩文字可看）。
fn classify_text(msg: &str, what: &str) -> StorageError {
    let lower = msg.to_lowercase();
    if lower.contains("permission") || lower.contains("denied") {
        StorageError::Permission(format!(
            "{what}失败：没有权限。请确认这个账号对同步目录有读写权限。"
        ))
    } else if lower.contains("no space") || lower.contains("quota") || lower.contains("disk full") {
        StorageError::Other(format!("{what}失败：服务器存储空间不足或超出配额。"))
    } else {
        StorageError::Other(format!("{what}失败：服务器返回错误（{msg}）。"))
    }
}

/// 一次操作的失败，分成两类：**连接坏了**（值得丢掉会话重连一次）和**操作本身失败**（重连没用）。
enum Fail {
    Dead(String),
    Err(StorageError),
}

impl From<StorageError> for Fail {
    fn from(e: StorageError) -> Self {
        Fail::Err(e)
    }
}

fn is_no_such_file(e: &SftpError) -> bool {
    matches!(e, SftpError::Status(s) if s.status_code == StatusCode::NoSuchFile)
}

/// `err` 是不是「路径不存在」。规范里是 `NoSuchFile`，但个别服务器把打开 / 列出不存在的路径报成
/// 笼统的 `Failure`——所以非 `NoSuchFile` 的服务器拒绝要再 `lstat` 确认一次，真不存在才算不存在
/// （没权限、会话坏了都不算，免得把真问题吞成「空目录」）。
async fn confirmed_missing(sftp: &SftpSession, path: &str, err: &SftpError) -> bool {
    if is_no_such_file(err) {
        return true;
    }
    if session_unusable(err) {
        return false;
    }
    matches!(sftp.symlink_metadata(path).await, Err(e) if is_no_such_file(&e))
}

/// SFTP 请求失败 → `Fail`。「会话不可用」的判据复用 `session_unusable`（`ssh::sftp_shared`，有
/// 测试钉着 `russh_sftp` 的真实行为）。
fn sftp_fail(e: SftpError, what: &str) -> Fail {
    if session_unusable(&e) {
        return Fail::Dead(e.to_string());
    }
    Fail::Err(match &e {
        SftpError::Status(st) => match st.status_code {
            StatusCode::PermissionDenied => StorageError::Permission(format!(
                "{what}失败：没有权限。请确认这个账号对同步目录有读写权限。"
            )),
            StatusCode::NoSuchFile => StorageError::Other(format!("{what}失败：路径不存在。")),
            StatusCode::OpUnsupported => {
                StorageError::Other(format!("{what}失败：服务器不支持这个 SFTP 操作。"))
            }
            _ => classify_text(&st.error_message, what),
        },
        other => StorageError::Other(format!("{what}失败：{other}")),
    })
}

/// `File` 的读写在 `russh_sftp` 里被包成 `io::Error`（消息是底层 SFTP 错误的文字）。
fn io_fail(e: io::Error, what: &str) -> Fail {
    let text = e.to_string();
    let lower = text.to_lowercase();
    let broken = matches!(
        e.kind(),
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::TimedOut
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
    ) || lower.contains("session closed")
        || lower.contains("timeout")
        || lower.contains("channel closed");
    if broken {
        Fail::Dead(text)
    } else {
        Fail::Err(classify_text(&text, what))
    }
}

/// 外层 `AppError`（拿不到共享会话 / 开不了独占通道）→ `Fail`。这几条都是「连接多半已经断了」，
/// 重连一次；重连后还失败，说明服务器没开 SFTP 之类，由 `run` 报最终错误。
fn open_fail(e: AppError) -> Fail {
    Fail::Dead(match e {
        AppError::Ssh(m) => m,
        other => other.to_string(),
    })
}

fn flat<T>(r: Result<Result<T, SftpError>, AppError>, what: &str) -> Result<T, Fail> {
    match r {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(sftp_fail(e, what)),
        Err(e) => Err(open_fail(e)),
    }
}

// ── 路径（纯函数） ───────────────────────────────────────────────────────

/// 把配置里的 `root` 解析成绝对路径：空 / `~` = 家目录；`~/x` = 家目录下；`/x` = 绝对路径；
/// 其它 = 相对家目录。`.`、`..`、重复斜杠按字面规整（不碰服务器，所以 root 还不存在也能算）。
pub(crate) fn resolve_root(home: &str, root: &str) -> String {
    let root = root.trim();
    let home = home.trim_end_matches('/');
    let joined = if root.is_empty() || root == "~" {
        home.to_string()
    } else if let Some(rest) = root.strip_prefix("~/") {
        format!("{home}/{rest}")
    } else if root.starts_with('/') {
        root.to_string()
    } else {
        format!("{home}/{root}")
    };
    normalize_abs(&joined)
}

fn normalize_abs(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    format!("/{}", out.join("/"))
}

/// `root` 下的相对路径（已规范化、用 `/` 连接，可为空）→ 服务器上的绝对路径。
fn join_abs(root: &str, rel: &str) -> String {
    match (rel.is_empty(), root.ends_with('/')) {
        (true, _) => root.to_string(),
        (false, true) => format!("{root}{rel}"),
        (false, false) => format!("{root}/{rel}"),
    }
}

/// 同目录下的隐藏 sidecar 路径（临时文件 / 被挪开的旧文件），沿用应用里其它 SFTP 写入的命名。
fn sidecar(path: &str, label: &str) -> Result<String, Fail> {
    crate::commands::sftp::remote_sidecar_path(path, label)
        .map_err(|e| Fail::Err(StorageError::Other(format!("无法生成临时文件名：{e}"))))
}

// ── 会话状态 ─────────────────────────────────────────────────────────────

struct Session {
    id: String,
    /// 解析后的 root 绝对路径。
    root: String,
    /// 登录用户家目录（建目录时据此跳过「肯定存在」的上级）。
    home: String,
    /// 正文传输用的独占 SFTP 通道（惰性开，适配器内共用）。
    body: Mutex<Option<Arc<SftpSession>>>,
}

struct State {
    session: Option<Arc<Session>>,
    last_used: Instant,
}

struct Inner {
    ssh: SshManager,
    connector: Arc<dyn SftpConnector>,
    connection_id: String,
    root_cfg: String,
    state: Mutex<State>,
    /// 已确认存在的绝对目录（只当「少发请求」的提示：写入报缺目录时无视缓存重新建）。
    known_dirs: PlMutex<HashSet<String>>,
    resolved: PlMutex<Option<String>>,
    /// 正在进行的操作数：空闲回收不能在传输中途把连接断了。
    inflight: AtomicUsize,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // 适配器被丢弃（同步停止 / 换了存储位置）时别把无头会话留在 `SshManager` 里。
        if let Some(s) = self.state.get_mut().session.take() {
            if let Ok(rt) = tokio::runtime::Handle::try_current() {
                let ssh = self.ssh.clone();
                rt.spawn(async move { ssh.disconnect_headless(&s.id).await });
            }
        }
    }
}

struct InFlight(Arc<Inner>);

impl InFlight {
    fn new(inner: &Arc<Inner>) -> Self {
        inner.inflight.fetch_add(1, Ordering::SeqCst);
        InFlight(inner.clone())
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.inflight.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Inner {
    /// 取得（必要时建立）会话。持着状态锁建连：并发的调用排队等同一次建连，不各开一条。
    async fn session(self: &Arc<Self>) -> Result<Arc<Session>, StorageError> {
        let mut st = self.state.lock().await;
        if let Some(s) = st.session.clone() {
            if self.ssh.get_connection(&s.id).await.is_some() {
                st.last_used = Instant::now();
                return Ok(s);
            }
            // 连接已经被摘掉（断线 / 被别处关闭）：重建。
            st.session = None;
        }

        let sid = match tokio::time::timeout(
            CONNECT_TIMEOUT,
            self.connector.connect(&self.connection_id),
        )
        .await
        {
            Ok(Ok(sid)) => sid,
            Ok(Err(e)) => return Err(map_connect_error(e)),
            Err(_) => {
                return Err(StorageError::Network(
                    "连接 SFTP 服务器超时（30 秒），请检查服务器是否在线、网络是否通畅。".into(),
                ))
            }
        };

        // 家目录：同时验证「SFTP 子系统可用」。拿不到就不留这条连接。
        let home = match self
            .ssh
            .with_shared_sftp(&sid, |sftp| async move { sftp.canonicalize(".").await })
            .await
        {
            Ok(Ok(home)) => home,
            Ok(Err(e)) => {
                self.ssh.disconnect_headless(&sid).await;
                return Err(match sftp_fail(e, "读取家目录") {
                    Fail::Dead(m) => {
                        StorageError::Network(format!("与 SFTP 服务器的连接中断了：{m}"))
                    }
                    Fail::Err(e) => e,
                });
            }
            Err(e) => {
                self.ssh.disconnect_headless(&sid).await;
                return Err(StorageError::Other(format!(
                    "这台服务器没有开启 SFTP 服务，或者账号不允许使用 SFTP，无法用来同步。（{}）",
                    match e {
                        AppError::Ssh(m) => m,
                        other => other.to_string(),
                    }
                )));
            }
        };

        let root = resolve_root(&home, &self.root_cfg);
        *self.resolved.lock() = Some(root.clone());
        self.known_dirs.lock().clear();
        let session = Arc::new(Session {
            id: sid.clone(),
            root,
            home,
            body: Mutex::new(None),
        });
        st.session = Some(session.clone());
        st.last_used = Instant::now();
        drop(st);
        self.spawn_reaper(sid);
        Ok(session)
    }

    /// 每条会话一个空闲回收任务（只持有弱引用，适配器丢了它自己结束）。
    fn spawn_reaper(self: &Arc<Self>, sid: String) {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut wait = IDLE_DISCONNECT;
            loop {
                tokio::time::sleep(wait).await;
                let Some(inner) = weak.upgrade() else { return };
                let mut st = inner.state.lock().await;
                match &st.session {
                    Some(s) if s.id == sid => {}
                    _ => return, // 已经被换掉 / 关掉
                }
                let idle = st.last_used.elapsed();
                if inner.inflight.load(Ordering::SeqCst) == 0 && idle >= IDLE_DISCONNECT {
                    st.session = None;
                    drop(st);
                    log::info!(
                        "[同步] SFTP 存储空闲超过 {} 秒，断开连接",
                        IDLE_DISCONNECT.as_secs()
                    );
                    inner.ssh.disconnect_headless(&sid).await;
                    return;
                }
                wait = if idle >= IDLE_DISCONNECT {
                    IDLE_DISCONNECT // 有操作在进行：过一个周期再看
                } else {
                    IDLE_DISCONNECT - idle
                };
            }
        });
    }

    async fn touch(&self) {
        self.state.lock().await.last_used = Instant::now();
    }

    /// 摘掉出了问题的会话并关闭它（只摘 id 对得上的，不误伤别人刚建好的新会话）。
    async fn drop_session(&self, sid: &str) {
        {
            let mut st = self.state.lock().await;
            if st.session.as_ref().is_some_and(|s| s.id == sid) {
                st.session = None;
            }
        }
        self.ssh.disconnect_headless(sid).await;
    }

    /// 跑一次操作；连接层面的失败丢掉会话、重连后**整个操作重来一次**（所有操作都是幂等的：
    /// 读、覆盖写、删除、建目录）。
    async fn run<T, F, Fut>(self: &Arc<Self>, op: F) -> Result<T, StorageError>
    where
        F: Fn(Arc<Session>) -> Fut,
        Fut: Future<Output = Result<T, Fail>>,
    {
        let _flight = InFlight::new(self);
        let mut retried = false;
        loop {
            let session = self.session().await?;
            match op(session.clone()).await {
                Ok(v) => {
                    self.touch().await;
                    return Ok(v);
                }
                Err(Fail::Err(e)) => {
                    self.touch().await;
                    return Err(e);
                }
                Err(Fail::Dead(msg)) => {
                    self.drop_session(&session.id).await;
                    if retried {
                        return Err(StorageError::Network(format!(
                            "与 SFTP 服务器的连接中断了，重连后仍然失败：{msg}"
                        )));
                    }
                    retried = true;
                    log::info!("[同步] SFTP 存储连接已失效（{msg}），重连后重试一次");
                }
            }
        }
    }

    /// 本适配器的正文通道（惰性开，失效随会话一起丢）。
    async fn body_channel(&self, s: &Session) -> Result<Arc<SftpSession>, Fail> {
        let mut slot = s.body.lock().await;
        if let Some(c) = slot.as_ref() {
            return Ok(c.clone());
        }
        let channel = Arc::new(self.ssh.open_sftp(&s.id).await.map_err(open_fail)?);
        *slot = Some(channel.clone());
        Ok(channel)
    }

    /// 逐级建出绝对目录 `abs_dir`（`mkdir -p`）。
    ///
    /// 先 `mkdir`、失败再查「是不是已经是目录」——和 `stat` 在前相比，目录已存在的常见情形也只要
    /// 一个往返；不能只信 `mkdir` 的失败（既可能是已存在也可能是没权限，要区分，不然权限问题
    /// 被吞掉，后面写文件时才收到一句莫名其妙的错）。家目录及其上级必然存在，直接跳过。
    async fn ensure_dirs(&self, s: &Session, abs_dir: &str) -> Result<(), Fail> {
        let comps: Vec<&str> = abs_dir.split('/').filter(|c| !c.is_empty()).collect();
        let home_comps: Vec<&str> = s.home.split('/').filter(|c| !c.is_empty()).collect();
        for depth in 1..=comps.len() {
            let path = format!("/{}", comps[..depth].join("/"));
            let under_home = depth <= home_comps.len() && comps[..depth] == home_comps[..depth];
            if under_home || self.known_dirs.lock().contains(&path) {
                continue;
            }
            let made = self
                .ssh
                .with_shared_sftp(&s.id, |sftp| {
                    let path = path.clone();
                    async move {
                        match sftp.create_dir(path.clone()).await {
                            Ok(()) => Ok(true),
                            Err(create_err) => match sftp.metadata(path).await {
                                Ok(meta) => Ok(meta.is_dir()),
                                Err(_) => Err(create_err),
                            },
                        }
                    }
                })
                .await;
            if !flat(made, "创建目录")? {
                return Err(Fail::Err(StorageError::Other(format!(
                    "创建目录失败：{path} 已存在，但它是一个文件而不是文件夹。"
                ))));
            }
            self.known_dirs.lock().insert(path);
        }
        Ok(())
    }
}

/// 覆盖提交：`tmp` → `target`。服务器的 `rename` 不覆盖已有文件时，先把旧文件挪成 sidecar，再
/// 改名新文件，失败把旧文件挪回去。
async fn commit_replace(sftp: &SftpSession, tmp: &str, target: &str) -> Result<(), Fail> {
    let first = match sftp.rename(tmp, target).await {
        Ok(()) => return Ok(()),
        Err(e) => e,
    };
    if session_unusable(&first) {
        return Err(sftp_fail(first, "提交文件"));
    }
    // 目标不存在时 rename 失败是别的原因（没权限……），原样报；存在且不是目录才走挪旧文件这条路。
    match sftp.symlink_metadata(target).await {
        Ok(meta) if !meta.is_dir() => {}
        _ => return Err(sftp_fail(first, "提交文件")),
    }
    let backup = sidecar(target, "syncbak")?;
    if let Err(e) = sftp.rename(target, &backup).await {
        return Err(sftp_fail(e, "替换旧文件"));
    }
    match sftp.rename(tmp, target).await {
        Ok(()) => {
            let _ = sftp.remove_file(&backup).await;
            Ok(())
        }
        Err(e) => {
            // 新文件没能就位：旧文件挪回去，别让目标消失。
            let _ = sftp.rename(&backup, target).await;
            Err(sftp_fail(e, "提交文件"))
        }
    }
}

/// 递归删除 `path`（文件 / 符号链接只删它本身，**不跟随链接**；目录连内容一起删）。不存在 = 成功。
async fn remove_tree(sftp: &SftpSession, path: &str) -> Result<(), SftpError> {
    let meta = match sftp.symlink_metadata(path).await {
        Ok(m) => m,
        Err(e) if is_no_such_file(&e) => return Ok(()),
        Err(e) => return Err(e),
    };
    if !meta.is_dir() {
        return match sftp.remove_file(path).await {
            Err(e) if !is_no_such_file(&e) => Err(e),
            _ => Ok(()),
        };
    }
    // 先深度优先收集所有目录（顺手删掉文件），最后由深到浅 rmdir。
    let mut dirs = vec![path.to_string()];
    let mut stack = vec![path.to_string()];
    while let Some(dir) = stack.pop() {
        let entries = match sftp.read_dir(dir.clone()).await {
            Ok(rd) => rd,
            Err(e) if is_no_such_file(&e) => continue,
            Err(e) => return Err(e),
        };
        for entry in entries {
            let child = entry.path();
            if entry.metadata().is_dir() {
                dirs.push(child.clone());
                stack.push(child);
            } else if let Err(e) = sftp.remove_file(child).await {
                if !is_no_such_file(&e) {
                    return Err(e);
                }
            }
        }
    }
    for dir in dirs.into_iter().rev() {
        if let Err(e) = sftp.remove_dir(dir).await {
            if !is_no_such_file(&e) {
                return Err(e);
            }
        }
    }
    Ok(())
}

// ── 对外的适配器 ─────────────────────────────────────────────────────────

/// 见模块文档。`Clone` 出来的实例共享同一条连接与状态。
#[derive(Clone)]
pub struct SftpStorage {
    inner: Arc<Inner>,
}

impl SftpStorage {
    /// `connection_id` 是已保存的连接 id；`root` 见模块文档（默认 `marcel-sync`，相对家目录）。
    pub fn new(
        ssh: SshManager,
        connector: Arc<dyn SftpConnector>,
        connection_id: &str,
        root: &str,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                ssh,
                connector,
                connection_id: connection_id.to_string(),
                root_cfg: root.to_string(),
                state: Mutex::new(State {
                    session: None,
                    last_used: Instant::now(),
                }),
                known_dirs: PlMutex::new(HashSet::new()),
                resolved: PlMutex::new(None),
                inflight: AtomicUsize::new(0),
            }),
        }
    }

    /// 生产构造：`ssh` 用 `AppState.ssh_manager`（known_hosts 同一份），`connections` 用
    /// `AppState.connection_store`。
    pub fn for_saved_connection(
        ssh: SshManager,
        connections: JsonStore<ConnectionStore>,
        connection_id: &str,
        root: &str,
    ) -> Self {
        let connector = Arc::new(SavedConnectionConnector::new(ssh.clone(), connections));
        Self::new(ssh, connector, connection_id, root)
    }

    /// 同步根目录在服务器上的**绝对路径**（连上之后才知道家目录，所以可能要先建一次连接）。
    /// 运行时据此把它加进受保护路径。
    pub async fn resolved_root(&self) -> Result<String, StorageError> {
        if let Some(root) = self.inner.resolved.lock().clone() {
            return Ok(root);
        }
        Ok(self.inner.session().await?.root.clone())
    }

    /// 立刻断开连接（同步停止、改了存储位置时调用）。之后再用会自动重连。
    pub async fn close(&self) {
        let sid = self
            .inner
            .state
            .lock()
            .await
            .session
            .take()
            .map(|s| s.id.clone());
        if let Some(sid) = sid {
            self.inner.ssh.disconnect_headless(&sid).await;
        }
    }
}

#[async_trait]
impl SyncStorage for SftpStorage {
    async fn list(&self, dir: &str) -> Result<Vec<RemoteEntry>, StorageError> {
        let rel = rel_segments(dir)?.join("/");
        let inner = self.inner.clone();
        let listed = inner
            .run(|s| {
                let abs = join_abs(&s.root, &rel);
                let inner = self.inner.clone();
                async move {
                    let r = inner
                        .ssh
                        .with_shared_sftp(&s.id, |sftp| {
                            let abs = abs.clone();
                            async move {
                                match sftp.read_dir(abs.clone()).await {
                                    Ok(entries) => Ok(Some(
                                        entries
                                            .filter_map(|e| {
                                                let name = e.file_name();
                                                if is_hidden_name(&name) {
                                                    return None;
                                                }
                                                let meta = e.metadata();
                                                let is_dir = meta.is_dir();
                                                Some(RemoteEntry {
                                                    name,
                                                    is_dir,
                                                    size: if is_dir {
                                                        0
                                                    } else {
                                                        meta.size.unwrap_or(0)
                                                    },
                                                })
                                            })
                                            .collect::<Vec<_>>(),
                                    )),
                                    // 目录不存在 → 空（规格约定）
                                    Err(e) if confirmed_missing(&sftp, &abs, &e).await => Ok(None),
                                    Err(e) => Err(e),
                                }
                            }
                        })
                        .await;
                    flat(r, "列出目录")
                }
            })
            .await?;
        Ok(listed.unwrap_or_default())
    }

    async fn get(&self, path: &str) -> Result<Option<Vec<u8>>, StorageError> {
        let rel = rel_segments(path)?.join("/");
        if rel.is_empty() {
            return Err(StorageError::Other("同步路径不能为空".into()));
        }
        let inner = self.inner.clone();
        inner
            .run(|s| {
                let abs = join_abs(&s.root, &rel);
                let inner = self.inner.clone();
                async move {
                    let sftp = inner.body_channel(&s).await?;
                    let mut file = match sftp.open(abs.as_str()).await {
                        Ok(f) => f,
                        Err(e) if confirmed_missing(&sftp, &abs, &e).await => return Ok(None),
                        Err(e) => return Err(sftp_fail(e, "读取文件")),
                    };
                    let mut buf = Vec::new();
                    file.read_to_end(&mut buf)
                        .await
                        .map_err(|e| io_fail(e, "读取文件"))?;
                    Ok(Some(buf))
                }
            })
            .await
    }

    async fn put(&self, path: &str, bytes: &[u8]) -> Result<(), StorageError> {
        let rel = rel_segments(path)?.join("/");
        if rel.is_empty() {
            return Err(StorageError::Other("同步路径不能为空".into()));
        }
        let inner = self.inner.clone();
        inner
            .run(|s| {
                let target = join_abs(&s.root, &rel);
                let inner = self.inner.clone();
                async move {
                    let sftp = inner.body_channel(&s).await?;
                    let tmp = sidecar(&target, "sync")?;
                    let flags = OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE;
                    // 乐观写：父目录多半已经在了（稳态），只有服务器说「没有这个路径」才去建目录。
                    let mut file = match sftp.open_with_flags(&tmp, flags).await {
                        Ok(f) => f,
                        Err(e) if is_no_such_file(&e) => {
                            let parent = target.rsplit_once('/').map(|(p, _)| p).unwrap_or("/");
                            inner.ensure_dirs(&s, parent).await?;
                            sftp.open_with_flags(&tmp, flags)
                                .await
                                .map_err(|e| sftp_fail(e, "写入文件"))?
                        }
                        Err(e) => return Err(sftp_fail(e, "写入文件")),
                    };
                    // shutdown = 等所有写确认 + 关句柄（关闭失败也会在这里暴露，比如配额超了）
                    let written = async {
                        file.write_all(bytes).await?;
                        file.shutdown().await
                    }
                    .await;
                    drop(file);
                    if let Err(e) = written {
                        let _ = sftp.remove_file(&tmp).await;
                        return Err(io_fail(e, "写入文件"));
                    }
                    if let Err(fail) = commit_replace(&sftp, &tmp, &target).await {
                        let _ = sftp.remove_file(&tmp).await;
                        return Err(fail);
                    }
                    Ok(())
                }
            })
            .await
    }

    async fn delete(&self, path: &str) -> Result<(), StorageError> {
        let rel = rel_segments(path)?.join("/");
        if rel.is_empty() {
            return Err(StorageError::Other("不能删除同步根目录本身".into()));
        }
        let inner = self.inner.clone();
        inner
            .run(|s| {
                let abs = join_abs(&s.root, &rel);
                let inner = self.inner.clone();
                async move {
                    let r = inner
                        .ssh
                        .with_shared_sftp(&s.id, |sftp| {
                            let abs = abs.clone();
                            async move { remove_tree(&sftp, &abs).await }
                        })
                        .await;
                    // 目录可能被删了：缓存里这棵子树的记录一并作废。
                    let prefix = format!("{abs}/");
                    inner
                        .known_dirs
                        .lock()
                        .retain(|d| *d != abs && !d.starts_with(&prefix));
                    flat(r, "删除")
                }
            })
            .await
    }

    async fn probe(&self) -> Result<(), StorageError> {
        // 1) 建连并确保 root 存在：连接 / 认证 / 主机密钥问题都在这一步暴露。
        let inner = self.inner.clone();
        inner
            .run(|s| {
                let inner = self.inner.clone();
                async move { inner.ensure_dirs(&s, &s.root).await }
            })
            .await?;
        // 2) 读：列一次 root。
        self.list("").await?;
        // 3) 写 / 读回 / 删一个探针文件。
        super::write_read_delete_probe(self).await
    }
}

#[cfg(test)]
mod tests;
