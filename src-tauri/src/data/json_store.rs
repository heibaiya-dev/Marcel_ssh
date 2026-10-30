//! 5 份 JSON 配置（settings / connections / quick_commands / skills / mcp_servers）
//! 的**唯一写盘出口**。
//!
//! 取代旧的「各处 `clone → 改 → save_to_path → 换回`」：那套写法散在 40 多处，
//! 每处各自决定要不要持锁、什么时候提交内存，两个并发写者（例如连点两次「添加快捷指令」）
//! 会各自 clone 同一个旧值，后提交的把先提交的整个覆盖——丢一次写入。现在：
//!
//! - **写者串行化**：所有写入先拿 `writer` 锁，clone → 改 → 落盘 → 换入内存 → 放锁 → 广播
//!   整段是一个整体，后来的写者看到的一定是前一个写者提交后的值；
//! - **读者不被写盘阻塞**：fsync 期间内存里还是旧值，读者照常读到旧值，换入那一瞬间才短暂
//!   拿写锁（这是 bdeef3a 想要的效果，同时没有丢写）；
//! - **先落盘成功才提交内存**：写盘失败内存保持旧值，调用方拿到 `Err`，与旧行为一致；
//! - **没变化就不写盘、不广播**：序列化结果前后相同时只把候选值换入内存（内存里有些字段
//!   本来就不进序列化，例如渠道的 `api_key`，它们的更新仍然要生效），但不碰磁盘；
//! - **落盘 + 换入 + 广播整段不受调用方取消影响**：它跑在一个独立任务里，调用方的 future
//!   中途被丢弃（例如 HTTP 客户端断开）也不会出现「盘上是新值、内存是旧值」。
//!
//! 读取点不用改：[`JsonStore::read`] 的返回类型与 `tokio::sync::RwLock::read` 一致。
//!
//! 注意别在**持有 `read()` 守卫时**去 `mutate`/`replace`：换入内存需要写锁，会等自己的读锁，
//! 死锁。先把要读的东西 clone 出来、放掉守卫，再写。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use tokio::sync::{Mutex, OwnedMutexGuard, RwLock, RwLockReadGuard, TryLockError};

use super::DataChangeBus;
use crate::config::persist::{atomic_write, JsonPersistable};
use crate::error::AppError;

/// `T` 的统一约束：能序列化落盘、能 clone 出候选值、能跨线程。
pub struct JsonStore<T: JsonPersistable + Clone + Send + Sync + 'static> {
    path: PathBuf,
    data: Arc<RwLock<T>>,
    /// 串行化所有写入（也串行化 [`Self::mutate_transient`]，避免它的改动被某次
    /// 进行中的 `mutate` 用旧 clone 覆盖）。读者不碰它。
    writer: Arc<Mutex<()>>,
    bus: Option<DataChangeBus>,
}

impl<T: JsonPersistable + Clone + Send + Sync + 'static> Clone for JsonStore<T> {
    fn clone(&self) -> Self {
        Self {
            path: self.path.clone(),
            data: Arc::clone(&self.data),
            writer: Arc::clone(&self.writer),
            bus: self.bus.clone(),
        }
    }
}

impl<T: JsonPersistable + Clone + Send + Sync + 'static> JsonStore<T> {
    /// 用已经加载好的初值建 store。路径一律取登记表里 `T::SOURCE` 的位置。
    ///
    /// 加载（读文件、备份损坏文件、迁移）仍由调用方按原样做，这里只接管「之后的写」。
    /// `bus` 为 `None` 时不广播（无头 `--mcp-server` 没有同步引擎，传 `None`）。
    pub fn new(config_dir: &Path, initial: T, bus: Option<DataChangeBus>) -> Self {
        Self {
            path: T::SOURCE.path(config_dir),
            data: Arc::new(RwLock::new(initial)),
            writer: Arc::new(Mutex::new(())),
            bus,
        }
    }

    /// 这份数据落盘的文件路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    // ── 读 ────────────────────────────────────────────────────────────────

    /// 读锁。签名与 `Arc<tokio::sync::RwLock<T>>::read()` 一致，现有读取点原样可用。
    pub async fn read(&self) -> RwLockReadGuard<'_, T> {
        self.data.read().await
    }

    /// 非阻塞读锁（通知 / 更新器这类「拿不到就跳过」的同步路径用）。
    pub fn try_read(&self) -> Result<RwLockReadGuard<'_, T>, TryLockError> {
        self.data.try_read()
    }

    /// 阻塞读锁，只能在**非 async 上下文**调用（例如 Tauri `setup` 回调）。
    pub fn blocking_read(&self) -> RwLockReadGuard<'_, T> {
        self.data.blocking_read()
    }

    /// 当前值的拷贝。
    pub async fn snapshot(&self) -> T {
        self.data.read().await.clone()
    }

    // ── 写 ────────────────────────────────────────────────────────────────

    /// 改一份候选值并落盘；**唯一推荐的写法**。
    ///
    /// - 拿写者锁；
    /// - 读锁下 clone 出候选值，`f(&mut 候选)`——`f` 返回 `Err` 就是什么都不改
    ///   （不写盘、不换入）；
    /// - 候选值与当前值序列化结果相同则不写盘、不广播；
    /// - 否则 `spawn_blocking` 原子写盘，**写盘失败内存保持旧值**并返回 `Err`；
    /// - 成功后短暂拿写锁换入；放写者锁后广播。
    ///
    /// 「找不到 id 也算成功」之类的 no-op 分支：`f` 里 `return Ok(..)` 且不动候选值即可。
    /// 写盘**之后**才该发生的副作用（刷新注册表、通知更新器……）放在本函数返回之后。
    pub async fn mutate<R, F>(&self, f: F) -> Result<R, AppError>
    where
        R: Send,
        F: FnOnce(&mut T) -> Result<R, AppError> + Send,
    {
        let mut update = self.begin_update().await;
        let out = f(update.candidate_mut())?;
        update.commit().await?;
        Ok(out)
    }

    /// 给「中间还要 `await`」的复杂流程用：持写者锁，自己改 `candidate_mut()`，
    /// 最后 [`UpdateGuard::commit`]；不 commit 直接丢掉 = 放弃（什么都不发生）。
    ///
    /// 持有它期间其他写者都在排队，所以别在里面做耗时很久的事。
    pub async fn begin_update(&self) -> UpdateGuard<'_, T> {
        let writer = self.lock_writer().await;
        let candidate = self.data.read().await.clone();
        UpdateGuard {
            store: self,
            writer,
            candidate,
        }
    }

    /// 整份替换（设置页整份保存用）。与 `mutate` 走同一条路：串行化、没变不写盘、
    /// 失败保持旧值。返回是否真的写了盘。
    pub async fn replace(&self, candidate: T) -> Result<bool, AppError> {
        let writer = self.lock_writer().await;
        self.finish(writer, candidate, false).await
    }

    /// 强制把当前内存值写盘并广播——**仅**给启动时「加载后做了迁移 / 归一化，
    /// 要回写」用；平时的改动一律走 `mutate`。
    pub async fn save_current(&self) -> Result<(), AppError> {
        let writer = self.lock_writer().await;
        let current = self.data.read().await.clone();
        self.finish(writer, current, true).await.map(|_| ())
    }

    /// **只改内存、不落盘、不广播**。
    ///
    /// 唯一合法用途：MCP 无头模式 `create_connection` 建的临时连接（随进程生灭、
    /// 刻意不进配置文件）。别拿它绕开 `mutate`——绕过的改动会在下一次 `mutate` 时
    /// 随整份序列化被一起写进磁盘，而同步引擎完全不知道。
    pub async fn mutate_transient<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        let _writer = self.lock_writer().await;
        let mut data = self.data.write().await;
        f(&mut data)
    }

    // ── 内部 ──────────────────────────────────────────────────────────────

    async fn lock_writer(&self) -> OwnedMutexGuard<()> {
        Arc::clone(&self.writer).lock_owned().await
    }

    fn publish(&self) {
        if let Some(bus) = &self.bus {
            bus.publish(T::SOURCE);
        }
    }

    /// 落盘 → 换入内存 → 放写者锁 → 广播，整段放进独立任务跑。
    ///
    /// 为什么要 `spawn`：调用方的 future 可能在 `spawn_blocking` 写盘期间被丢弃（HTTP 客户端
    /// 断开会取消处理器）。那样写盘已经完成而「换入内存」永远不会发生，盘与内存从此分叉。
    /// 独立任务不随调用方取消，要么整段完成要么整段没发生。
    async fn finish(
        &self,
        writer: OwnedMutexGuard<()>,
        candidate: T,
        force: bool,
    ) -> Result<bool, AppError> {
        let store = self.clone();
        tokio::spawn(async move {
            let changed = store.persist_and_swap(candidate, force).await?;
            drop(writer);
            if changed {
                store.publish();
            }
            Ok(changed)
        })
        .await
        .map_err(|e| AppError::Config(format!("配置写入任务异常: {}", e)))?
    }

    /// 调用时必须已持有写者锁。返回「是否写了盘」。
    async fn persist_and_swap(&self, candidate: T, force: bool) -> Result<bool, AppError> {
        let after = to_pretty(&candidate)?;
        let unchanged = !force && {
            let current = self.data.read().await;
            to_pretty(&*current)? == after
        };
        if !unchanged {
            let path = self.path.clone();
            tokio::task::spawn_blocking(move || write_text(&path, &after))
                .await
                .map_err(|e| AppError::Config(format!("配置写盘线程异常: {}", e)))??;
        }
        // 没变化也要换入：内存里有不进序列化的字段（如渠道 api_key），候选值上的更新得生效。
        *self.data.write().await = candidate;
        Ok(!unchanged)
    }
}

/// [`JsonStore::begin_update`] 的句柄：持着写者锁，里面是一份候选值。
pub struct UpdateGuard<'a, T: JsonPersistable + Clone + Send + Sync + 'static> {
    store: &'a JsonStore<T>,
    writer: OwnedMutexGuard<()>,
    candidate: T,
}

impl<'a, T: JsonPersistable + Clone + Send + Sync + 'static> UpdateGuard<'a, T> {
    /// 候选值（只读）。
    pub fn candidate(&self) -> &T {
        &self.candidate
    }

    /// 候选值（可改）。commit 之前外界看不到任何改动。
    pub fn candidate_mut(&mut self) -> &mut T {
        &mut self.candidate
    }

    /// 其他读者此刻看到的值（commit 之前就是改动前的值）。
    pub async fn current(&self) -> RwLockReadGuard<'_, T> {
        self.store.data.read().await
    }

    /// 落盘并提交。返回「是否真的变了」（没变则没写盘、没广播）。
    /// 写盘失败返回 `Err`，内存保持旧值。
    pub async fn commit(self) -> Result<bool, AppError> {
        let Self {
            store,
            writer,
            candidate,
        } = self;
        store.finish(writer, candidate, false).await
    }
}

fn to_pretty<S: Serialize>(value: &S) -> Result<String, AppError> {
    serde_json::to_string_pretty(value)
        .map_err(|e| AppError::Config(format!("序列化配置失败: {}", e)))
}

/// 写一段已经序列化好的 JSON 文本：建目录 + tmp/fsync/rename（`atomic_write`）。
fn write_text(path: &Path, json: &str) -> Result<(), AppError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| AppError::Config(format!("创建配置目录失败: {}", e)))?;
    }
    atomic_write(path, json).map_err(|e| AppError::Config(format!("写入配置文件失败: {}", e)))
}

/// pretty JSON + `atomic_write`。
#[cfg(test)]
fn write_json<S: Serialize>(path: &Path, value: &S) -> Result<(), AppError> {
    write_text(path, &to_pretty(value)?)
}

/// 测试造夹具用：把 `value` 以与 `JsonStore` 完全相同的格式写到 `T::SOURCE` 的位置。
/// 取代旧的 `JsonPersistable::save_to_path`（那个口子已被删掉，生产代码只能走 `JsonStore`）。
#[cfg(test)]
pub fn write_fixture<T: JsonPersistable>(config_dir: &Path, value: &T) -> Result<(), AppError> {
    write_json(&T::SOURCE.path(config_dir), value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::DataSourceId;
    use serde::{Deserialize, Serialize};
    use std::time::Duration;

    /// 测试专用配置：落在 `settings.json` 的位置，只有一个列表和一个不进序列化的内存字段。
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    struct Doc {
        items: Vec<String>,
        #[serde(skip)]
        memory_only: String,
    }

    impl JsonPersistable for Doc {
        const SOURCE: DataSourceId = DataSourceId::Settings;
    }

    fn temp_dir(tag: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("marcel-json-store-{tag}-"))
            .tempdir()
            .unwrap()
    }

    fn store_in(dir: &Path, items: &[&str]) -> (JsonStore<Doc>, DataChangeBus) {
        let bus = DataChangeBus::new();
        let doc = Doc {
            items: items.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        (JsonStore::new(dir, doc, Some(bus.clone())), bus)
    }

    fn on_disk(dir: &Path) -> Option<Doc> {
        let path = DataSourceId::Settings.path(dir);
        let text = std::fs::read_to_string(path).ok()?;
        Some(serde_json::from_str(&text).unwrap())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mutate_writes_updates_memory_and_broadcasts() {
        let dir = temp_dir("write");
        let (store, bus) = store_in(dir.path(), &[]);
        let mut rx = bus.subscribe();

        let out = store
            .mutate(|d| {
                d.items.push("a".into());
                Ok(7)
            })
            .await
            .unwrap();

        assert_eq!(out, 7, "f 的返回值原样带回");
        assert_eq!(store.read().await.items, vec!["a".to_string()]);
        assert_eq!(on_disk(dir.path()).unwrap().items, vec!["a".to_string()]);
        assert_eq!(rx.try_recv().unwrap().source, DataSourceId::Settings);
        assert!(rx.try_recv().is_err(), "一次写入只广播一次");
    }

    /// 「找不到 id 是成功的 no-op」：不写盘、不广播（reorder 无变化不写盘语义）。
    #[tokio::test(flavor = "multi_thread")]
    async fn unchanged_mutate_neither_writes_nor_broadcasts() {
        let dir = temp_dir("noop");
        let (store, bus) = store_in(dir.path(), &["x"]);
        let mut rx = bus.subscribe();

        store.mutate(|_d| Ok(())).await.unwrap();
        // 改了又改回去，序列化结果与原来相同，同样算没变
        store
            .mutate(|d| {
                d.items.push("tmp".into());
                d.items.pop();
                Ok(())
            })
            .await
            .unwrap();

        assert!(
            on_disk(dir.path()).is_none(),
            "没变化不得创建 / 改写配置文件"
        );
        assert!(rx.try_recv().is_err(), "没变化不得广播");
    }

    /// 序列化不可见的内存字段（渠道 api_key 这类）：没变化不写盘，但更新必须生效。
    #[tokio::test(flavor = "multi_thread")]
    async fn memory_only_fields_are_swapped_in_without_touching_disk() {
        let dir = temp_dir("mem-only");
        let (store, bus) = store_in(dir.path(), &["x"]);
        let mut rx = bus.subscribe();

        store
            .mutate(|d| {
                d.memory_only = "secret".into();
                Ok(())
            })
            .await
            .unwrap();

        assert_eq!(store.read().await.memory_only, "secret");
        assert!(on_disk(dir.path()).is_none());
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn failing_closure_changes_nothing() {
        let dir = temp_dir("f-err");
        let (store, bus) = store_in(dir.path(), &["keep"]);
        let mut rx = bus.subscribe();

        let err = store
            .mutate(|d| -> Result<(), AppError> {
                d.items.push("half-done".into());
                Err(AppError::Config("boom".into()))
            })
            .await
            .unwrap_err();

        assert!(err.to_string().contains("boom"));
        assert_eq!(store.read().await.items, vec!["keep".to_string()]);
        assert!(on_disk(dir.path()).is_none(), "f 出错不得写盘");
        assert!(rx.try_recv().is_err(), "f 出错不得广播");
    }

    /// 目标路径是个目录：tmp 写得下去但 rename 必败 → 内存保持旧值、不广播、返回 Err。
    #[tokio::test(flavor = "multi_thread")]
    async fn write_failure_keeps_old_value_in_memory() {
        let dir = temp_dir("write-fail");
        let (store, bus) = store_in(dir.path(), &["old"]);
        let mut rx = bus.subscribe();
        std::fs::create_dir(DataSourceId::Settings.path(dir.path())).unwrap();

        let err = store
            .mutate(|d| {
                d.items.push("new".into());
                Ok(())
            })
            .await
            .expect_err("写盘必须失败");

        assert!(err.to_string().contains("写入配置文件失败"), "{err}");
        assert_eq!(
            store.read().await.items,
            vec!["old".to_string()],
            "写盘失败内存必须保持旧值"
        );
        assert!(rx.try_recv().is_err(), "写盘失败不得广播");

        // 失败不会把写者锁带死：移走障碍后下一次写入照常成功
        std::fs::remove_dir(DataSourceId::Settings.path(dir.path())).unwrap();
        store
            .mutate(|d| {
                d.items.push("new".into());
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(
            on_disk(dir.path()).unwrap().items,
            vec!["old".to_string(), "new".to_string()]
        );
    }

    /// 钉住竞态修复：两个并发 mutate 各加一条，最后两条都在。
    /// 旧写法（短锁改内存 → 放锁 → 落盘 → 再锁提交）会各自 clone 同一个旧值，丢掉先提交的那条。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_mutations_are_serialized_and_none_is_lost() {
        let dir = temp_dir("race");
        let (store, _bus) = store_in(dir.path(), &[]);

        let mut tasks = Vec::new();
        for i in 0..16 {
            let store = store.clone();
            tasks.push(tokio::spawn(async move {
                store
                    .mutate(|d| {
                        d.items.push(format!("item-{i}"));
                        Ok(())
                    })
                    .await
                    .unwrap();
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }

        let mut mem = store.snapshot().await.items;
        mem.sort();
        let mut disk = on_disk(dir.path()).unwrap().items;
        disk.sort();
        let mut expected: Vec<String> = (0..16).map(|i| format!("item-{i}")).collect();
        expected.sort();
        assert_eq!(mem, expected, "内存里 16 条一条不能少");
        assert_eq!(disk, expected, "磁盘上 16 条一条不能少");
    }

    /// 读者在写盘期间不被阻塞：持着写者锁（模拟 fsync 中）时 read 仍立即返回旧值。
    #[tokio::test(flavor = "multi_thread")]
    async fn readers_are_not_blocked_while_a_writer_is_in_flight() {
        let dir = temp_dir("readers");
        let (store, _bus) = store_in(dir.path(), &["old"]);

        let mut update = store.begin_update().await;
        update.candidate_mut().items.push("new".into());

        let seen = tokio::time::timeout(Duration::from_secs(2), store.snapshot())
            .await
            .expect("持写者锁期间读者不得被阻塞");
        assert_eq!(seen.items, vec!["old".to_string()], "commit 前读到的是旧值");

        update.commit().await.unwrap();
        assert_eq!(
            store.snapshot().await.items,
            vec!["old".to_string(), "new".to_string()]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn begin_update_dropped_without_commit_abandons_changes() {
        let dir = temp_dir("abandon");
        let (store, bus) = store_in(dir.path(), &["keep"]);
        let mut rx = bus.subscribe();

        {
            let mut update = store.begin_update().await;
            update.candidate_mut().items.push("never".into());
        }

        assert_eq!(store.read().await.items, vec!["keep".to_string()]);
        assert!(on_disk(dir.path()).is_none());
        assert!(rx.try_recv().is_err());
        // 放弃后写者锁已释放
        store.mutate(|_| Ok(())).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn commit_reports_whether_anything_changed() {
        let dir = temp_dir("commit-bool");
        let (store, _bus) = store_in(dir.path(), &[]);

        let update = store.begin_update().await;
        assert!(!update.commit().await.unwrap(), "没改 = 没变");

        let mut update = store.begin_update().await;
        update.candidate_mut().items.push("a".into());
        assert!(update.commit().await.unwrap(), "改了 = 变了");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn replace_swaps_the_whole_value_and_skips_identical_ones() {
        let dir = temp_dir("replace");
        let (store, bus) = store_in(dir.path(), &["a"]);
        let mut rx = bus.subscribe();

        let same = store.snapshot().await;
        assert!(!store.replace(same).await.unwrap(), "整份相同不写盘");
        assert!(on_disk(dir.path()).is_none());
        assert!(rx.try_recv().is_err());

        let next = Doc {
            items: vec!["b".into(), "c".into()],
            ..Default::default()
        };
        assert!(store.replace(next).await.unwrap());
        assert_eq!(
            on_disk(dir.path()).unwrap().items,
            vec!["b".to_string(), "c".to_string()]
        );
        assert_eq!(rx.try_recv().unwrap().source, DataSourceId::Settings);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mutate_transient_neither_writes_nor_broadcasts_but_is_visible() {
        let dir = temp_dir("transient");
        let (store, bus) = store_in(dir.path(), &[]);
        let mut rx = bus.subscribe();

        store
            .mutate_transient(|d| d.items.push("temp".into()))
            .await;

        assert_eq!(store.read().await.items, vec!["temp".to_string()]);
        assert!(on_disk(dir.path()).is_none(), "transient 不得落盘");
        assert!(rx.try_recv().is_err(), "transient 不得广播");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn save_current_forces_a_write_and_broadcasts() {
        let dir = temp_dir("save-current");
        let (store, bus) = store_in(dir.path(), &["seed"]);
        let mut rx = bus.subscribe();

        // 内存值没被任何 mutate 动过，但启动迁移后就是要把它回写
        store.save_current().await.unwrap();

        assert_eq!(on_disk(dir.path()).unwrap().items, vec!["seed".to_string()]);
        assert_eq!(rx.try_recv().unwrap().source, DataSourceId::Settings);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn store_without_bus_still_writes() {
        let dir = temp_dir("no-bus");
        let store = JsonStore::new(dir.path(), Doc::default(), None);
        store
            .mutate(|d| {
                d.items.push("a".into());
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(on_disk(dir.path()).unwrap().items, vec!["a".to_string()]);
    }

    /// 落盘文件的格式与旧 `save_to_path` 一致（pretty JSON），用 `write_fixture` 造的夹具
    /// 能被 `load_from_path` 原样读回。
    #[tokio::test(flavor = "multi_thread")]
    async fn written_file_is_pretty_json_readable_by_the_loader() {
        let dir = temp_dir("format");
        let (store, _bus) = store_in(dir.path(), &[]);
        store
            .mutate(|d| {
                d.items.push("a".into());
                Ok(())
            })
            .await
            .unwrap();

        let text = std::fs::read_to_string(DataSourceId::Settings.path(dir.path())).unwrap();
        assert_eq!(
            text,
            serde_json::to_string_pretty(&store.snapshot().await).unwrap()
        );

        let fixture_dir = temp_dir("fixture");
        write_fixture(
            fixture_dir.path(),
            &Doc {
                items: vec!["f".into()],
                ..Default::default()
            },
        )
        .unwrap();
        let loaded = Doc::load_from_path(&Doc::default_file(fixture_dir.path())).unwrap();
        assert_eq!(loaded.items, vec!["f".to_string()]);
    }

    /// 调用方 future 被丢弃（超时 / 客户端断开）时，已发出的写入仍然整段完成：
    /// 不会出现「盘上是新值、内存是旧值」。
    #[tokio::test(flavor = "multi_thread")]
    async fn dropping_the_caller_does_not_split_disk_and_memory() {
        let dir = temp_dir("cancel");
        let (store, _bus) = store_in(dir.path(), &[]);

        let fut = store.mutate(|d| {
            d.items.push("a".into());
            Ok(())
        });
        // 只给一次极短的机会：多半在 spawn_blocking 写盘途中被丢弃
        let _ = tokio::time::timeout(Duration::from_micros(1), fut).await;

        // 无论是否被丢弃，等后台任务落定后，盘和内存必须一致
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mem = store.snapshot().await;
        let disk = on_disk(dir.path());
        match disk {
            Some(d) => assert_eq!(d.items, mem.items, "盘与内存必须一致"),
            None => assert!(mem.items.is_empty(), "没落盘就内存也不能变"),
        }
    }
}
