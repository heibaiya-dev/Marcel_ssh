//! 后台作业（Background Job）—— [`crate::command_exec`] 体系的一部分。
//!
//! 后台作业不是一套平行的执行体系，而是「提交后立即返回、输出流式沉淀」的
//! 命令执行模式：执行仍走 [`super::manager::CommandExecutionManager`] 的
//! 统一注册表（exec_id、取消注册表、断连级联取消、审计记录全部复用），
//! 本模块只提供 Job 的**状态模型**与**输出沉淀**（环形缓冲 + 磁盘溢出文件）：
//!
//! - 内存中每作业保留最近 [`MAX_RING_BUFFER_BYTES`] 字节，完整输出写入
//!   临时目录的 `marcel-job-<id>.log`，`offset` 回读超出环形窗口时从
//!   溢出文件补齐。
//! - 溢出文件本身有上限 [`MAX_SPILL_BYTES`]：命令产出超过它的部分**无处
//!   可存**，回读时如实报 lossy，而不是拿内存尾巴冒充全量（见
//!   [`OutputRead`]）。
//! - `notify` watch 通道在每有新输出或状态迁移时递增总字节数，
//!   `job_output(wait=true)` 据此非忙等唤醒。
//!
//! 安全约定：`JobInfo.command` 只存展示命令（截断），绝不存含 sudo
//! 密码的实际执行命令；溢出文件内容为远端输出，路径在应用私有目录下。

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use super::ticket::{truncate_display, CancelReason};

/// 每个作业在内存中保留的最近输出字节数（超出部分仅存磁盘溢出文件）。
pub(crate) const MAX_RING_BUFFER_BYTES: usize = 128 * 1024;

/// 每个作业磁盘溢出文件的上限。超过它就不再写入（内存尾巴继续滚动），
/// 超限事实由 [`OutputRead::lossy`] 如实上报。
///
/// 有上限是硬要求：后台作业跑的是编译、下载、常驻服务，一条 `yes`、
/// 一个刷屏的调试日志就能写出无界文件，把用户磁盘写满。宁可有界地丢，
/// 不要无界地存。
pub(crate) const MAX_SPILL_BYTES: usize = 64 * 1024 * 1024;

/// 一次输出回读的结果。
///
/// `lossy` 是**必须传给模型**的事实：它意味着本次没能给出请求区间里的
/// 全部内容（内存窗口已滑出、溢出文件又不可用/不完整）。以前这种情况
/// 会返回缓冲尾巴却照报「已读到总长」，调用方以为拿全了——那是在骗模型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OutputRead {
    /// 本次返回的文本。
    pub text: String,
    /// 文本起点在原始输出流中的字节偏移；遇到空洞时可晚于请求起点。
    /// 文本为空且无新输出时等于请求起点。禁止用解码后的 text.len() 推游标。
    pub text_start_offset: usize,
    /// 调用方下次必须原样传回的**原始字节**偏移（包括已经报告的空洞）。
    pub next_offset: usize,
    /// 本次固定快照的末尾；next_offset < snapshot_end 表示还有内容可续读。
    pub snapshot_end: usize,
    /// 原始字节含无效 UTF-8，显示时使用了替换字符；不计入 skipped_bytes。
    pub invalid_utf8: bool,
    /// 是否丢了内容（丢了就必须说，不许假装读全）。
    pub lossy: bool,
    /// 丢掉的字节数：本次回读里接不上的那段空洞（前缀与尾巴之间缺掉的
    /// 字节数）。请求起点本身就落在空洞里时，它等于「请求起点到本次文本
    /// 起点」的距离。`lossy=false` 时恒为 0。
    pub skipped_bytes: usize,
}

/// 作业状态。是 [`super::ticket::ExecutionStatus`] 在 Job 语境下的收敛视图：
/// 断连/失败 → `failed`，用户主动终止 → `killed`，
/// **应用退出**（通道随进程消失）→ `interrupted`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Running,
    Completed,
    Killed,
    Failed,
    /// 上一次应用运行时派发、应用退出时还没结束的作业。从台账恢复出来的
    /// 记录用它：**通道已经随着进程关闭**，之后的输出再也读不到了，
    /// 远端进程是否还在跑则无从得知（没人发过信号）。
    Interrupted,
}

impl JobStatus {
    /// 解析 `job_list(status=...)` 过滤参数；无法识别时返回 None（不过滤）。
    pub fn parse_filter(s: &str) -> Option<Self> {
        match s {
            "running" => Some(Self::Running),
            "completed" => Some(Self::Completed),
            "killed" => Some(Self::Killed),
            "failed" => Some(Self::Failed),
            "interrupted" => Some(Self::Interrupted),
            _ => None,
        }
    }

    /// 是否已经是终态（不再变化）。
    pub fn is_terminal(self) -> bool {
        self != Self::Running
    }
}

impl std::fmt::Display for JobStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Running => write!(f, "running"),
            Self::Completed => write!(f, "completed"),
            Self::Killed => write!(f, "killed"),
            Self::Failed => write!(f, "failed"),
            Self::Interrupted => write!(f, "interrupted"),
        }
    }
}

/// 作业元数据（Tauri 事件与 `job_list` / `job_kill` 的载荷）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobInfo {
    pub job_id: String,
    pub session_id: String,
    pub task_id: Option<String>,
    /// 归属对话（子 agent 派发的作业记在父对话名下）。作业台账与访问围栏
    /// 的键；老记录/无归属为 None（对任何调用方可见，见 manager 的围栏）。
    #[serde(default)]
    pub owner_conversation_id: Option<String>,
    pub description: String,
    /// 展示命令（截断），不含 sudo 密码。
    pub command: String,
    pub status: JobStatus,
    /// 结算细节：退出事实（`exit code: 3` / `signal: KILL`）、失败原因等。
    /// 无则缺省——展示层不得自行编造。
    #[serde(default)]
    pub detail: Option<String>,
    pub started_at_millis: u128,
    pub finished_at_millis: Option<u128>,
    pub total_output_bytes: usize,
}

impl JobInfo {
    /// 是否属于某个归属对话（`owner_conversation_id` 为 None = 老记录 /
    /// 无归属作业，对任何调用方可见——与 DSH 的 unowned 作业同规则）。
    pub fn owned_by_conversation(&self, conversation_id: &str) -> bool {
        match self.owner_conversation_id.as_deref() {
            Some(owner) => owner == conversation_id,
            None => true,
        }
    }
}

/// `job_output` 的增量读取结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobOutputResult {
    pub job_id: String,
    /// 自 `offset` 起的新增输出。
    pub delta: String,
    /// 文本起点在原始输出流中的偏移（丢内容时可晚于请求起点）。
    /// 仅用于说明范围，续读必须使用 offset，不能加 delta.len() 推算。
    #[serde(default)]
    pub text_start_offset: usize,
    /// 已读到的原始字节偏移（调用方下次原样传回）。
    pub offset: usize,
    /// 本次快照的末尾；offset < snapshot_end 表示尚有内容可继续读取。
    #[serde(default)]
    pub snapshot_end: usize,
    /// 显示时替换了无效 UTF-8；offset 仍按原始字节计算。
    #[serde(default)]
    pub invalid_utf8: bool,
    pub status: JobStatus,
    /// 结算细节（退出码 / 信号 / 失败原因），无则缺省。
    #[serde(default)]
    pub detail: Option<String>,
    /// 终止来源（仅 Killed/断连结算时有值）。调用方据此区分
    /// 界面用户终止、Agent job_kill、任务级联取消——
    /// `JobStatus::Killed` 本身不携带「谁终止的」信息。
    pub cancel_reason: Option<CancelReason>,
    /// 本次回读是否丢了内容（内存窗口滑出且溢出文件不可用/不完整）。
    /// **调用方必须把它转达给模型**：丢了就说丢了，不能假装读全。
    #[serde(default)]
    pub lossy: bool,
    /// 丢掉的字节数（本次回读里接不上的那段空洞：前缀与尾巴之间缺掉的
    /// 字节数；请求起点就落在空洞里时＝请求起点到本次文本起点的距离）。
    #[serde(default)]
    pub skipped_bytes: usize,
    /// 完整输出的落点（溢出文件），仅在确实写下过内容时有值。
    /// 文件在用户本机（应用私有目录），模型读不到，只用于告知与排障。
    #[serde(default)]
    pub spill_path: Option<String>,
}

impl JobOutputResult {
    /// 单页显示文本的 UTF-8 字节上限。工具和存储共用，禁止读取全量后再截断。
    pub const MAX_READ_BYTES: usize = 32_000;
}

/// UTF-8 最多四字节；额外三字节仅用于确认分页切点处的完整字符。
const UTF8_LOOKAHEAD: usize = 3;

/// 一次读取的不可变输入。文件 append / ring 滚动不改变这页的末尾与内存内容。
pub(crate) struct OutputSnapshot {
    from_offset: usize,
    snapshot_end: usize,
    ring_start: usize,
    ring: Vec<u8>,
    spill_path: Option<PathBuf>,
    budget: usize,
    #[cfg(test)]
    before_spill_read: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
}

impl OutputSnapshot {
    /// 所有文件操作都在调用方释放实例锁之后执行。
    pub fn read(self) -> OutputRead {
        let mut result = OutputRead {
            text: String::new(),
            text_start_offset: self.from_offset,
            next_offset: self.from_offset.min(self.snapshot_end),
            snapshot_end: self.snapshot_end,
            invalid_utf8: false,
            lossy: false,
            skipped_bytes: 0,
        };
        if self.from_offset >= self.snapshot_end {
            return result;
        }
        let mut next = self.from_offset;
        if next < self.ring_start {
            #[cfg(test)]
            if let Some(probe) = &self.before_spill_read {
                probe();
            }
            let want = (self.ring_start - next).min(self.budget + UTF8_LOOKAHEAD);
            let mut bytes = read_spill_range(self.spill_path.as_deref(), next, want);
            let spill_end = next + bytes.len();
            if spill_end == self.ring_start {
                // 文件与 ring 连续：先拼原始字节再解码，字符跨边界不会被替换。
                bytes.extend_from_slice(
                    &self.ring[..self
                        .ring
                        .len()
                        .min(self.budget + UTF8_LOOKAHEAD - bytes.len())],
                );
                let (text, consumed, invalid) = decode_page(&bytes, self.budget);
                result.text = text;
                result.next_offset = next + consumed;
                result.invalid_utf8 = invalid;
                return result;
            }
            let (text, consumed, invalid) = decode_page(&bytes, self.budget);
            next += consumed;
            result.text = text;
            result.next_offset = next;
            result.invalid_utf8 = invalid;
            // 预算已用尽或 lookahead 中仍有未消费字节：下一页继续读前缀，
            // 还没有走到空洞，不能提前报告/跳过它。
            if consumed < bytes.len() || bytes.len() == want || result.text.len() == self.budget {
                return result;
            }
            // 文件短读、缺失或失败，只能接到本次冻结的 ring 起点。
            result.skipped_bytes = self.ring_start - next;
            result.lossy = result.skipped_bytes > 0;
            result.next_offset = self.ring_start;
            if !result.text.is_empty() && !self.ring.is_empty() {
                // 前缀和尾巴之间有空洞，分两页给出，避免把不连续的字节
                // 拼成看似连续的文本。已报告的空洞由 next_offset 跨过，只报一次。
                return result;
            }
            next = self.ring_start;
            if result.text.is_empty() {
                result.text_start_offset = next;
            }
        }
        let (text, consumed, invalid) = decode_page(&self.ring, self.budget - result.text.len());
        result.text.push_str(&text);
        result.next_offset = next + consumed;
        result.invalid_utf8 |= invalid;
        result
    }
}

/// 单个作业的可变状态。调用方必须持有外层锁访问（见 manager 的 jobs 注册表）。
pub(crate) struct JobInstance {
    pub info: JobInfo,
    /// 关联的统一执行记录 exec_id（kill 时据此定位取消信号）。
    pub exec_id: u64,
    /// 最近输出的环形缓冲（最多 [`MAX_RING_BUFFER_BYTES`] 字节）。
    ring_buffer: VecDeque<u8>,
    /// 自启动以来累计收到的输出字节数。
    pub(crate) total_bytes_written: usize,
    /// 完整输出溢出文件（首个 chunk 到达时惰性创建）。
    spill_path: Option<PathBuf>,
    /// 已写入溢出文件的字节数（达 [`MAX_SPILL_BYTES`] 后停止写入）。
    spill_bytes: usize,
    /// 溢出文件是否已放弃（超上限，或写入失败已警告过）。
    spill_abandoned: bool,
    /// 终止来源。仅结算为 Killed/断连失败时落值（首次结算者为准）；
    /// `None` = 尚未结算或旧数据/未知来源，展示层必须保持中性文案。
    pub(crate) cancel_reason: Option<CancelReason>,
    /// 新输出/状态迁移通知（值 = total_bytes_written）。
    pub notify_tx: watch::Sender<usize>,
    /// 结算通知是否已投递给所属 agent（等价 DSH 的 reported）。
    /// 置位后该作业不再产生新的「已完成」通知；`job_output(wait=true)`
    /// 等到结算与 agent 循环挂起消费都会置位，防重复注入。
    pub(crate) settled_notified: bool,
    /// 测试慢盘/并发时只截住本实例的一次读取，不影响并行运行的其它测试。
    #[cfg(test)]
    pub(crate) before_spill_read: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
}

impl JobInstance {
    pub fn new(
        job_id: String,
        exec_id: u64,
        session_id: String,
        task_id: Option<String>,
        owner_conversation_id: Option<String>,
        description: Option<String>,
        display_command: String,
        notify_tx: watch::Sender<usize>,
    ) -> Self {
        let now = now_millis();
        Self {
            info: JobInfo {
                job_id,
                session_id,
                task_id,
                owner_conversation_id,
                description: description.unwrap_or_else(|| {
                    format!(
                        "Background execution of {}",
                        truncate_display(&display_command)
                    )
                }),
                command: truncate_display(&display_command),
                status: JobStatus::Running,
                detail: None,
                started_at_millis: now,
                finished_at_millis: None,
                total_output_bytes: 0,
            },
            exec_id,
            ring_buffer: VecDeque::with_capacity(1024),
            total_bytes_written: 0,
            spill_path: None,
            spill_bytes: 0,
            spill_abandoned: false,
            cancel_reason: None,
            notify_tx,
            settled_notified: false,
            #[cfg(test)]
            before_spill_read: None,
        }
    }

    /// 从台账恢复一条上一次应用运行的作业。
    ///
    /// 恢复出来的记录只有「当时抓到的输出」（溢出文件）+ 结束时的状态：
    /// 通道随进程消失，之后再也没有新输出；环形缓冲是空的（进程内资源），
    /// 回读一律走溢出文件。`settled_notified` 直接置位——上一次运行的作业
    /// 绝不该给这一轮的对话注入「作业已完成」通知。
    pub fn restore(
        info: JobInfo,
        spill_path: Option<PathBuf>,
        captured_bytes: usize,
        cancel_reason: Option<CancelReason>,
    ) -> Self {
        let (notify_tx, _) = watch::channel(captured_bytes);
        Self {
            info: JobInfo {
                total_output_bytes: captured_bytes,
                ..info
            },
            // 恢复的作业没有可取消的 exec（那条通道早没了）。
            exec_id: 0,
            ring_buffer: VecDeque::new(),
            total_bytes_written: captured_bytes,
            spill_bytes: captured_bytes,
            spill_path,
            // 溢出文件若写不动/不存在，回读会如实报 lossy。
            spill_abandoned: false,
            cancel_reason,
            notify_tx,
            settled_notified: true,
            #[cfg(test)]
            before_spill_read: None,
        }
    }

    /// 追加一段输出：写溢出文件 + 进环形缓冲 + 通知等待方。
    ///
    /// 溢出文件达 [`MAX_SPILL_BYTES`] 后停止写入并只记一次警告：内存尾巴
    /// 继续滚动（诊断价值集中在尾部），超出的部分回读时如实报 lossy。
    pub fn append_output(&mut self, bytes: &[u8], temp_dir: &std::path::Path) {
        if bytes.is_empty() {
            return;
        }

        if self.spill_path.is_none() {
            self.spill_path = Some(temp_dir.join(spill_file_name(
                &self.info.job_id,
                self.info.started_at_millis,
            )));
        }
        if !self.spill_abandoned {
            self.spill_bytes = self.write_spill(bytes);
        }

        for &b in bytes {
            if self.ring_buffer.len() >= MAX_RING_BUFFER_BYTES {
                self.ring_buffer.pop_front();
            }
            self.ring_buffer.push_back(b);
        }

        self.total_bytes_written += bytes.len();
        self.info.total_output_bytes = self.total_bytes_written;
        let _ = self.notify_tx.send(self.total_bytes_written);
    }

    /// 把一段输出追加进溢出文件，返回文件新增后的总字节数。达到上限或
    /// 写入失败时放弃溢出（后续只保留内存尾巴），并各自警告一次。
    fn write_spill(&mut self, bytes: &[u8]) -> usize {
        let Some(path) = self.spill_path.clone() else {
            return self.spill_bytes;
        };
        if self.spill_bytes >= MAX_SPILL_BYTES {
            self.spill_abandoned = true;
            log::warn!(
                "command_exec: 作业 {} 输出超过溢出上限（{} MB），之后的内容只保留内存尾巴",
                self.info.job_id,
                MAX_SPILL_BYTES / (1024 * 1024)
            );
            return self.spill_bytes;
        }
        // 只写到上限，不写超出的部分（宁可截断也不留无界文件）。
        let remaining = MAX_SPILL_BYTES - self.spill_bytes;
        let to_write = &bytes[..bytes.len().min(remaining)];
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(mut file) => {
                if let Err(e) = file.write_all(to_write) {
                    self.spill_abandoned = true;
                    log::warn!(
                        "command_exec: 作业 {} 溢出文件写入失败（{}），之后的内容只保留内存尾巴",
                        self.info.job_id,
                        e
                    );
                    return self.spill_bytes;
                }
                self.spill_bytes + to_write.len()
            }
            Err(e) => {
                // 目录不存在 / 权限不足：只警告一次，不打断作业。
                self.spill_abandoned = true;
                log::warn!(
                    "command_exec: 作业 {} 溢出文件不可用（{}: {}），之后的内容只保留内存尾巴",
                    self.info.job_id,
                    path.display(),
                    e
                );
                self.spill_bytes
            }
        }
    }

    /// 溢出文件的落点（可能不存在——写入被放弃或目录不可用）。台账与
    /// 提示文案据此告诉调用方「完整输出在哪」。
    pub fn spill_path(&self) -> Option<&std::path::Path> {
        self.spill_path.as_deref()
    }

    /// 溢出文件里实际保住的字节数（≤ [`MAX_SPILL_BYTES`]）。
    pub fn spill_bytes(&self) -> usize {
        self.spill_bytes
    }

    /// 在实例锁内只冻结末尾、路径和至多 budget + 3 字节的 ring 片段，
    /// 不打开文件、不做 metadata。返回值不借用实例，读盘可移到锁外 blocking worker。
    pub fn output_snapshot(&self, from_offset: usize, budget: usize) -> OutputSnapshot {
        // 至少容得下一个 UTF-8 字符；上限由同一个权威常量限制所有调用方。
        let budget = budget.clamp(4, JobOutputResult::MAX_READ_BYTES);
        let snapshot_end = self.total_bytes_written;
        let ring_start = snapshot_end.saturating_sub(self.ring_buffer.len());
        let local_start = from_offset.max(ring_start).min(snapshot_end) - ring_start;
        let local_end = local_start
            .saturating_add(budget + UTF8_LOOKAHEAD)
            .min(self.ring_buffer.len());
        OutputSnapshot {
            from_offset,
            snapshot_end,
            ring_start,
            ring: self
                .ring_buffer
                .range(local_start..local_end)
                .copied()
                .collect(),
            spill_path: self.spill_path.clone(),
            budget,
            #[cfg(test)]
            before_spill_read: self.before_spill_read.clone(),
        }
    }

    /// 测试中的同步入口；生产经 manager 先取快照，再在锁外读盘。
    #[cfg(test)]
    fn read_output_from(&self, from_offset: usize, budget: usize) -> OutputRead {
        self.output_snapshot(from_offset, budget).read()
    }

    /// 结算作业状态。只在 `Running` 时生效（幂等）：这里落下的终态即最终
    /// 状态，后到的结算（worker / kill / 断连）一律不改写。
    pub fn finalize(&mut self, status: JobStatus) {
        self.finalize_with_reason(status, None);
    }

    /// 带终止来源的结算。`reason` 只在首次结算生效时落值，与状态一起锁定。
    ///
    /// 「先到者为准」只覆盖**都走到本方法**的结算方。worker 的收尾顺序是
    /// 「先把执行记录摘出运行表，再落作业终态」，两件事之间的窗口里到达的
    /// `kill_job` 不再落 `Killed`（它发现 exec 已离开运行表就只如实返回记录，
    /// 见 `manager.rs` 的 `kill_job`）——否则会把一条其实已经成功结束的作业记成
    /// 被终止，并把 worker 带着退出细节的那次结算幂等挡掉。
    pub fn finalize_with_reason(&mut self, status: JobStatus, reason: Option<CancelReason>) {
        if self.info.status != JobStatus::Running {
            return;
        }
        self.info.status = status;
        self.cancel_reason = reason;
        self.info.finished_at_millis = Some(now_millis());
        let _ = self.notify_tx.send(self.total_bytes_written);
    }

    /// 结算并附上退出事实等细节（只在 `Running` 时生效，语义同上）。
    pub fn finalize_with_detail(
        &mut self,
        status: JobStatus,
        reason: Option<CancelReason>,
        detail: Option<String>,
    ) {
        if self.info.status != JobStatus::Running {
            return;
        }
        self.finalize_with_reason(status, reason);
        self.info.detail = detail.filter(|d| !d.is_empty());
    }
}

/// 输出溢出文件的文件名。
///
/// 确定性命名（job_id + 启动毫秒，时间戳用来避开进程重启后计数器归零带
/// 来的同名文件）：**落台账时就要写出这个名字**，否则应用在作业跑着时
/// 退出，重启后就没有路径可指、退出前抓到的输出也就找不回来了。
/// 命名规则只此一处。
pub(crate) fn spill_file_name(job_id: &str, started_at_millis: u128) -> String {
    format!("marcel-job-{}-{}.log", job_id, started_at_millis)
}

/// 读有限的原始字节；短读继续、Interrupted 重试，EOF/错误保留已读前缀。
/// want 在进入文件层前已被单页预算限制，绝不按剩余日志总长分配或读取。
fn read_spill_range(path: Option<&std::path::Path>, from_offset: usize, want: usize) -> Vec<u8> {
    let Some(mut file) = path.and_then(|path| File::open(path).ok()) else {
        return Vec::new();
    };
    if file.seek(SeekFrom::Start(from_offset as u64)).is_err() {
        return Vec::new();
    }
    #[cfg(test)]
    let mut file = CountingReader(file);
    read_bounded(&mut file, want)
}

fn read_bounded(reader: &mut impl Read, want: usize) -> Vec<u8> {
    let mut buf = vec![0; want];
    let mut filled = 0;
    while filled < want {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    buf.truncate(filled);
    buf
}

/// 按显示字节预算解码一页，同时返回实际消费的原始字节数。
/// 无效字节替换成 U+FFFD（3B）时也受预算约束，游标绝不按替换后的长度推进。
fn decode_page(bytes: &[u8], budget: usize) -> (String, usize, bool) {
    let mut text = String::new();
    let mut consumed = 0;
    let mut invalid = false;
    while consumed < bytes.len() && text.len() < budget {
        let rest = &bytes[consumed..];
        let (valid, error_len) = match std::str::from_utf8(rest) {
            Ok(valid) => (valid, None),
            Err(error) => (
                // valid_up_to 是标准库验证过的字符边界。
                std::str::from_utf8(&rest[..error.valid_up_to()]).unwrap(),
                Some(
                    error
                        .error_len()
                        .unwrap_or(rest.len() - error.valid_up_to()),
                ),
            ),
        };
        let mut cut = valid.len().min(budget - text.len());
        while !valid.is_char_boundary(cut) {
            cut -= 1;
        }
        text.push_str(&valid[..cut]);
        consumed += cut;
        if cut < valid.len() {
            break;
        }
        let Some(error_len) = error_len else { break };
        if budget - text.len() < '\u{fffd}'.len_utf8() {
            break;
        }
        text.push('\u{fffd}');
        consumed += error_len;
        invalid = true;
    }
    (text, consumed, invalid)
}

#[cfg(test)]
thread_local! {
    static SPILL_READ_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// 统计真正由 Read 返回的字节，不能用请求长度或输出字符串长度代替 I/O。
#[cfg(test)]
struct CountingReader<R>(R);

#[cfg(test)]
impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.0.read(buf)?;
        SPILL_READ_BYTES.with(|bytes| bytes.set(bytes.get() + n));
        Ok(n)
    }
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个测试用独立 job_id + 独立临时子目录，避免溢出文件互相污染。
    fn instance(id: &str) -> JobInstance {
        let (tx, _) = watch::channel(0);
        JobInstance::new(
            id.into(),
            1,
            "sess_1".into(),
            None,
            Some("conv_1".into()),
            Some("test job".into()),
            "echo hello".into(),
            tx,
        )
    }

    fn test_temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("marcel-job-test-{}-{}", tag, std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn spill_paging_reads_linear_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let mut inst = instance("job_read_budget");
        let budget = JobOutputResult::MAX_READ_BYTES;
        let chunk = vec![b'x'; 1024 * 1024];
        // 实际 append_output 生成 spill；64 MiB 是生产文件上限，非虚拟 reader。
        for _ in 0..64 {
            inst.append_output(&chunk, temp.path());
        }
        assert_eq!(inst.spill_bytes(), MAX_SPILL_BYTES);
        let mut offset = 0;
        let mut total_read = 0;
        let mut max_page_read = 0;
        let mut pages = 0;
        let started = std::time::Instant::now();
        while offset < inst.total_bytes_written {
            SPILL_READ_BYTES.with(|bytes| bytes.set(0));
            let read = inst.read_output_from(offset, budget);
            let physical = SPILL_READ_BYTES.with(|bytes| bytes.get());
            total_read += physical;
            max_page_read = max_page_read.max(physical);
            assert!(read.text.len() <= budget);
            assert_eq!(read.text_start_offset, offset);
            assert_eq!(read.next_offset - offset, read.text.len());
            assert!(read.text.bytes().all(|b| b == b'x'));
            assert!(!read.lossy);
            assert!(!read.invalid_utf8);
            assert!(read.next_offset > offset);
            offset = read.next_offset;
            pages += 1;
        }
        eprintln!(
            "spill paging: output={} budget={} pages={} max_page_read={} total_read={} elapsed={:?}",
            inst.total_bytes_written, budget, pages, max_page_read, total_read, started.elapsed()
        );
        assert!(
            max_page_read <= budget + 3,
            "单页实际读盘超预算: {max_page_read}"
        );
        assert!(
            total_read <= inst.total_bytes_written + pages * 3,
            "分页累计读盘必须 O(B): {total_read}"
        );
    }

    /// 旧测试验证的是整段内容可恢复；新契约必须逐页收齐验证同一事实，
    /// 不能仅把原来的全量长度断言改成一页长度就算通过。
    fn collect_output(inst: &JobInstance, from: usize) -> OutputRead {
        let mut read = inst.read_output_from(from, JobOutputResult::MAX_READ_BYTES);
        while read.next_offset < read.snapshot_end {
            let next = inst.read_output_from(read.next_offset, JobOutputResult::MAX_READ_BYTES);
            assert!(next.next_offset > read.next_offset);
            read.text.push_str(&next.text);
            read.next_offset = next.next_offset;
            read.skipped_bytes += next.skipped_bytes;
            read.lossy |= next.lossy;
            read.invalid_utf8 |= next.invalid_utf8;
        }
        read
    }

    #[test]
    fn ring_buffer_and_delta_read() {
        let temp = test_temp_dir("delta");
        let mut inst = instance("job_delta_1");

        inst.append_output(b"hello world\n", &temp);
        assert_eq!(inst.total_bytes_written, 12);

        let read = collect_output(&inst, 0);
        assert_eq!(read.text, "hello world\n");
        assert_eq!(read.next_offset, 12);
        assert_eq!(read.text_start_offset, 0);
        assert!(!read.lossy);

        inst.append_output(b"second line\n", &temp);
        assert_eq!(inst.total_bytes_written, 24);

        let read2 = inst.read_output_from(12, JobOutputResult::MAX_READ_BYTES);
        assert_eq!(read2.text, "second line\n");
        assert_eq!(read2.next_offset, 24);
        // 无空洞路径：文本起点就是请求起点（续读偏移的对照基准）
        assert_eq!(read2.text_start_offset, 12);
        assert!(!read2.lossy);

        // 超前 offset：空增量，原样返回总长
        let read3 = inst.read_output_from(999, JobOutputResult::MAX_READ_BYTES);
        assert_eq!(read3.text, "");
        assert_eq!(read3.next_offset, 24);
        assert_eq!(read3.text_start_offset, 999);
        assert!(!read3.lossy);
    }

    #[test]
    fn ring_buffer_evicts_oldest_beyond_window() {
        let temp = test_temp_dir("evict");
        let mut inst = instance("job_evict_1");
        // 超过窗口大小，环形缓冲只留最近 MAX_RING_BUFFER_BYTES 字节
        let big = vec![b'x'; MAX_RING_BUFFER_BYTES + 4096];
        inst.append_output(&big, &temp);
        assert_eq!(inst.total_bytes_written, big.len());
        assert_eq!(inst.ring_buffer.len(), MAX_RING_BUFFER_BYTES);

        // 回读最早内容需要走溢出文件，且必须读全（lossy = false）
        let head = collect_output(&inst, 0);
        assert_eq!(head.text.len(), big.len());
        assert!(head.text.starts_with('x'));
        assert_eq!(head.text_start_offset, 0);
        assert!(!head.lossy);

        // 中段偏移同样可从溢出文件精确回读
        let mid = collect_output(&inst, 1024);
        assert!(mid.text.starts_with('x'));
        assert_eq!(mid.text.len(), big.len() - 1024);
        assert_eq!(mid.text_start_offset, 1024);
        assert!(!mid.lossy);
    }

    #[test]
    fn missing_spill_file_reports_lossy_tail() {
        // 溢出目录不存在（= 生产里"目录没建 / 写失败"的情形）：
        // 回读超窗口内容时必须如实报 lossy，而不是拿内存尾巴冒充全量。
        let temp = test_temp_dir("no-spill-dir").join("absent");
        let mut inst = instance("job_lossy_1");
        let big = vec![b'y'; MAX_RING_BUFFER_BYTES + 4096];
        inst.append_output(&big, &temp);
        assert!(!temp.exists());
        assert_eq!(inst.spill_bytes(), 0);

        let read = collect_output(&inst, 0);
        assert!(read.lossy, "溢出文件不可用必须标记 lossy");
        // 尾巴仍然是可用的近似（诊断价值在尾部），偏移推进到总长
        assert_eq!(read.text.len(), MAX_RING_BUFFER_BYTES);
        // 一段前缀都没补上：返回文本就是尾巴，起点是窗口起点而不是请求起点
        assert_eq!(read.text_start_offset, big.len() - MAX_RING_BUFFER_BYTES);
        assert_eq!(read.skipped_bytes, big.len() - MAX_RING_BUFFER_BYTES);
        assert_eq!(read.next_offset, big.len());

        // 窗口内的读不受影响，也不算 lossy
        let tail = inst.read_output_from(big.len() - 10, JobOutputResult::MAX_READ_BYTES);
        assert_eq!(tail.text.len(), 10);
        assert!(!tail.lossy);
        assert_eq!(tail.text_start_offset, big.len() - 10);
        assert_eq!(tail.skipped_bytes, 0);
    }

    /// 分片续读的回归：溢出文件不可用 + 内存尾巴大于调用方单次回读上限时，
    /// 尾巴必须能沿存储层返回的原始 next_offset 一段段读完，第二段不能重复
    /// 第一段的内容。
    ///
    /// 曾经的算法把续读偏移算成「请求 offset + 已取走字节数」——丢内容时
    /// 返回文本的起点是窗口起点（`ring_start`），比请求起点靠后一大截，
    /// 于是每次续读都落回尾巴开头，模型反复拿到同一段首字节。
    #[test]
    fn lossy_tail_read_resumes_from_the_tail_start_without_repeating() {
        let temp = test_temp_dir("lossy-chunk").join("absent");
        let mut inst = instance("job_lossy_chunk_1");
        // 尾巴的前 32KB 是 'A'、其余是 'B'：一旦重复读，第二段又会以 'A' 开头。
        let cut = JobOutputResult::MAX_READ_BYTES;
        let mut body = vec![b'A'; cut];
        body.extend(std::iter::repeat(b'B').take(MAX_RING_BUFFER_BYTES - cut));
        inst.append_output(&vec![b'x'; MAX_RING_BUFFER_BYTES], &temp);
        inst.append_output(&body, &temp);
        assert!(!temp.exists());
        assert_eq!(inst.total_bytes_written, MAX_RING_BUFFER_BYTES + body.len());

        let first = inst.read_output_from(0, cut);
        assert!(first.lossy);
        assert_eq!(first.text_start_offset, MAX_RING_BUFFER_BYTES);
        assert_eq!(first.text, "A".repeat(cut));
        assert_eq!(first.next_offset, MAX_RING_BUFFER_BYTES + cut);

        // 存储层已经分页；调用方只传回真实游标，不再对字符串重复切片。
        let second = inst.read_output_from(first.next_offset, cut);
        assert_eq!(second.text_start_offset, MAX_RING_BUFFER_BYTES + cut);
        assert!(second.text.starts_with('B'), "续读不能重复已读过的尾巴开头");
        assert!(!second.lossy);
        let remaining = collect_output(&inst, second.next_offset);
        assert_eq!(
            format!("{}{}{}", first.text, second.text, remaining.text).as_bytes(),
            body
        );
    }

    #[test]
    fn spill_is_capped_and_reports_the_gap() {
        let temp = test_temp_dir("cap");
        let mut inst = instance("job_cap_1");
        // 写入超过溢出上限的内容：文件必须停在 MAX_SPILL_BYTES
        let chunk = vec![b'z'; 1024 * 1024];
        for _ in 0..(MAX_SPILL_BYTES / chunk.len() + 2) {
            inst.append_output(&chunk, &temp);
        }
        assert!(inst.total_bytes_written > MAX_SPILL_BYTES);
        assert_eq!(inst.spill_bytes(), MAX_SPILL_BYTES);

        let spill = inst.spill_path().expect("溢出路径应已登记").to_path_buf();
        assert_eq!(
            std::fs::metadata(&spill).unwrap().len(),
            MAX_SPILL_BYTES as u64
        );

        // 溢出上限之外、内存窗口之前的那一段是真丢了：读全量必须报 lossy，
        // 并把空洞大小说清楚（这里 = 窗口起点 - 溢出覆盖终点）。
        let ring_start = inst.total_bytes_written - MAX_RING_BUFFER_BYTES;
        let gap = ring_start - MAX_SPILL_BYTES;
        let early = collect_output(&inst, 0);
        assert!(early.lossy, "溢出上限之外的内容读不到，必须报 lossy");
        assert_eq!(early.skipped_bytes, gap);
        // 能读到的部分 = 溢出文件覆盖的全部 + 内存尾巴（逐页收齐，一字节不漏）
        assert_eq!(early.text.len(), MAX_SPILL_BYTES + MAX_RING_BUFFER_BYTES);
        assert_eq!(early.next_offset, inst.total_bytes_written);
        // 前缀补上了：文本从请求起点开始（空洞夹在文本中间）
        assert_eq!(early.text_start_offset, 0);

        // 从空洞起点读：仍然拿得到尾巴，空洞照实报
        let beyond = collect_output(&inst, MAX_SPILL_BYTES);
        assert!(beyond.lossy);
        assert_eq!(beyond.skipped_bytes, gap);
        assert_eq!(beyond.text.len(), MAX_RING_BUFFER_BYTES);
        // 这段前缀一段都没补上：文本就是尾巴，起点是窗口起点
        assert_eq!(beyond.text_start_offset, ring_start);
    }

    #[test]
    fn utf8_pages_cross_file_ring_and_wrapped_ring_boundaries_losslessly() {
        let temp = tempfile::tempdir().unwrap();
        let mut inst = instance("job_utf8");
        let mut text = "中\u{1f642}文abc\n".repeat(MAX_RING_BUFFER_BYTES / 6);
        while text.is_char_boundary(text.len() - MAX_RING_BUFFER_BYTES) {
            text.push('x');
        }
        // 多次追加使 VecDeque 绕回；ring 起点刻意落在一个多字节字符中间。
        for chunk in text.as_bytes().chunks(7919) {
            inst.append_output(chunk, temp.path());
        }
        let ring_start = text.len() - inst.ring_buffer.len();
        assert!(!text.is_char_boundary(ring_start));
        assert!(!inst.ring_buffer.as_slices().1.is_empty());
        for budget in [4, 7, 11, JobOutputResult::MAX_READ_BYTES] {
            let mut offset = 0;
            let mut collected = String::new();
            let mut total_read = 0;
            let mut pages = 0;
            while offset < text.len() {
                let snapshot = inst.output_snapshot(offset, budget);
                assert!(snapshot.ring.len() <= budget + UTF8_LOOKAHEAD);
                SPILL_READ_BYTES.with(|bytes| bytes.set(0));
                let read = snapshot.read();
                let physical = SPILL_READ_BYTES.with(|bytes| bytes.get());
                assert!(physical <= budget + UTF8_LOOKAHEAD);
                total_read += physical;
                pages += 1;
                assert!(!read.lossy);
                assert!(
                    !read.invalid_utf8,
                    "合法 UTF-8 不能被切坏，offset={offset} budget={budget}"
                );
                assert!(read.text.len() <= budget);
                assert_eq!(read.text_start_offset, offset);
                assert_eq!(read.next_offset, offset + read.text.len());
                assert!(read.next_offset > offset);
                assert!(text.is_char_boundary(read.next_offset));
                collected.push_str(&read.text);
                offset = read.next_offset;
            }
            assert_eq!(collected, text);
            assert!(total_read <= text.len() + pages * (UTF8_LOOKAHEAD + 3));
        }
    }

    #[test]
    fn prefix_gap_and_tail_use_disjoint_offsets_and_report_the_gap_once() {
        let temp = tempfile::tempdir().unwrap();
        let mut inst = instance("job_gap_pages");
        let prefix = "prefix中文\u{1f642}";
        let missing = "missing".repeat(20);
        let tail = "TAIL0123456789\n".repeat(MAX_RING_BUFFER_BYTES / 15 + 1);
        let text = format!("{prefix}{missing}{tail}");
        inst.append_output(text.as_bytes(), temp.path());
        let ring_start = text.len() - MAX_RING_BUFFER_BYTES;
        OpenOptions::new()
            .write(true)
            .open(inst.spill_path().unwrap())
            .unwrap()
            .set_len(prefix.len() as u64)
            .unwrap();
        let mut offset = 0;
        let mut collected = String::new();
        let mut skipped = 0;
        let mut gap_pages = 0;
        while offset < text.len() {
            let read = inst.read_output_from(offset, 13);
            assert!(read.next_offset > offset);
            if read.lossy {
                gap_pages += 1;
                skipped += read.skipped_bytes;
                assert_eq!(read.next_offset, ring_start);
                assert!(!read.text.contains("TAIL"), "前缀和尾巴不能冒充连续文本");
            } else {
                assert_eq!(read.text_start_offset, offset);
                assert_eq!(read.next_offset, offset + read.text.len());
            }
            assert!(!read.invalid_utf8);
            collected.push_str(&read.text);
            offset = read.next_offset;
        }
        assert_eq!(gap_pages, 1);
        assert_eq!(skipped, ring_start - prefix.len());
        assert_eq!(collected, format!("{}{}", prefix, &text[ring_start..]));
        // 起点落在 gap 里，直接跳到 ring 且只收本次跳过的距离。
        let from_gap = inst.read_output_from(prefix.len() + 3, 13);
        assert_eq!(from_gap.text_start_offset, ring_start);
        assert_eq!(from_gap.skipped_bytes, ring_start - prefix.len() - 3);
        assert_eq!(from_gap.next_offset, ring_start + from_gap.text.len());
    }

    #[test]
    fn invalid_utf8_restored_spill_uses_raw_byte_offsets_and_bounded_text() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bad-utf8.log");
        let mut raw = b"a\xff\xfe".to_vec();
        raw.extend_from_slice("中\u{1f642}".as_bytes());
        raw.extend_from_slice(b"\xf0\x9fend\xe4\xb8");
        std::fs::write(&path, &raw).unwrap();
        let info = instance("job_invalid").info;
        let inst = JobInstance::restore(info, Some(path.clone()), raw.len(), None);
        for budget in [0, 1, 4, 5, 8, JobOutputResult::MAX_READ_BYTES] {
            let mut offset = 0;
            let mut text = String::new();
            let mut replaced = false;
            while offset < raw.len() {
                let read = inst.read_output_from(offset, budget);
                assert!(read.next_offset > offset);
                assert_eq!(read.text_start_offset, offset);
                assert!(read.text.len() <= budget.clamp(4, JobOutputResult::MAX_READ_BYTES));
                assert!(!read.lossy);
                assert_eq!(read.skipped_bytes, 0);
                assert_eq!(
                    read.text,
                    String::from_utf8_lossy(&raw[offset..read.next_offset])
                );
                text.push_str(&read.text);
                replaced |= read.invalid_utf8;
                offset = read.next_offset;
            }
            assert!(replaced);
            assert_eq!(text, String::from_utf8_lossy(&raw));
            assert_eq!(offset, raw.len());
        }
        // 非法/任意用户 offset 也按原始字节定位，不能以替换文本长度跳过后续字节。
        let mid_char = inst.read_output_from(4, 4);
        assert!(mid_char.invalid_utf8);
        assert_eq!(mid_char.next_offset, 5);
        assert_eq!(std::fs::read(path).unwrap(), raw, "读路径不能修写原文件");
    }

    #[test]
    fn snapshot_stays_fixed_when_output_appends_or_spill_disappears() {
        let temp = tempfile::tempdir().unwrap();
        let mut inst = instance("job_snapshot");
        inst.append_output(&vec![b'a'; MAX_RING_BUFFER_BYTES + 25], temp.path());
        let end = inst.total_bytes_written;
        let snapshot = inst.output_snapshot(20, 16);
        assert!(snapshot.ring.len() <= 19);
        inst.append_output(&vec![b'b'; MAX_RING_BUFFER_BYTES + 50], temp.path());
        let read = snapshot.read();
        assert_eq!(read.snapshot_end, end);
        assert_eq!(read.text, "a".repeat(16));
        assert_eq!(read.next_offset, 36);
        assert!(!read.lossy);

        let snapshot = inst.output_snapshot(0, 16);
        let ring_start = inst.total_bytes_written - MAX_RING_BUFFER_BYTES;
        std::fs::remove_file(inst.spill_path().unwrap()).unwrap();
        let missing = snapshot.read();
        assert!(missing.lossy);
        assert_eq!(missing.skipped_bytes, ring_start);
        assert_eq!(missing.text_start_offset, ring_start);
        assert_eq!(missing.next_offset, ring_start + 16);
        assert_eq!(missing.text, "b".repeat(16));
    }

    #[test]
    fn restored_truncated_or_missing_spill_and_empty_reads_keep_state() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("truncated.log");
        std::fs::write(&path, b"head").unwrap();
        let mut original = instance("job_restored");
        original.finalize_with_detail(
            JobStatus::Killed,
            Some(CancelReason::User),
            Some("kept".into()),
        );
        let inst = JobInstance::restore(
            original.info.clone(),
            Some(path.clone()),
            1000,
            Some(CancelReason::User),
        );
        let page = inst.read_output_from(0, 10);
        assert_eq!(page.text, "head");
        assert_eq!(page.next_offset, 1000);
        assert!(page.lossy);
        assert_eq!(page.skipped_bytes, 996);
        let empty = inst.read_output_from(page.next_offset, 10);
        assert!(empty.text.is_empty());
        assert!(!empty.lossy);
        assert_eq!(empty.next_offset, 1000);
        std::fs::remove_file(path).unwrap();
        let missing = inst.read_output_from(0, 10);
        assert!(missing.text.is_empty());
        assert!(missing.lossy);
        assert_eq!(missing.skipped_bytes, 1000);
        assert_eq!(missing.next_offset, 1000);
        assert_eq!(inst.info.status, JobStatus::Killed);
        assert_eq!(inst.info.detail.as_deref(), Some("kept"));
        assert_eq!(inst.info.total_output_bytes, 1000);
        assert_eq!(inst.cancel_reason, Some(CancelReason::User));
        assert!(inst.settled_notified);
        let empty_old = JobInstance::restore(original.info, None, 0, None);
        let read = empty_old.read_output_from(0, 0);
        assert!(!read.lossy);
        assert_eq!(read.next_offset, 0);
        assert_eq!(empty_old.info.status, JobStatus::Killed);
    }

    #[test]
    fn bounded_reader_retries_short_reads_and_keeps_prefix_on_error() {
        struct ShortReader {
            bytes: std::io::Cursor<Vec<u8>>,
            calls: usize,
        }
        impl Read for ShortReader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.calls += 1;
                match self.calls {
                    1 => Err(std::io::ErrorKind::Interrupted.into()),
                    4 => Err(std::io::ErrorKind::Other.into()),
                    _ => {
                        let n = buf.len().min(2);
                        self.bytes.read(&mut buf[..n])
                    }
                }
            }
        }
        let mut short = ShortReader {
            bytes: std::io::Cursor::new(b"abcdef".to_vec()),
            calls: 0,
        };
        assert_eq!(read_bounded(&mut short, 5), b"abcd");
        let mut reader = std::io::Cursor::new(b"abcdef".to_vec());
        assert_eq!(read_bounded(&mut reader, 3), b"abc");
        assert_eq!(reader.position(), 3);
    }

    #[test]
    fn finalize_is_idempotent() {
        let mut inst = instance("job_fin_1");
        inst.finalize(JobStatus::Completed);
        assert_eq!(inst.info.status, JobStatus::Completed);
        // 后到的 kill / 断连结算不得覆盖已落定的状态
        inst.finalize(JobStatus::Killed);
        assert_eq!(inst.info.status, JobStatus::Completed);
        assert!(inst.info.finished_at_millis.is_some());
    }

    #[test]
    fn display_command_is_truncated_into_info() {
        let (tx, _) = watch::channel(0);
        let long = "a".repeat(300);
        let inst = JobInstance::new(
            "job_disp_1".into(),
            2,
            "s".into(),
            None,
            None,
            None,
            long,
            tx,
        );
        assert!(inst.info.command.chars().count() <= 121);
        assert!(inst.info.description.contains("Background execution"));
    }

    #[test]
    fn parse_filter_matches_known_statuses_only() {
        assert_eq!(JobStatus::parse_filter("running"), Some(JobStatus::Running));
        assert_eq!(JobStatus::parse_filter("killed"), Some(JobStatus::Killed));
        assert_eq!(JobStatus::parse_filter("bogus"), None);
    }

    #[test]
    fn spill_file_name_is_unique_per_instance() {
        // 两个同 id 实例（模拟进程重启后计数器归零）应得到不同溢出文件名
        let a = instance("job_1");
        // 强制不同的启动毫秒
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = instance("job_1");
        assert_ne!(a.info.started_at_millis, b.info.started_at_millis);
        let dir = test_temp_dir("names");
        assert_ne!(
            dir.join(format!("marcel-job-job_1-{}.log", a.info.started_at_millis)),
            dir.join(format!("marcel-job-job_1-{}.log", b.info.started_at_millis))
        );
    }
}
