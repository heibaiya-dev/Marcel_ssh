//! SFTP 适配器的测试：纯函数（路径 / 错误映射）+ 对一个**内存 SFTP 服务器**的端到端。
//!
//! 内存服务器（`MemFs`）刻意学 OpenSSH 的几个脾气：`rename` 目标已存在就失败、`mkdir` 已存在就
//! 失败、`remove` 不删目录——这些正是适配器要兜住的地方。它跑在 `SshManager::
//! register_loopback_sftp_session` 登记的真实 russh 回环连接上，所以 `with_shared_sftp` /
//! `open_sftp` 走的是真实代码路径，不是 mock。

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::Mutex as PlMutex;
use russh_sftp::protocol::{Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Status};

use super::*;
use crate::error::KeyAuthCode;
use crate::sync::storage::contract_tests;

// ── 纯函数 ───────────────────────────────────────────────────────────────

#[test]
fn resolve_root_handles_relative_home_tilde_and_absolute_forms() {
    let home = "/home/alice";
    for (root, want) in [
        ("marcel-sync", "/home/alice/marcel-sync"),
        ("a/b/c", "/home/alice/a/b/c"),
        ("", "/home/alice"),
        ("~", "/home/alice"),
        ("~/sync", "/home/alice/sync"),
        ("/srv/sync", "/srv/sync"),
        ("/srv//sync/./x/..", "/srv/sync"),
        ("  marcel-sync  ", "/home/alice/marcel-sync"),
        ("../shared", "/home/shared"),
        ("/", "/"),
    ] {
        assert_eq!(resolve_root(home, root), want, "root={root:?}");
    }
    // 家目录自己带尾斜杠、或者就是 `/`（容器 / 某些 NAS）
    assert_eq!(resolve_root("/home/alice/", "x"), "/home/alice/x");
    assert_eq!(resolve_root("/", "x"), "/x");
}

#[test]
fn join_abs_never_doubles_or_drops_slashes() {
    assert_eq!(join_abs("/a/b", ""), "/a/b");
    assert_eq!(join_abs("/a/b", "c/d"), "/a/b/c/d");
    assert_eq!(join_abs("/", "c"), "/c");
    assert_eq!(join_abs("/", ""), "/");
}

#[test]
fn connect_errors_map_to_the_right_kind_and_never_carry_secrets() {
    let mismatch = map_connect_error(AppError::HostKeyMismatch {
        host: "nas.local".into(),
        port: 22,
        stored_algorithm: "ssh-ed25519".into(),
        stored_fingerprint: "SHA256:old".into(),
        presented_algorithm: "ssh-ed25519".into(),
        presented_fingerprint: "SHA256:new".into(),
    });
    match &mismatch {
        StorageError::HostKey(m) => {
            assert!(
                m.contains("nas.local:22") && m.contains("SHA256:old") && m.contains("SHA256:new"),
                "{m}"
            );
            assert!(m.contains("终端"), "要告诉用户去哪里确认：{m}");
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        map_connect_error(AppError::HostKeyVerification("x".into())),
        StorageError::HostKey(_)
    ));
    assert!(matches!(
        map_connect_error(AppError::KeyAuth {
            code: KeyAuthCode::Rejected,
            message: "认证失败：用户名或密码错误".into()
        }),
        StorageError::Auth(m) if m.contains("密码错误")
    ));
    assert!(matches!(
        map_connect_error(AppError::Config("未找到已保存的密码".into())),
        StorageError::Auth(_)
    ));
    assert!(matches!(
        map_connect_error(AppError::Config("同步用的服务器连接已经被删除了".into())),
        StorageError::Other(_)
    ));
    assert!(matches!(
        map_connect_error(AppError::Ssh("连接失败: 连接被拒绝".into())),
        StorageError::Network(m) if m.contains("连接被拒绝") && !m.contains("连接失败: ")
    ));
    assert!(matches!(
        map_connect_error(AppError::Ssh("密码认证错误: x".into())),
        StorageError::Auth(_)
    ));
}

#[test]
fn sftp_errors_are_classified_dead_vs_final() {
    use russh_sftp::protocol::StatusCode;
    let status = |code: StatusCode, msg: &str| {
        SftpError::Status(Status {
            id: 1,
            status_code: code,
            error_message: msg.into(),
            language_tag: "en".into(),
        })
    };
    assert!(matches!(
        sftp_fail(status(StatusCode::PermissionDenied, "x"), "写入文件"),
        Fail::Err(StorageError::Permission(_))
    ));
    assert!(matches!(
        sftp_fail(status(StatusCode::Failure, "Quota exceeded"), "写入文件"),
        Fail::Err(StorageError::Other(m)) if m.contains("空间")
    ));
    assert!(matches!(
        sftp_fail(status(StatusCode::OpUnsupported, "x"), "x"),
        Fail::Err(StorageError::Other(_))
    ));
    // 会话本身坏了的几种：值得重连
    assert!(matches!(sftp_fail(SftpError::Timeout, "x"), Fail::Dead(_)));
    assert!(matches!(
        sftp_fail(SftpError::UnexpectedBehavior("session closed".into()), "x"),
        Fail::Dead(_)
    ));
    assert!(matches!(
        sftp_fail(status(StatusCode::ConnectionLost, "x"), "x"),
        Fail::Dead(_)
    ));
    assert!(matches!(
        io_fail(
            io::Error::new(io::ErrorKind::BrokenPipe, "write channel closed"),
            "x"
        ),
        Fail::Dead(_)
    ));
    assert!(matches!(
        io_fail(io::Error::other("Permission denied"), "写入文件"),
        Fail::Err(StorageError::Permission(_))
    ));
}

// ── 内存 SFTP 服务器 ─────────────────────────────────────────────────────

const HOME: &str = "/home/tester";

#[derive(Clone)]
enum Node {
    Dir,
    File(Vec<u8>),
}

struct FsState {
    nodes: BTreeMap<String, Node>,
    /// 这个目录（含其下）禁止一切写操作 → PermissionDenied。
    deny_writes_under: Option<String>,
    /// 第 N 次 `rename`（从 `rename_calls` 清零后数起，1 起）强制失败，用来测覆盖写的回滚。
    rename_calls: usize,
    fail_rename_number: Option<usize>,
    /// 打开 / 列出不存在的路径时回笼统的 `Failure` 而不是 `NoSuchFile`（个别服务器的做法）。
    missing_as_failure: bool,
}

type SharedFs = Arc<PlMutex<FsState>>;

fn new_fs() -> SharedFs {
    let mut nodes = BTreeMap::new();
    for d in ["/", "/home", HOME, "/srv"] {
        nodes.insert(d.to_string(), Node::Dir);
    }
    Arc::new(PlMutex::new(FsState {
        nodes,
        deny_writes_under: None,
        rename_calls: 0,
        fail_rename_number: None,
        missing_as_failure: false,
    }))
}

enum OpenHandle {
    File { path: String },
    Dir { path: String, drained: bool },
}

struct MemFs {
    fs: SharedFs,
    handles: HashMap<String, OpenHandle>,
    next: u32,
}

impl MemFs {
    fn new(fs: SharedFs) -> Self {
        Self {
            fs,
            handles: HashMap::new(),
            next: 0,
        }
    }

    fn norm(path: &str) -> String {
        if path.starts_with('/') {
            normalize_abs(path)
        } else {
            normalize_abs(&format!("{HOME}/{path}"))
        }
    }

    fn new_handle(&mut self, h: OpenHandle) -> String {
        self.next += 1;
        let name = format!("h{}", self.next);
        self.handles.insert(name.clone(), h);
        name
    }

    fn denied(st: &FsState, p: &str) -> bool {
        st.deny_writes_under
            .as_ref()
            .is_some_and(|d| p == d || p.starts_with(&format!("{d}/")))
    }

    fn parent(p: &str) -> String {
        match p.rsplit_once('/') {
            Some(("", _)) | None => "/".to_string(),
            Some((parent, _)) => parent.to_string(),
        }
    }

    fn attrs(node: &Node) -> FileAttributes {
        match node {
            Node::Dir => FileAttributes {
                permissions: Some(0o040755),
                ..Default::default()
            },
            Node::File(b) => FileAttributes {
                size: Some(b.len() as u64),
                permissions: Some(0o100644),
                ..Default::default()
            },
        }
    }
}

fn ok(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: "Ok".into(),
        language_tag: "en-US".into(),
    }
}

impl russh_sftp::server::Handler for MemFs {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        Ok(Name {
            id,
            files: vec![File::dummy(Self::norm(&path))],
        })
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        let p = Self::norm(&path);
        {
            let st = self.fs.lock();
            match st.nodes.get(&p) {
                Some(Node::Dir) => {}
                Some(_) => return Err(StatusCode::Failure),
                None if st.missing_as_failure => return Err(StatusCode::Failure),
                None => return Err(StatusCode::NoSuchFile),
            }
        }
        let handle = self.new_handle(OpenHandle::Dir {
            path: p,
            drained: false,
        });
        Ok(Handle { id, handle })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        let Some(OpenHandle::Dir { path, drained }) = self.handles.get_mut(&handle) else {
            return Err(StatusCode::Failure);
        };
        if *drained {
            return Err(StatusCode::Eof);
        }
        *drained = true;
        let st = self.fs.lock();
        let files = st
            .nodes
            .iter()
            .filter(|(k, _)| k.as_str() != "/" && Self::parent(k) == *path)
            .map(|(k, n)| {
                let name = k.rsplit('/').next().unwrap_or(k).to_string();
                File::new(name, Self::attrs(n))
            })
            .collect();
        Ok(Name { id, files })
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        self.handles.remove(&handle);
        Ok(ok(id))
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        self.stat(id, path).await
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        match self.fs.lock().nodes.get(&Self::norm(&path)) {
            Some(n) => Ok(Attrs {
                id,
                attrs: Self::attrs(n),
            }),
            None => Err(StatusCode::NoSuchFile),
        }
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        pflags: OpenFlags,
        _attrs: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        let p = Self::norm(&filename);
        {
            let mut st = self.fs.lock();
            let writing = pflags.contains(OpenFlags::WRITE) || pflags.contains(OpenFlags::CREATE);
            if writing && Self::denied(&st, &p) {
                return Err(StatusCode::PermissionDenied);
            }
            match st.nodes.get(&p) {
                Some(Node::Dir) => return Err(StatusCode::Failure),
                Some(Node::File(_)) => {
                    if pflags.contains(OpenFlags::TRUNCATE) {
                        st.nodes.insert(p.clone(), Node::File(Vec::new()));
                    }
                }
                None => {
                    if !pflags.contains(OpenFlags::CREATE) {
                        return Err(if st.missing_as_failure {
                            StatusCode::Failure
                        } else {
                            StatusCode::NoSuchFile
                        });
                    }
                    if !matches!(st.nodes.get(&Self::parent(&p)), Some(Node::Dir)) {
                        return Err(StatusCode::NoSuchFile);
                    }
                    st.nodes.insert(p.clone(), Node::File(Vec::new()));
                }
            }
        }
        let handle = self.new_handle(OpenHandle::File { path: p });
        Ok(Handle { id, handle })
    }

    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, Self::Error> {
        let Some(OpenHandle::File { path }) = self.handles.get(&handle) else {
            return Err(StatusCode::Failure);
        };
        let st = self.fs.lock();
        let Some(Node::File(bytes)) = st.nodes.get(path) else {
            return Err(StatusCode::NoSuchFile);
        };
        let start = offset as usize;
        if start >= bytes.len() {
            return Err(StatusCode::Eof);
        }
        let end = (start + len as usize).min(bytes.len());
        Ok(Data {
            id,
            data: bytes[start..end].to_vec(),
        })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, Self::Error> {
        let Some(OpenHandle::File { path }) = self.handles.get(&handle) else {
            return Err(StatusCode::Failure);
        };
        let mut st = self.fs.lock();
        let Some(Node::File(bytes)) = st.nodes.get_mut(path) else {
            return Err(StatusCode::NoSuchFile);
        };
        let start = offset as usize;
        if bytes.len() < start + data.len() {
            bytes.resize(start + data.len(), 0);
        }
        bytes[start..start + data.len()].copy_from_slice(&data);
        Ok(ok(id))
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        let p = Self::norm(&filename);
        let mut st = self.fs.lock();
        if Self::denied(&st, &p) {
            return Err(StatusCode::PermissionDenied);
        }
        match st.nodes.get(&p) {
            Some(Node::File(_)) => {
                st.nodes.remove(&p);
                Ok(ok(id))
            }
            Some(Node::Dir) => Err(StatusCode::Failure), // OpenSSH：remove 不删目录
            None => Err(StatusCode::NoSuchFile),
        }
    }

    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        let p = Self::norm(&path);
        let mut st = self.fs.lock();
        if Self::denied(&st, &p) {
            return Err(StatusCode::PermissionDenied);
        }
        match st.nodes.get(&p) {
            Some(Node::Dir) => {
                if st.nodes.keys().any(|k| k != &p && Self::parent(k) == p) {
                    return Err(StatusCode::Failure); // 非空
                }
                st.nodes.remove(&p);
                Ok(ok(id))
            }
            Some(_) => Err(StatusCode::Failure),
            None => Err(StatusCode::NoSuchFile),
        }
    }

    async fn mkdir(
        &mut self,
        id: u32,
        path: String,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        let p = Self::norm(&path);
        let mut st = self.fs.lock();
        if Self::denied(&st, &p) {
            return Err(StatusCode::PermissionDenied);
        }
        if st.nodes.contains_key(&p) {
            return Err(StatusCode::Failure); // OpenSSH：已存在就是 Failure
        }
        if !matches!(st.nodes.get(&Self::parent(&p)), Some(Node::Dir)) {
            return Err(StatusCode::NoSuchFile);
        }
        st.nodes.insert(p, Node::Dir);
        Ok(ok(id))
    }

    async fn rename(
        &mut self,
        id: u32,
        oldpath: String,
        newpath: String,
    ) -> Result<Status, Self::Error> {
        let (from, to) = (Self::norm(&oldpath), Self::norm(&newpath));
        let mut st = self.fs.lock();
        st.rename_calls += 1;
        if st.fail_rename_number == Some(st.rename_calls) {
            return Err(StatusCode::Failure);
        }
        if Self::denied(&st, &from) || Self::denied(&st, &to) {
            return Err(StatusCode::PermissionDenied);
        }
        if !st.nodes.contains_key(&from) {
            return Err(StatusCode::NoSuchFile);
        }
        if st.nodes.contains_key(&to) {
            return Err(StatusCode::Failure); // 关键：不覆盖已有文件
        }
        let node = st.nodes.remove(&from).unwrap();
        st.nodes.insert(to, node);
        Ok(ok(id))
    }
}

// ── 测试用连接器与装配 ───────────────────────────────────────────────────

struct LoopbackConnector {
    ssh: SshManager,
    fs: SharedFs,
    connects: AtomicUsize,
    closers: Arc<std::sync::Mutex<Vec<(russh::server::Handle, russh::ChannelId)>>>,
    /// 排队的连接失败（先进先出），用来测错误映射。
    failures: PlMutex<Vec<AppError>>,
}

#[async_trait]
impl SftpConnector for LoopbackConnector {
    async fn connect(&self, connection_id: &str) -> Result<String, AppError> {
        assert_eq!(connection_id, "conn-1", "适配器应当用配置里的连接 id");
        if let Some(e) = {
            let mut f = self.failures.lock();
            if f.is_empty() {
                None
            } else {
                Some(f.remove(0))
            }
        } {
            return Err(e);
        }
        let n = self.connects.fetch_add(1, Ordering::SeqCst) + 1;
        let sid = format!("loopback-{n}");
        let fs = self.fs.clone();
        self.ssh
            .register_loopback_sftp_session(
                &sid,
                move || MemFs::new(fs.clone()),
                self.closers.clone(),
            )
            .await;
        Ok(sid)
    }
}

struct Rig {
    storage: SftpStorage,
    connector: Arc<LoopbackConnector>,
    fs: SharedFs,
    ssh: SshManager,
}

fn rig(root: &str) -> Rig {
    let ssh = SshManager::new();
    let fs = new_fs();
    let connector = Arc::new(LoopbackConnector {
        ssh: ssh.clone(),
        fs: fs.clone(),
        connects: AtomicUsize::new(0),
        closers: Arc::new(std::sync::Mutex::new(Vec::new())),
        failures: PlMutex::new(Vec::new()),
    });
    let storage = SftpStorage::new(ssh.clone(), connector.clone(), "conn-1", root);
    Rig {
        storage,
        connector,
        fs,
        ssh,
    }
}

fn names_under(fs: &SharedFs, prefix: &str) -> Vec<String> {
    fs.lock()
        .nodes
        .keys()
        .filter(|k| k.starts_with(prefix))
        .cloned()
        .collect()
}

// ── 端到端 ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn satisfies_the_storage_contract() {
    let r = rig("marcel-sync");
    contract_tests::run(&r.storage).await;
    // 整个契约只用一条连接
    assert_eq!(r.connector.connects.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn contract_holds_for_an_absolute_root_outside_home() {
    let r = rig("/srv/team/marcel-sync");
    contract_tests::run(&r.storage).await;
    assert!(
        names_under(&r.fs, "/srv/team/marcel-sync").contains(&"/srv/team/marcel-sync".to_string())
    );
}

#[tokio::test]
async fn resolved_root_is_absolute_and_available_before_the_root_exists() {
    let r = rig("marcel-sync");
    assert_eq!(
        r.storage.resolved_root().await.unwrap(),
        "/home/tester/marcel-sync"
    );
    assert!(
        !r.fs.lock().nodes.contains_key("/home/tester/marcel-sync"),
        "只是解析路径，不应顺手把目录建出来"
    );
    let abs = rig("/srv/sync");
    assert_eq!(abs.storage.resolved_root().await.unwrap(), "/srv/sync");
}

#[tokio::test]
async fn overwrite_replaces_the_file_and_leaves_no_sidecars_behind() {
    let r = rig("marcel-sync");
    r.storage
        .put("devices/d1/seg-1.msync", b"first version")
        .await
        .unwrap();
    r.storage
        .put("devices/d1/seg-1.msync", b"v2")
        .await
        .unwrap();
    assert_eq!(
        r.storage.get("devices/d1/seg-1.msync").await.unwrap(),
        Some(b"v2".to_vec())
    );
    let leftovers: Vec<String> = names_under(&r.fs, "/home/tester/marcel-sync")
        .into_iter()
        .filter(|p| p.rsplit('/').next().is_some_and(|n| n.starts_with('.')))
        .collect();
    assert!(
        leftovers.is_empty(),
        "写入完成后不该残留临时文件：{leftovers:?}"
    );
}

#[tokio::test]
async fn failed_overwrite_keeps_the_old_content() {
    let r = rig("marcel-sync");
    r.storage
        .put("devices/d1/seg-1.msync", b"precious")
        .await
        .unwrap();
    // 之后整个目录变成只读：覆盖写在「写临时文件」这一步就失败
    r.fs.lock().deny_writes_under = Some("/home/tester/marcel-sync".into());
    let err = r
        .storage
        .put("devices/d1/seg-1.msync", b"overwritten")
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::Permission(_)), "{err:?}");
    r.fs.lock().deny_writes_under = None;
    assert_eq!(
        r.storage.get("devices/d1/seg-1.msync").await.unwrap(),
        Some(b"precious".to_vec())
    );
}

#[tokio::test]
async fn overwrite_that_fails_halfway_rolls_back_to_the_old_file() {
    let r = rig("marcel-sync");
    r.storage
        .put("devices/d1/seg-1.msync", b"precious")
        .await
        .unwrap();
    // 覆盖写的改名序列：① tmp→目标（目标已存在，必败）② 目标→备份 ③ tmp→目标 ④（回滚）备份→目标。
    // 让 ③ 失败：旧文件必须被 ④ 挪回来，临时文件不残留。
    {
        let mut st = r.fs.lock();
        st.rename_calls = 0;
        st.fail_rename_number = Some(3);
    }
    let err = r
        .storage
        .put("devices/d1/seg-1.msync", b"new")
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::Other(_)), "{err:?}");
    r.fs.lock().fail_rename_number = None;
    assert_eq!(
        r.storage.get("devices/d1/seg-1.msync").await.unwrap(),
        Some(b"precious".to_vec())
    );
    assert_eq!(
        names_under(&r.fs, "/home/tester/marcel-sync"),
        vec![
            "/home/tester/marcel-sync",
            "/home/tester/marcel-sync/devices",
            "/home/tester/marcel-sync/devices/d1",
            "/home/tester/marcel-sync/devices/d1/seg-1.msync",
        ]
    );
}

#[tokio::test]
async fn servers_that_report_missing_paths_as_generic_failure_still_read_as_missing() {
    let r = rig("marcel-sync");
    r.fs.lock().missing_as_failure = true;
    assert!(r.storage.list("devices").await.unwrap().is_empty());
    assert_eq!(r.storage.get("devices/d1/seg-1.msync").await.unwrap(), None);
    // 真正的失败仍然是失败：没权限不能被吞成「空目录」
    r.storage.put("devices/d1/seg-1.msync", b"x").await.unwrap();
    r.fs.lock().deny_writes_under = Some("/home/tester".into());
    r.fs.lock().nodes.insert(
        "/home/tester/marcel-sync/devices/d1/seg-1.msync".into(),
        Node::File(b"x".to_vec()),
    );
    assert_eq!(
        r.storage.get("devices/d1/seg-1.msync").await.unwrap(),
        Some(b"x".to_vec())
    );
}

#[tokio::test]
async fn read_only_location_is_reported_as_a_permission_problem() {
    let r = rig("marcel-sync");
    r.fs.lock().deny_writes_under = Some("/home/tester".into());
    let err = r.storage.probe().await.unwrap_err();
    assert!(matches!(err, StorageError::Permission(_)), "{err:?}");
    assert!(err.to_string().contains("权限"), "{err}");
}

#[tokio::test]
async fn list_hides_sidecars_and_delete_of_a_directory_sweeps_them() {
    let r = rig("marcel-sync");
    r.storage.put("devices/d1/seg-1.msync", b"x").await.unwrap();
    // 模拟一次崩溃留下的 sidecar
    r.fs.lock().nodes.insert(
        "/home/tester/marcel-sync/devices/d1/.seg-2.msync.marcel-sync-deadbeef".into(),
        Node::File(b"half".to_vec()),
    );
    let listed: Vec<String> = r
        .storage
        .list("devices/d1")
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(listed, vec!["seg-1.msync".to_string()]);
    r.storage.delete("devices").await.unwrap();
    assert!(names_under(&r.fs, "/home/tester/marcel-sync/devices").is_empty());
}

#[tokio::test]
async fn reconnects_when_the_session_vanished_from_the_manager() {
    let r = rig("marcel-sync");
    r.storage.put("a.txt", b"1").await.unwrap();
    assert_eq!(r.connector.connects.load(Ordering::SeqCst), 1);
    // 连接被别处关掉 / 断线后被清理
    r.ssh.disconnect_headless("loopback-1").await;
    assert_eq!(r.storage.get("a.txt").await.unwrap(), Some(b"1".to_vec()));
    assert_eq!(r.connector.connects.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn reconnects_once_when_the_transport_dies_mid_session() {
    let r = rig("marcel-sync");
    r.storage.put("a.txt", b"1").await.unwrap();
    // 对端把所有 SFTP 通道关了（连接在册，但已不可用）——正文通道与共享元数据会话都废了
    let closers: Vec<_> = r.connector.closers.lock().unwrap().clone();
    assert!(!closers.is_empty());
    for (handle, ch) in closers {
        handle.close(ch).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        r.storage.get("a.txt").await.unwrap(),
        Some(b"1".to_vec()),
        "读操作应当透明重连后成功"
    );
    r.storage.put("b.txt", b"2").await.unwrap();
    assert_eq!(r.storage.get("b.txt").await.unwrap(), Some(b"2".to_vec()));
    assert!(
        r.connector.connects.load(Ordering::SeqCst) <= 2,
        "只重连一次，之后复用"
    );
}

#[tokio::test]
async fn connect_failures_surface_with_the_right_kind() {
    let r = rig("marcel-sync");
    r.connector.failures.lock().push(AppError::KeyAuth {
        code: KeyAuthCode::Rejected,
        message: "认证失败：用户名或密码错误".into(),
    });
    assert!(matches!(
        r.storage.list("").await,
        Err(StorageError::Auth(_))
    ));

    r.connector.failures.lock().push(AppError::HostKeyMismatch {
        host: "nas".into(),
        port: 22,
        stored_algorithm: "a".into(),
        stored_fingerprint: "SHA256:aaa".into(),
        presented_algorithm: "a".into(),
        presented_fingerprint: "SHA256:bbb".into(),
    });
    let err = r.storage.get("x").await.unwrap_err();
    assert!(matches!(err, StorageError::HostKey(_)), "{err:?}");

    // 失败不留痕迹：条件恢复后下一次调用正常建连
    assert!(r.storage.list("").await.unwrap().is_empty());
    assert_eq!(r.connector.connects.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn concurrent_operations_share_one_connection() {
    let r = rig("marcel-sync");
    let (a, b, c) = tokio::join!(
        r.storage.put("devices/d1/a", b"a"),
        r.storage.put("devices/d1/b", b"b"),
        r.storage.list("")
    );
    a.unwrap();
    b.unwrap();
    c.unwrap();
    assert_eq!(r.connector.connects.load(Ordering::SeqCst), 1);
    assert_eq!(
        r.storage.get("devices/d1/a").await.unwrap(),
        Some(b"a".to_vec())
    );
}

#[tokio::test(start_paused = true)]
async fn idle_connection_is_dropped_after_two_minutes_and_rebuilt_on_demand() {
    let r = rig("marcel-sync");
    r.storage.put("a.txt", b"1").await.unwrap();
    assert!(r.ssh.get_connection("loopback-1").await.is_some());

    // 一分钟后又用了一次：闲置计时从头来，不该断
    tokio::time::advance(Duration::from_secs(60)).await;
    r.storage.get("a.txt").await.unwrap();
    tokio::time::advance(Duration::from_secs(90)).await;
    tokio::task::yield_now().await;
    assert!(
        r.ssh.get_connection("loopback-1").await.is_some(),
        "距上次使用才 90 秒"
    );

    // 再闲够 2 分钟：断开
    tokio::time::advance(Duration::from_secs(60)).await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(r.ssh.get_connection("loopback-1").await.is_none());

    // 下一次使用自动重连
    assert_eq!(r.storage.get("a.txt").await.unwrap(), Some(b"1".to_vec()));
    assert_eq!(r.connector.connects.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn close_and_drop_release_the_connection() {
    let r = rig("marcel-sync");
    r.storage.put("a.txt", b"1").await.unwrap();
    r.storage.close().await;
    assert!(r.ssh.get_connection("loopback-1").await.is_none());
    // 关了之后仍可使用（自动重连）
    assert_eq!(r.storage.get("a.txt").await.unwrap(), Some(b"1".to_vec()));
    assert!(r.ssh.get_connection("loopback-2").await.is_some());

    let Rig { storage, ssh, .. } = r;
    drop(storage);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        ssh.get_connection("loopback-2").await.is_none(),
        "适配器丢弃时连接要一起收掉"
    );
}

#[tokio::test]
async fn a_file_where_a_directory_is_needed_gets_a_clear_error() {
    let r = rig("marcel-sync");
    r.storage.put("devices", b"i am a file").await.unwrap();
    let err = r.storage.put("devices/d1/seg", b"x").await.unwrap_err();
    assert!(
        err.to_string().contains("不是文件夹") || err.to_string().contains("失败"),
        "{err}"
    );
}
