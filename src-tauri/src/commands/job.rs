//! 后台作业的 Tauri commands。
//!
//! 作业本体由 [`crate::command_exec::CommandExecutionManager`] 统一管理
//! （与前台执行共用注册表 / 取消 / 断连级联），这里只是 IPC 门面。
//!
//! **两台同构的 manager**：远端（`AppState.command_exec`）与本机
//! （`AppState.local_command_exec`，id 前缀 `local_job_`）。界面只有一份作业
//! 列表，所以本模块每个命令都要把两边一起覆盖：
//! - `job_list` 合并两边的列表（界面本来就该看到用户机器上的全部作业）；
//! - `job_kill` 按 id 形状路由——列表里列得出的作业，「终止」按钮就必须
//!   终止得了；
//! - `job_pending_notice` / `job_ack_notice` 合并两边的唤醒结局：本机作业
//!   同样「活得比回合久」，它的结局也得在回合结束后把模型叫起来。

use serde::Serialize;
use tauri::State;

use crate::agent::tools::job_ops::{is_local_job_id, merge_job_lists};
use crate::command_exec::{CancelReason, CommandExecutionManager, JobCaller, JobFilter, JobInfo};
use crate::error::AppError;
use crate::AppState;

/// 一条待交付给模型的「作业结算告知」：文本 + 它覆盖的作业 id。
///
/// 两个字段都要：文本交给模型，id 用来在**真的送出去之后**确认
/// （`job_ack_notice`）——只置一次 `settled_notified` 的话，一次「问了但没送
/// 出去」（会话已关、应用正要退出、自动继续的额度用尽）就会把结局吞掉。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobNotice {
    pub text: String,
    pub job_ids: Vec<String>,
}

/// 按 id 形状解析处理它的 manager：本机 id → 本机 manager，其余 → 远端。
///
/// 判据是 agent 工具侧（`agent/tools/job_ops.rs`）同一条
/// [`is_local_job_id`]（前缀只在
/// [`CommandExecutionManager::LOCAL_JOB_ID_PREFIX`] 定义一次），两端不会各认一套。
///
/// **界面侧不需要工具侧那套「ctx 侧别」象限判断**：界面手里两台 manager 都在
/// （`AppState.command_exec` + `AppState.local_command_exec`），拿到的 id 只可能
/// 来自合并列表 / 事件载荷，哪一侧的 id 都路由得到真正持有它的 manager，不存在
/// 「这一侧读不到」的可达路径。那格失败只存在于 agent 侧：本机 ctx 只挂着一台
/// manager 句柄（本机 manager），远端 id 在它手里无处可查——照 id 形状路由会落到
/// 本机 manager 上，把「在另一侧」说成「本进程查无此作业」（见 `job_ops.rs` 的
/// `resolve_job_side`）。
fn manager_for_job<'a>(state: &'a AppState, job_id: &str) -> &'a CommandExecutionManager {
    if is_local_job_id(job_id) {
        &state.local_command_exec
    } else {
        &state.command_exec
    }
}

/// 两台 manager 的待播报结局合并（按启动时间升序，与各自
/// `pending_job_notices` 的内部排序一致，合并后的先后与实际发生顺序相同）。
///
/// 本机作业与远端作业同构：只读 `state.command_exec` 时本机作业的结局永远
/// 送不出去（会话结束了也没人把模型叫起来读输出）。
fn pending_notices(
    remote: &CommandExecutionManager,
    local: &CommandExecutionManager,
    conversation_id: &str,
) -> Vec<JobInfo> {
    let mut jobs = remote.pending_job_notices(conversation_id);
    jobs.extend(local.pending_job_notices(conversation_id));
    jobs.sort_by_key(|j| j.started_at_millis);
    jobs
}

/// 确认这批结局已交给模型：两台 manager 都要确认。
///
/// 同一条 id 只可能属于其中一边（前缀把两边的号段分开），所以两个返回值
/// 相加不会重复计数；返回语义与单台一致 —— 真的被确认（此前未被任何路径
/// 消费）的条数。
fn ack_notices(
    remote: &CommandExecutionManager,
    local: &CommandExecutionManager,
    job_ids: &[String],
) -> usize {
    remote.ack_job_notices(job_ids) + local.ack_job_notices(job_ids)
}

#[tauri::command]
pub async fn job_list(
    state: State<'_, AppState>,
    session_id: Option<String>,
    status: Option<String>,
) -> Result<Vec<JobInfo>, AppError> {
    // session_id 为空 = 拉取全部会话的作业（界面按机器筛时传会话）。
    // 界面不设归属围栏（用户本来就该看到自己机器上的全部作业，包括上一次
    // 应用运行留下、已恢复成 interrupted 的那些）；Agent 的 job_list 工具
    // 走 tools/job_ops.rs，按对话归属过滤，两者语义不同、互不影响。
    //
    // 两台 manager 的列表合并成一份（界面 / 移动端只有一份作业列表，里面
    // 既有远端 `job_N` 也有本机 `local_job_N`）。本机作业的会话是哨兵
    // "local"、不属于任何 SSH 会话，所以按机器筛（真会话 id）时它不会混进
    // 某台机器的列表；拉全部时两边都在。过滤条件两边一致，合并后按启动
    // 时间统一排一次（与 agent 工具侧共用同一份合并规则）。
    let status_filter = status.as_deref();
    let remote = state
        .command_exec
        .list_jobs(
            match session_id.as_deref() {
                Some(sid) => JobFilter::Session(sid),
                None => JobFilter::All,
            },
            status_filter,
        )
        .await;
    let local = state
        .local_command_exec
        .list_jobs(
            match session_id.as_deref() {
                Some(sid) => JobFilter::Session(sid),
                None => JobFilter::All,
            },
            status_filter,
        )
        .await;
    Ok(merge_job_lists(remote, local))
}

#[tauri::command]
pub async fn job_kill(state: State<'_, AppState>, job_id: String) -> Result<JobInfo, AppError> {
    // 该 command 只由前端「终止」按钮调用（任务抽屉 / 移动端作业列表），
    // 是真·用户手动终止 → User。Agent 的 job_kill 工具走
    // tools/job_ops.rs，传 CancelReason::Agent，两者绝不混用。
    //
    // 按 id 形状路由：`job_list` 把两台 manager 合并展示，终止就必须同样
    // 覆盖两边——否则界面点了本机作业的「终止」只会得到一句查无此作业。
    manager_for_job(&state, &job_id)
        .kill_job(&job_id, CancelReason::User, JobCaller::Unscoped)
        .await
}

/// 取某会话「作业跑完了、但还没告诉过模型」的结局（没有则 `None`）。
///
/// 前端在两种时机调用：① 收到某条作业的 `job://updated`（终态）；② 某轮
/// 结束（Done / Cancelled / Failed）—— 后者关掉「作业恰好在回合收尾那一刻
/// 结算、通知谁都错过」的竞态窗口。
///
/// 拿到文本之后由前端开一轮把告知交给模型（`agent_start_task` 带
/// `origin: "job_notice"`），成功后调 [`job_ack_notice`] 确认。**只读**：
/// 光调用它不会消费结局。
#[tauri::command]
pub async fn job_pending_notice(
    state: State<'_, AppState>,
    conversation_id: String,
) -> Result<Option<JobNotice>, AppError> {
    let jobs = pending_notices(
        &state.command_exec,
        &state.local_command_exec,
        &conversation_id,
    );
    if jobs.is_empty() {
        return Ok(None);
    }
    Ok(Some(JobNotice {
        text: crate::agent::agent_loop::build_job_settlement_notice(&jobs),
        job_ids: jobs.into_iter().map(|j| j.job_id).collect(),
    }))
}

/// 确认这些作业的结局已经交给模型（此后不再播报）。返回真的被确认的条数
/// ——已被别的路径（`job_output` 读到终态、agent 自己 job_kill）消费过的
/// 不计入，那不是失败。
#[tauri::command]
pub async fn job_ack_notice(
    state: State<'_, AppState>,
    job_ids: Vec<String>,
) -> Result<usize, AppError> {
    Ok(ack_notices(
        &state.command_exec,
        &state.local_command_exec,
        &job_ids,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command_exec::executor::{ExecOutcome, ExecTransport};
    use crate::command_exec::{CommandSource, CommandTicket, JobStatus};
    use async_trait::async_trait;
    use std::sync::Arc;
    use tauri::AppHandle;
    use tokio::sync::watch;

    /// 立即完成的 mock 传输层（真 SSH 不参与）：作业一提交就结算，用来钉住
    /// 「两边的结局都能取出、ack 之后不重复、别的对话不受影响」。
    struct InstantTransport;

    #[async_trait]
    impl ExecTransport for InstantTransport {
        async fn exec(
            &self,
            _ticket: &CommandTicket,
            _app: Option<&AppHandle>,
            _cancel: Option<&watch::Receiver<CancelReason>>,
        ) -> Result<ExecOutcome, AppError> {
            Ok(ExecOutcome::Completed {
                output: String::new(),
                exit: Default::default(),
            })
        }
    }

    /// 远端 manager（默认前缀）。
    fn remote_manager() -> CommandExecutionManager {
        CommandExecutionManager::with_transport(Arc::new(InstantTransport))
    }

    /// 本机 manager（`local_job_` 前缀，与 lib.rs 的生产构造同一形态）。
    fn local_manager() -> CommandExecutionManager {
        CommandExecutionManager::with_transport(Arc::new(InstantTransport))
            .with_id_prefix(CommandExecutionManager::LOCAL_JOB_ID_PREFIX)
    }

    /// 派一条作业并等它结算（worker 在独立任务里跑，给调度留时间）。
    async fn submit_settled(mgr: &CommandExecutionManager, session: &str, owner: &str) -> JobInfo {
        let info = mgr
            .submit_background(
                None,
                CommandTicket::new(session, "build", CommandSource::Agent)
                    .cancellable("task-notice", "构建")
                    .owned_by(owner),
                None,
            )
            .await
            .unwrap();
        for _ in 0..400 {
            if mgr.job_status(&info.job_id).await.unwrap() != JobStatus::Running {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        info
    }

    #[tokio::test]
    async fn pending_notices_cover_both_managers_and_ack_is_idempotent() {
        let remote = remote_manager();
        let local = local_manager();
        let r = submit_settled(&remote, "s1", "conv-a").await;
        let l = submit_settled(&local, "local", "conv-a").await;
        assert_eq!(r.job_id, "job_1");
        assert_eq!(l.job_id, "local_job_1", "本机 id 必须有前缀，不能撞远端");

        // 两边的结局都能取出——只读 `command_exec` 时本机那条永远送不出去
        let pending = pending_notices(&remote, &local, "conv-a");
        assert_eq!(pending.len(), 2, "{pending:?}");
        assert!(pending.iter().any(|j| j.job_id == r.job_id));
        assert!(pending.iter().any(|j| j.job_id == l.job_id));
        // 只读：再取一次仍是两条（送不出去就不算已读）
        assert_eq!(pending_notices(&remote, &local, "conv-a").len(), 2);

        // 两边都确认；同一批再确认不再计数（幂等）
        let ids: Vec<String> = pending.iter().map(|j| j.job_id.clone()).collect();
        assert_eq!(ack_notices(&remote, &local, &ids), 2);
        assert!(pending_notices(&remote, &local, "conv-a").is_empty());
        assert_eq!(ack_notices(&remote, &local, &ids), 0);

        // 别的对话不受影响（围栏在 manager 内部，两台语义相同）
        assert!(pending_notices(&remote, &local, "conv-b").is_empty());
    }

    #[test]
    fn kill_route_judges_by_id_shape() {
        // 界面「终止」按钮的路由判据：只有本机前缀才是本机作业。
        assert!(is_local_job_id("local_job_1"));
        assert!(!is_local_job_id("job_1"));
        assert!(!is_local_job_id(""));
        assert!(!is_local_job_id("x_local_job_1"));
    }
}
