//! 本机（用户这台电脑）命令执行的传输层。
//!
//! 与远端 [`super::executor::SshExecTransport`] **完全同构**：同一个
//! [`super::executor::ExecTransport`] trait、同一套结果类型（[`ExecExit`] /
//! [`ExecOutcome`]）、同一套收尾语义。差异只在「通道」是什么：
//!
//! - 远端：一条 SSH exec channel（stdout + stderr 在同一条通道上回流）；
//! - 本机：一个 Windows PowerShell / `bash` 子进程，stdout 与 stderr 各是一个管道。
//!
//! ## 收尾语义必须与远端一字不差（见 `executor.rs` 顶部模块注释）
//!
//! 超时 / 取消 = **停止等待**：丢弃我们这侧的读端句柄（等价于远端「关闭通道」
//! ——子进程之后往 stdout/stderr 写会因管道断开出错），**绝不杀进程**。静默
//! 运行、重定向了输出、被 detached 起来的进程会继续在本机跑完；残余进程由
//! agent 自己用 `tasklist` / `Get-Process` 配合 `Stop-Process -Id`（Windows）
//! 或 `ps` / `pgrep` 配合 `kill <pid>` 清理。对外文案因此必须写「已停止等待、
//! 不保证已终止」（见 `agent/tools/bash.rs` 的超时说明）。
//!
//! 本层因此**刻意不采用** `agent/tools/browser_cdp.rs` 那套 kill-on-drop：
//! 那是「自己起的短命子进程、退出即弃」的另一种设计，与本体系的长任务语义
//! 不同。
//!
//! ## 唯一必须不同于远端的一处：收尸
//!
//! 远端进程由 sshd 回收；本机子进程如果没人 `wait()`，退出后是僵尸（Unix）
//! 或句柄泄漏（Windows）。所以起进程之后立刻挂一个 detached reaper
//! （`child.wait()`），它只回收退出状态、**不发任何信号**——收尸不是杀进程。
//! 超时 / 取消时我们丢掉读端走人，reaper 留在后台把子进程收干净。
//!
//! ## 输出编码
//!
//! Windows 控制台代码页默认不是 UTF-8（简体中文机器上是 CP936），Windows PowerShell 按它把
//! 中文写进管道而我们按 UTF-8 解码 → 全是乱码。所以用户命令**前面**拼一段设置
//! 输出编码的前置语句（[`WINDOWS_UTF8_PRELUDE`]），再按 UTF-8 解码读到的字节；
//! 真·非法字节按 `String::from_utf8_lossy` 处理（与远端一致）。跨读取块被切开
//! 的多字节字符由 [`Utf8Carry`] 接起来，否则一个「你」字会解成两个 U+FFFD。

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use tauri::AppHandle;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::sync::watch;

use crate::emit_event;
use crate::error::AppError;

use super::executor::{ChunkCallback, ExecExit, ExecOutcome, ExecTransport, OutputSink};
use super::ticket::{CancelReason, CommandTicket};

/// 单次读取的缓冲大小（一个 chunk 对应一次 read）。
const READ_BUFFER_BYTES: usize = 16 * 1024;

/// Windows PowerShell 的输出编码前置语句。
///
/// `[Console]::OutputEncoding` 决定 Windows PowerShell 往 stdout 写字节时用哪种编码（机器
/// 默认是控制台代码页），`$OutputEncoding` 决定它给外部程序喂 stdin 用哪种。
/// 两者都设成 UTF-8，我们才能按 UTF-8 解出中文。
///
/// 末尾带 `;`，所以直接接用户命令即可——**不能**加换行、`&&`、`|`：那会改变
/// 用户复合命令的语义（这里只是两条独立语句）。
///
/// 非 Windows 构建里用不到（那边的输出本来就是 UTF-8），`allow(dead_code)`
/// 只为不在 Linux / Android 上冒出「never used」。
#[cfg_attr(not(windows), allow(dead_code))]
const WINDOWS_UTF8_PRELUDE: &str =
    "[Console]::OutputEncoding=[Text.Encoding]::UTF8; $OutputEncoding=[Text.Encoding]::UTF8;";

/// Windows shell：固定 `powershell`（Windows PowerShell **5.1**，Windows 出厂自带）。
///
/// 产品决定：只用 5.1，不考虑 PowerShell 7（pwsh）——5.1 每台 Windows 都有，
/// 不必先让用户装一个 shell 才能用本机能力。代价是 5.1 没有 `&&` / `||` 链式
/// 操作符，工具描述里已提醒模型用 `;` 或分步执行。
#[cfg(windows)]
const WINDOWS_SHELL_PROGRAM: &str = "powershell";
#[cfg(windows)]
const WINDOWS_SHELL_ARGS: &[&str] = &["-NoProfile", "-NonInteractive", "-Command"];

/// 其它平台：`bash -lc`（登录 shell，与用户交互时的 PATH / 环境一致）。
#[cfg(not(windows))]
const POSIX_SHELL_PROGRAM: &str = "bash";
#[cfg(not(windows))]
const POSIX_SHELL_ARGS: &[&str] = &["-lc"];
/// 没有 bash 时的退路（精简镜像 / 非 glibc 系统）。**只有这一种情况退**。
#[cfg(not(windows))]
const POSIX_SH_ARGS: &[&str] = &["-c"];

/// 本机 shell 的启动形态。
struct ShellSpec {
    program: &'static str,
    /// 固定参数（不含命令文本）。
    args: &'static [&'static str],
    /// 输出编码等前置语句（空串 = 不需要）。
    prelude: &'static str,
}

impl ShellSpec {
    /// 生产形态。
    fn platform() -> Self {
        #[cfg(windows)]
        {
            Self {
                program: WINDOWS_SHELL_PROGRAM,
                args: WINDOWS_SHELL_ARGS,
                prelude: WINDOWS_UTF8_PRELUDE,
            }
        }
        #[cfg(not(windows))]
        {
            Self {
                program: POSIX_SHELL_PROGRAM,
                args: POSIX_SHELL_ARGS,
                prelude: "",
            }
        }
    }

    /// 前置语句 + 用户命令：用 `;` 分隔（PowerShell 与 sh 都认），语义不变。
    fn script(&self, command: &str) -> String {
        format!("{}{}", self.prelude, command)
    }

    /// 起这个 shell 跑 `command`。
    fn spawn(&self, command: &str) -> Result<Child, AppError> {
        let script = self.script(command);
        match spawn_process(self.program, &with_script(self.args, &script)) {
            Ok(child) => Ok(child),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // 没有 bash（精简镜像 / 非 glibc 系统）：退到 POSIX `sh -c`。
                // 只有这一种情况退，别的错误（权限、资源不足）如实上抛。
                #[cfg(not(windows))]
                {
                    spawn_process("sh", &with_script(POSIX_SH_ARGS, &script))
                        .map_err(|error| spawn_error("sh", error))
                }
                #[cfg(windows)]
                {
                    Err(spawn_error(self.program, error))
                }
            }
            Err(error) => Err(spawn_error(self.program, error)),
        }
    }

    /// 测试形态：就是生产形态（Windows 上 `powershell` 必定存在），只有 POSIX
    /// 侧保留一条 `sh` 退路（精简镜像可能没有 bash）。测试要验的是**这套执行
    /// 机制**（读循环 / 超时 / 取消 / 编码 / 收尸），生产代码不做额外退让。
    #[cfg(test)]
    fn for_tests() -> Self {
        #[allow(unused_mut)]
        let mut candidates = vec![Self::platform()];
        #[cfg(not(windows))]
        candidates.push(Self {
            program: "sh",
            args: POSIX_SH_ARGS,
            prelude: "",
        });
        candidates
            .into_iter()
            .find(|spec| program_on_path(spec.program))
            .unwrap_or_else(Self::platform)
    }
}

/// shell 的固定参数 + 命令文本（命令永远是最后一个参数）。
fn with_script<'a>(args: &[&'a str], script: &'a str) -> Vec<&'a str> {
    let mut all: Vec<&'a str> = args.to_vec();
    all.push(script);
    all
}

/// 起进程的公共路径。
///
/// stdin 关掉（本机执行是非交互的，应用也没有终端可给它读）；stdout/stderr
/// 都接管成管道；`kill_on_drop(false)` 明写出来锁住「超时 / 取消都不杀进程」
/// 这条语义（默认就是 false，这里不让它被无意改掉）。
fn spawn_process(program: &str, args: &[&str]) -> Result<Child, std::io::Error> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(false);
    #[cfg(windows)]
    {
        // CREATE_NO_WINDOW：否则每次本机执行都会闪一个控制台黑窗（先例：
        // `updater/install.rs` 的静默安装器）。
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command.spawn()
}

/// 起进程失败的错误映射。
///
/// 「找不到这个程序」必须说清要装什么，不能把 `program not found` 原样抛给
/// 用户（模型会拿它去瞎猜）；其它 IO 错误如实上抛。
fn spawn_error(program: &str, error: std::io::Error) -> AppError {
    if error.kind() == std::io::ErrorKind::NotFound {
        return AppError::Other(format!(
            "本机命令执行失败：找不到 `{}`。Marcel SSH 通过它执行本机命令（不会静默改用别的 shell），\
             请先安装并确保它在 PATH 上。原始错误：{}",
            program, error
        ));
    }
    AppError::Io(error)
}

/// 程序是否在 PATH 上（`for_tests` 选 shell 用，只做文件存在性判断）。
#[cfg(test)]
fn program_on_path(program: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths)
        .any(|dir| dir.join(program).is_file() || dir.join(format!("{}.exe", program)).is_file())
}

/// 子进程退出事实 → [`ExecExit`]（与远端同构：只如实上报，绝不猜）。
fn exit_from_status(status: &std::process::ExitStatus) -> ExecExit {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(signal) = status.signal() {
            // 被信号打死时退出码没有意义（与远端 `ExitSignal` 处理一致）。
            return ExecExit {
                code: None,
                signal: Some(signal_name(signal)),
            };
        }
    }
    ExecExit {
        // Windows 的退出码是 u32 语义：`code()` 给的是有符号解释，取位模式。
        code: status.code().map(|code| code as u32),
        signal: None,
    }
}

/// 信号号 → 名字，与远端 `signal: KILL` 的形态对齐；不认识的信号报编号
/// （不编名字）。
#[cfg(unix)]
fn signal_name(signal: i32) -> String {
    match signal {
        1 => "HUP".to_string(),
        2 => "INT".to_string(),
        3 => "QUIT".to_string(),
        6 => "ABRT".to_string(),
        9 => "KILL".to_string(),
        11 => "SEGV".to_string(),
        13 => "PIPE".to_string(),
        14 => "ALRM".to_string(),
        15 => "TERM".to_string(),
        other => other.to_string(),
    }
}

/// 跨读取块的 UTF-8 解码器。
///
/// 一次 `read` 可能恰好把一个多字节字符切成两半（大块输出 / 块边界），两边
/// 各自 `from_utf8_lossy` 就都解成 U+FFFD，中文输出中间冒出「�」。这里把末尾
/// 那段「合法的、还没读完的前缀」（最多 3 字节）留到下一块一起解。
///
/// **每个流各持一份**：stdout 与 stderr 是两股独立字节流，交错时共用会把
/// stdout 的半截序列粘到 stderr 的开头上。
///
/// 真·非法字节（不是合法前缀的残缺）不跨块保留，直接交给
/// `String::from_utf8_lossy`——与远端逐 chunk 解码的行为一致。
#[derive(Default)]
struct Utf8Carry {
    pending: Vec<u8>,
}

impl Utf8Carry {
    /// 解码一块字节，返回本次可以交付的文本（可能为空）。
    fn decode(&mut self, bytes: &[u8]) -> String {
        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(bytes);
        match std::str::from_utf8(&buf) {
            Ok(text) => text.to_string(),
            Err(error) if error.error_len().is_none() => {
                // 末尾是一段合法的完整前缀（序列还没写完）：留给下一块。
                // 前半段是合法的，lossy 不会产生替换字符。
                let split = error.valid_up_to();
                let (text, tail) = buf.split_at(split);
                self.pending.extend_from_slice(tail);
                String::from_utf8_lossy(text).into_owned()
            }
            Err(_) => String::from_utf8_lossy(&buf).into_owned(),
        }
    }

    /// 流结束时取走尾巴：此刻留着的就是真·残缺数据，按 lossy 交付，不吞掉。
    fn flush(&mut self) -> String {
        String::from_utf8_lossy(&std::mem::take(&mut self.pending)).into_owned()
    }
}

/// 读循环的一步。
enum ReadStep {
    /// 某个流读到 n 字节（n = 0 表示该流到达 EOF）。
    Read(Stream, std::io::Result<usize>),
    Cancelled,
    TimedOut,
}

/// 两个管道都 EOF 后，等退出事实的一步。
enum WaitStep {
    Exited(
        Result<std::io::Result<std::process::ExitStatus>, tokio::sync::oneshot::error::RecvError>,
    ),
    Cancelled,
    TimedOut,
}

#[derive(Debug, Clone, Copy)]
enum Stream {
    Stdout,
    Stderr,
}

/// 读一块；该流已经 EOF 时返回一个永不就绪的 future（在 select 臂上停住，
/// 而不是空转地反复读已关闭的句柄）。
async fn read_or_pending<R: AsyncRead + Unpin>(
    reader: &mut R,
    buf: &mut [u8],
    done: bool,
) -> std::io::Result<usize> {
    if done {
        return std::future::pending().await;
    }
    reader.read(buf).await
}

/// 取消臂：`cancel = None` 时永不就绪（`pending`），与远端 `read_channel` 同构。
async fn cancelled(cancel: Option<&watch::Receiver<CancelReason>>) {
    match cancel {
        // watch::Receiver 可克隆：克隆体订阅同一通道，仅供本臂等待。
        Some(rx) => {
            let mut rx = rx.clone();
            let _ = rx.changed().await;
        }
        None => std::future::pending().await,
    }
}

/// 取消信号对应的终止来源（与远端同构：拿不到就按 User，不编来源）。
fn cancel_reason(cancel: Option<&watch::Receiver<CancelReason>>) -> CancelReason {
    cancel.map(|rx| *rx.borrow()).unwrap_or(CancelReason::User)
}

/// 交付一个 chunk：进 sink（前台收集 / 后台沉淀），并在声明了流式目标时发
/// `{type:"toolOutput", toolCallId, chunk}` 事件（与远端逐字节同协议）。
fn deliver<O: OutputSink>(
    output: &mut O,
    chunk: &str,
    streaming: Option<(&AppHandle, &str, &str)>,
) {
    if chunk.is_empty() {
        return;
    }
    output.push(chunk);
    if let Some((app, event_name, stream_id)) = streaming {
        emit_event(
            app,
            event_name,
            &serde_json::json!({
                "type": "toolOutput",
                "toolCallId": stream_id,
                "chunk": chunk,
            }),
        );
    }
}

/// 两个解码器的尾巴一并交付（超时 / 取消 / 正常结束都要做，否则末尾最多 3
/// 字节被吞）。
fn flush_decoders<O: OutputSink>(
    output: &mut O,
    stdout: &mut Utf8Carry,
    stderr: &mut Utf8Carry,
    streaming: Option<(&AppHandle, &str, &str)>,
) {
    deliver(output, &stdout.flush(), streaming);
    deliver(output, &stderr.flush(), streaming);
}

/// 生产入口：按平台形态起 shell，再把余下的事交给 [`read_child`]。
async fn run_local<O: OutputSink>(
    ticket: &CommandTicket,
    streaming: Option<(&AppHandle, &str, &str)>,
    output: O,
    cancel: Option<&watch::Receiver<CancelReason>>,
) -> Result<ExecOutcome<O::Output>, AppError> {
    let child = ShellSpec::platform().spawn(&ticket.command)?;
    read_child(child, ticket.timeout, streaming, output, cancel).await
}

/// 起进程之后的一切：读 stdout/stderr、流式交付、超时 / 取消收尾、等退出码
/// （本机版 [`super::executor::run_raw`]，语义逐条对齐）。
///
/// - `streaming = Some((app, event_name, stream_id))` 时每个 chunk 发射
///   `{type:"toolOutput", ...}`；
/// - `output` 决定输出归属：`String` 收集前台结果，`ChunkCallback` 逐块交给
///   后台作业（执行层不重复保存）；
/// - stdout 与 stderr **合并进同一个 sink，顺序按实际到达**（与远端把
///   Data / ExtendedData 合流一致）；
/// - 取消、超时与数据由 biased select 竞争，**优先级是「取消 → 超时 → 数据」**；
///   取消 / 超时都只丢弃读端 + 交给 reaper 收尸，**不杀进程**（见模块注释）。
async fn read_child<O: OutputSink>(
    mut child: Child,
    timeout: Duration,
    streaming: Option<(&AppHandle, &str, &str)>,
    mut output: O,
    cancel: Option<&watch::Receiver<CancelReason>>,
) -> Result<ExecOutcome<O::Output>, AppError> {
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| AppError::Other("本机子进程没有 stdout 管道".into()))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| AppError::Other("本机子进程没有 stderr 管道".into()))?;

    // 收尸：起进程之后立刻挂一个 detached reaper 等它退出，退出状态经 oneshot
    // 交回来。超时 / 取消时我们丢弃读端走人，reaper 仍留在后台把子进程收干净
    // （没人 wait 的退出子进程会变僵尸）。**它不向子进程发任何信号。**
    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let _ = exit_tx.send(child.wait().await);
    });

    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);

    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut stdout_decoder = Utf8Carry::default();
    let mut stderr_decoder = Utf8Carry::default();
    let mut stdout_buf = vec![0u8; READ_BUFFER_BYTES];
    let mut stderr_buf = vec![0u8; READ_BUFFER_BYTES];
    // 两个流都有数据时逐轮交替优先顺序：`biased` 用来保证「取消优先」，但两个
    // 数据臂若固定顺序，「一侧永不停歇地有输出」会把另一侧饿死（远端两股流合进
    // 同一条通道，不存在这个问题，这里用交替补上）。
    let mut prefer_stdout = true;

    loop {
        if stdout_done && stderr_done {
            break;
        }
        // 臂序是正确性的一部分，**不许把 deadline 挪到数据臂之后**（`biased`
        // 按下标顺序 poll）：命令持续满速输出时每一轮数据臂都就绪，排在后面的
        // 超时 future 永远不会被 poll，到点也不会停——超时形同不存在。放在
        // 数据臂之前才保证「任何情况下到点就停等待」；取消仍排在最前（取消优先）。
        let step = if prefer_stdout || stderr_done {
            tokio::select! {
                biased;
                _ = cancelled(cancel) => ReadStep::Cancelled,
                _ = &mut deadline => ReadStep::TimedOut,
                read = read_or_pending(&mut stdout, &mut stdout_buf, stdout_done) => {
                    ReadStep::Read(Stream::Stdout, read)
                }
                read = read_or_pending(&mut stderr, &mut stderr_buf, stderr_done) => {
                    ReadStep::Read(Stream::Stderr, read)
                }
            }
        } else {
            tokio::select! {
                biased;
                _ = cancelled(cancel) => ReadStep::Cancelled,
                _ = &mut deadline => ReadStep::TimedOut,
                read = read_or_pending(&mut stderr, &mut stderr_buf, stderr_done) => {
                    ReadStep::Read(Stream::Stderr, read)
                }
                read = read_or_pending(&mut stdout, &mut stdout_buf, stdout_done) => {
                    ReadStep::Read(Stream::Stdout, read)
                }
            }
        };
        prefer_stdout = !prefer_stdout;

        match step {
            ReadStep::Cancelled => {
                // 丢弃读端 = 等价于远端「关闭通道」：子进程之后往这两个管道写
                // 会因管道断开出错。**不 kill 进程**（见模块注释），收尸交给
                // 上面的 reaper。
                //
                // 与超时路径一样先 flush 解码器：取消可能正好落在多字节字符
                // 中间，不 flush 就会把已经从子进程收到的半截前缀悄悄吞掉
                // （后台作业的 sink 里会缺这一下；前台 String 收集器本来就不
                // 随 `Cancelled` 交付，flush 对它无副作用）。
                flush_decoders(
                    &mut output,
                    &mut stdout_decoder,
                    &mut stderr_decoder,
                    streaming,
                );
                drop(stdout);
                drop(stderr);
                return Ok(ExecOutcome::Cancelled {
                    reason: cancel_reason(cancel),
                });
            }
            ReadStep::TimedOut => {
                // 超时保留已收到的部分输出（与远端 `TimedOut` 一致）。
                flush_decoders(
                    &mut output,
                    &mut stdout_decoder,
                    &mut stderr_decoder,
                    streaming,
                );
                drop(stdout);
                drop(stderr);
                return Ok(ExecOutcome::TimedOut {
                    output: output.finish(),
                });
            }
            ReadStep::Read(stream, Ok(0)) => match stream {
                Stream::Stdout => stdout_done = true,
                Stream::Stderr => stderr_done = true,
            },
            ReadStep::Read(stream, Ok(n)) => {
                let (bytes, decoder) = match stream {
                    Stream::Stdout => (&stdout_buf[..n], &mut stdout_decoder),
                    Stream::Stderr => (&stderr_buf[..n], &mut stderr_decoder),
                };
                let text = decoder.decode(bytes);
                deliver(&mut output, &text, streaming);
            }
            ReadStep::Read(_, Err(error)) => {
                // 读管道失败（罕见）：已交付的输出照旧，错误如实上抛——与远端
                // 「测到断连就报错、不把部分输出伪装成正常完成」同一条纪律。
                return Err(AppError::Io(error));
            }
        }
    }

    // 两侧管道都到 EOF：子进程通常已经退出，但退出状态可能还没回来（比如它
    // 先关掉自己的输出、再继续跑），所以这一步同样与取消、超时竞争。
    flush_decoders(
        &mut output,
        &mut stdout_decoder,
        &mut stderr_decoder,
        streaming,
    );
    let wait = tokio::select! {
        biased;
        _ = cancelled(cancel) => WaitStep::Cancelled,
        status = exit_rx => WaitStep::Exited(status),
        _ = &mut deadline => WaitStep::TimedOut,
    };

    match wait {
        WaitStep::Cancelled => Ok(ExecOutcome::Cancelled {
            reason: cancel_reason(cancel),
        }),
        WaitStep::TimedOut => Ok(ExecOutcome::TimedOut {
            output: output.finish(),
        }),
        WaitStep::Exited(Ok(Ok(status))) => Ok(ExecOutcome::Completed {
            output: output.finish(),
            exit: exit_from_status(&status),
        }),
        WaitStep::Exited(Ok(Err(error))) => Err(AppError::Io(error)),
        WaitStep::Exited(Err(_)) => Err(AppError::Other(
            "本机子进程收尸失败（等待任务提前结束）".into(),
        )),
    }
}

/// 有 AppHandle 且 ticket 声明了流式目标时才发 chunk 事件（与
/// [`super::executor::SshExecTransport`] 同判据）。
fn streaming_target<'a>(
    ticket: &'a CommandTicket,
    app: Option<&'a AppHandle>,
) -> Option<(&'a AppHandle, &'a str, &'a str)> {
    ticket.streaming.as_ref().and_then(|stream| {
        app.map(|app| (app, stream.event_name.as_str(), stream.stream_id.as_str()))
    })
}

/// 生产传输层：在本机以 Windows PowerShell（5.1）/ `bash` 子进程执行用户命令。
pub struct LocalExecTransport;

impl LocalExecTransport {
    pub fn new() -> Self {
        Self
    }
}

impl Default for LocalExecTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ExecTransport for LocalExecTransport {
    async fn exec(
        &self,
        ticket: &CommandTicket,
        app: Option<&AppHandle>,
        cancel: Option<&watch::Receiver<CancelReason>>,
    ) -> Result<ExecOutcome, AppError> {
        let streaming = streaming_target(ticket, app);
        run_local(ticket, streaming, String::new(), cancel).await
    }

    async fn exec_observable(
        &self,
        ticket: &CommandTicket,
        app: Option<&AppHandle>,
        on_chunk: ChunkCallback,
        cancel: Option<&watch::Receiver<CancelReason>>,
    ) -> Result<ExecOutcome<()>, AppError> {
        // **必须覆盖默认实现**：默认实现会先收全文再一次性回调，后台作业的
        // 实时性（job_output(wait=true) 的尾随输出、跑长命令时逐块可见）全靠
        // 这里逐块交付。
        let streaming = streaming_target(ticket, app);
        run_local(ticket, streaming, on_chunk, cancel).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use super::super::ticket::CommandSource;

    /// 平台各自「跑一条最小命令」的写法（Windows 是 PowerShell，其它是 POSIX
    /// shell）。
    fn echo(text: &str) -> String {
        #[cfg(windows)]
        {
            format!("Write-Output '{}'", text)
        }
        #[cfg(not(windows))]
        {
            format!("printf '%s\\n' '{}'", text)
        }
    }

    /// 睡一段时间的命令（超时 / 取消测试用）。
    fn sleepy_seconds(seconds: u32) -> String {
        #[cfg(windows)]
        {
            format!("Start-Sleep -Seconds {}", seconds)
        }
        #[cfg(not(windows))]
        {
            format!("sleep {}", seconds)
        }
    }

    /// **满速**输出、且自己会停下的命令（超时测试用）。
    ///
    /// 输出时长必须明显超过测试的短超时（否则命令先结束，测的就不是超时了），
    /// 但命令最终要自己退出——测试不能在本机留下一个永远在跑的子进程。命令是
    /// 满速的：消费端只要比它慢，每一轮读循环的数据臂都会就绪，正是「超时
    /// future 被饿死」那个回归场景。
    fn fast_output_for_seconds(seconds: u32) -> String {
        #[cfg(windows)]
        {
            // 大块（64KB）写 + 时间上界到点自己退出：小块的 `Write-Output`
            // 每秒只有几 MB，会被慢消费端追上、数据臂时有时无，测不稳。
            format!(
                "$line = 'x' * 65536; $out = [Console]::Out; \
                 $end = (Get-Date).AddSeconds({seconds}); \
                 while ((Get-Date) -lt $end) {{ $out.WriteLine($line) }}"
            )
        }
        #[cfg(not(windows))]
        {
            // `yes` 满速产出；读端一关，`head` 写出错退出、`yes` 收到
            // SIGPIPE，命令自己结束（写不到 400MB 那个上界）。
            "yes 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx' | head -c 400000000".to_string()
        }
    }

    /// 只写出一个多字节字符的**前两个字节**、然后挂住的命令（取消测试用）。
    ///
    /// 「你」的 UTF-8 是 E4 BD A0：只写前两字节，解码器会把它当成「合法但还
    /// 没读完的前缀」留在 carry 里；随后命令挂住，于是取消正好落在字符中间。
    ///
    /// 写完字节后再落一个**标记文件**：调用方等标记出现再取消，这样「父进程
    /// 已经把那两字节读进 carry」是确定的，而不是靠 300ms 定时赌调度（那种写法
    /// 在全量并行跑测试时会偶发失败——子进程还没起来就被取消了，sink 为空）。
    fn partial_multibyte_then_signal(marker: &std::path::Path) -> String {
        let marker = marker.display().to_string().replace('\'', "''");
        #[cfg(windows)]
        {
            format!(
                "$out = [Console]::OpenStandardOutput(); \
                 $out.Write([byte[]](0xE4, 0xBD), 0, 2); $out.Flush(); \
                 [System.IO.File]::WriteAllText('{marker}', 'x'); Start-Sleep -Seconds 4"
            )
        }
        #[cfg(not(windows))]
        {
            format!("printf '\\344\\275'; : > '{marker}'; sleep 4")
        }
    }

    fn ticket(command: String, timeout: Duration) -> CommandTicket {
        CommandTicket::new("local-test-session", command, CommandSource::Agent).timeout(timeout)
    }

    /// 经完整执行机制跑一条命令（无 AppHandle → 不发流式事件）。shell 用
    /// [`ShellSpec::for_tests`]：优先生产形态，本机没装的生产 shell 才退到
    /// 本机存在的 shell。
    async fn run(command: String, timeout: Duration) -> ExecOutcome {
        let child = ShellSpec::for_tests()
            .spawn(&command)
            .expect("测试用 shell 必须能起来");
        read_child(child, timeout, None, String::new(), None)
            .await
            .expect("本机执行不应报基础设施错误")
    }

    #[tokio::test]
    async fn exec_runs_a_local_command_and_reports_exit_code() {
        let outcome = run(echo("hi"), Duration::from_secs(60)).await;
        match outcome {
            ExecOutcome::Completed { output, exit } => {
                assert!(output.contains("hi"), "output: {output:?}");
                assert_eq!(exit.code, Some(0));
                assert!(exit.is_success(), "exit: {exit:?}");
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[tokio::test]
    async fn nonzero_exit_is_not_a_failure_but_the_code_is_reported() {
        let outcome = run("exit 3".to_string(), Duration::from_secs(60)).await;
        match outcome {
            ExecOutcome::Completed { exit, .. } => {
                assert_eq!(exit.code, Some(3));
                assert!(!exit.is_success());
                assert_eq!(exit.describe(), "exit code: 3");
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[tokio::test]
    async fn stderr_is_merged_into_the_same_sink() {
        let command = {
            #[cfg(windows)]
            {
                "[Console]::Error.Write('boom'); exit 1".to_string()
            }
            #[cfg(not(windows))]
            {
                "printf 'boom' >&2; exit 1".to_string()
            }
        };
        let outcome = run(command, Duration::from_secs(60)).await;
        match outcome {
            ExecOutcome::Completed { output, exit } => {
                assert!(output.contains("boom"), "output: {output:?}");
                assert_eq!(exit.code, Some(1));
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[tokio::test]
    async fn timeout_keeps_partial_output() {
        // 先产出一行（显式 flush：管道不是终端，别赌缓冲），再睡到超时之后。
        // 睡 4 秒而不是 30 秒：超时只停止等待，子进程会活到自己退出，而
        // Windows 上「没人 wait 的子进程」会让测试运行时的退出等它——测试
        // 够用即止，别让它拖慢整条 `cargo test --lib`。
        let command = {
            #[cfg(windows)]
            {
                "Write-Output 'partial'; [Console]::Out.Flush(); Start-Sleep -Seconds 4".to_string()
            }
            #[cfg(not(windows))]
            {
                "printf 'partial\\n'; sleep 4".to_string()
            }
        };
        let outcome = run(command, Duration::from_secs(1)).await;
        match outcome {
            ExecOutcome::TimedOut { output } => assert!(
                output.contains("partial"),
                "超时也要保留已收到的部分输出：{output:?}"
            ),
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancel_reports_the_reason_it_was_given() {
        let (cancel_tx, cancel_rx) = watch::channel(CancelReason::User);
        // 同上：子进程只活 4 秒，够验证「取消立刻返回」又不拖慢测试。
        let child = ShellSpec::for_tests()
            .spawn(&sleepy_seconds(4))
            .expect("测试用 shell 必须能起来");
        let sender = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let _ = cancel_tx.send(CancelReason::Task);
        });
        let outcome = read_child(
            child,
            Duration::from_secs(120),
            None,
            String::new(),
            Some(&cancel_rx),
        )
        .await
        .unwrap();
        sender.await.unwrap();
        // 初始值是 User，这次发的是 Task：拿到 Task 才说明终止来源没被合并。
        assert!(matches!(
            outcome,
            ExecOutcome::Cancelled {
                reason: CancelReason::Task
            }
        ));
    }

    #[tokio::test]
    async fn chinese_output_decodes_correctly() {
        let text = "你好，世界";
        let outcome = run(echo(text), Duration::from_secs(60)).await;
        match outcome {
            ExecOutcome::Completed { output, .. } => {
                assert!(output.contains(text), "中文输出乱码：{output:?}");
                assert!(!output.contains('\u{fffd}'), "output: {output:?}");
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    /// 回归：命令**满速持续输出**时，到点也必须停等待。
    ///
    /// 曾经的臂序是「取消 → stdout → stderr → deadline」，而 `biased` 按下标
    /// 顺序 poll：数据臂每轮都就绪时，排在最后的超时 future 永远轮不到 poll，
    /// 超时形同不存在（命令不自己结束就会一直挂住）。修法是把 deadline 提到
    /// 数据臂之前，注释里那句「不许挪回去」就靠这条测试撑腰。
    ///
    /// 消费端故意比生产端慢（每块 10ms，代表后台作业 sink 的溢出文件写）：
    /// 只让生产端满速而消费端飞快时管道会被读空，超时照样触发，测不出这个
    /// 回归；消费端慢下来，管道里**始终有数据**，数据臂才会每轮都就绪。
    /// 超时也刻意长于 PowerShell 的启动（~0.4s）：否则超时落在「还没开始
    /// 输出」的空档里，同样测不出饿死。
    ///
    /// **平台差异（别被 Windows 上的绿骗了）**：Windows 上 tokio 把子进程管道
    /// 读丢给阻塞线程池（`io::blocking::Blocking`），每一轮新建的读 future 至少
    /// 有一次 `Pending`，后面的 deadline 臂必然被 poll 到——把臂序改回去这条在
    /// Windows 上也不会红。Unix 侧读是就绪驱动的（有数据立即 `Ready`），饿死才
    /// 真的发生；这条测试在那里才是臂序护栏。
    #[tokio::test]
    async fn sustained_output_cannot_starve_the_timeout() {
        let received = Arc::new(AtomicUsize::new(0));
        let counter = received.clone();
        let sink: ChunkCallback = Arc::new(move |chunk| {
            counter.fetch_add(chunk.len(), Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(10));
        });
        let child = ShellSpec::for_tests()
            .spawn(&fast_output_for_seconds(4))
            .expect("测试用 shell 必须能起来");
        let started = std::time::Instant::now();
        let outcome = read_child(child, Duration::from_millis(1500), None, sink, None)
            .await
            .expect("本机执行不应报基础设施错误");
        let elapsed = started.elapsed();
        match outcome {
            // 输出归 sink（`ChunkCallback` 的 Output 是 `()`）：已收到多少
            // 看 sink 自己记的数。
            ExecOutcome::TimedOut { output: () } => {}
            other => {
                panic!("持续满速输出时必须在到期那刻返回 TimedOut：{other:?}（用时 {elapsed:?}）")
            }
        }
        assert!(
            received.load(Ordering::Relaxed) > 0,
            "满速输出下超时也要保留已收到的输出"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "超时必须到点就返回，不能等命令自己结束：{elapsed:?}"
        );
    }

    /// 回归：取消路径也要 flush 解码器尾巴。
    ///
    /// 取消可能正好落在多字节字符中间（carry 里留着「合法但没读完」的前缀）。
    /// 只有超时 / 正常路径 flush 会把这两字节悄悄吞掉：后台作业的 sink 里缺
    /// 这一下，同一段输出「超时看得到、取消看不到」。
    #[tokio::test]
    async fn cancel_flushes_a_partial_multibyte_tail_instead_of_swallowing_it() {
        let received = Arc::new(parking_lot::Mutex::new(String::new()));
        let buffer = received.clone();
        let sink: ChunkCallback = Arc::new(move |chunk| buffer.lock().push_str(chunk));
        let (cancel_tx, cancel_rx) = watch::channel(CancelReason::User);
        // 标记文件：子进程写完那两字节才落它，测试据此确定「字节已经进了管道」，
        // 再等一小会儿让读循环把它们收进 carry —— 不靠定时赌调度。
        let dir = tempfile::TempDir::new().unwrap();
        let marker = dir.path().join("wrote-partial-bytes");
        let child = ShellSpec::for_tests()
            .spawn(&partial_multibyte_then_signal(&marker))
            .expect("测试用 shell 必须能起来");
        let sender = tokio::spawn(async move {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
            while !marker.exists() && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            // 标记已出现 ⇒ 两字节已在管道里；再给读循环一拍把它们收进 carry。
            tokio::time::sleep(Duration::from_millis(300)).await;
            let _ = cancel_tx.send(CancelReason::Task);
        });
        let outcome = read_child(
            child,
            Duration::from_secs(120),
            None,
            sink,
            Some(&cancel_rx),
        )
        .await
        .unwrap();
        sender.await.unwrap();
        assert!(matches!(
            outcome,
            ExecOutcome::Cancelled {
                reason: CancelReason::Task
            }
        ));
        // 半截的「你」必须以一个替换字符交付：交付的内容是完整字符（不是半
        // 个字节），且那两字节没有被吞掉。
        let text = received.lock().clone();
        assert_eq!(text, "\u{fffd}", "取消也要 flush 解码器尾巴：{text:?}");
    }

    /// 「尾巴不吞」这条保证的纯函数版本：定时相关的端到端用例偶尔会抢在子进程
    /// 写出之前取消（那时 sink 合法地为空），这条不受调度影响。
    #[test]
    fn a_partial_multibyte_tail_survives_flush() {
        let mut carry = Utf8Carry::default();
        // 「你」的前两字节：合法前缀，暂不交付。
        assert_eq!(carry.decode(&[0xE4, 0xBD]), "");
        // 流结束时必须把残缺尾巴按 lossy 交出来，而不是吞掉。
        assert_eq!(carry.flush(), "\u{fffd}");
        // flush 之后 carry 清空，不会把尾巴带到下一次解码。
        assert_eq!(carry.flush(), "");

        // 完整字符照常直通，不受 carry 影响。
        let mut carry = Utf8Carry::default();
        assert_eq!(carry.decode("你".as_bytes()), "你");
        assert_eq!(carry.flush(), "");
    }

    #[tokio::test]
    async fn observable_delivers_chunks_to_the_sink() {
        let received = Arc::new(parking_lot::Mutex::new(String::new()));
        let buffer = received.clone();
        let sink: ChunkCallback = Arc::new(move |chunk| buffer.lock().push_str(chunk));
        let child = ShellSpec::for_tests()
            .spawn(&echo("stream-me"))
            .expect("测试用 shell 必须能起来");
        let outcome = read_child(child, Duration::from_secs(60), None, sink, None)
            .await
            .unwrap();
        assert!(matches!(
            outcome,
            ExecOutcome::Completed {
                output: (),
                exit: ExecExit { code: Some(0), .. }
            }
        ));
        assert!(received.lock().contains("stream-me"));
    }

    #[test]
    fn decode_carries_multibyte_chars_split_across_reads() {
        let mut carry = Utf8Carry::default();
        let mut decoded = String::new();
        // 逐字节喂：每一块都是「还没读完的合法前缀」，只有补全后才解得出字符。
        for byte in "你好a".as_bytes() {
            decoded.push_str(&carry.decode(&[*byte]));
        }
        decoded.push_str(&carry.flush());
        assert_eq!(decoded, "你好a");
        assert!(!decoded.contains('\u{fffd}'));
    }

    #[test]
    fn decode_treats_invalid_bytes_as_lossy_without_carrying_them() {
        let mut carry = Utf8Carry::default();
        assert_eq!(carry.decode(b"a\xffb"), "a\u{fffd}b");
        // 真·非法字节不跨块保留（否则会把后面的内容一起粘成乱码）。
        assert_eq!(carry.flush(), "");
    }

    #[test]
    fn missing_program_is_reported_explicitly() {
        // 走的就是生产 shell 找不到时那条错误路径，只是换成必然不存在的程序名
        // （不依赖环境里到底装了哪个 shell）。
        let error = spawn_process("marcel-local-exec-no-such-program", &["-c", "noop"])
            .expect_err("不存在的程序必须起不来");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        let message = spawn_error("marcel-local-exec-no-such-program", error).to_string();
        assert!(
            message.contains("marcel-local-exec-no-such-program"),
            "{message}"
        );
        assert!(message.contains("找不到"), "{message}");
        assert_eq!(
            ShellSpec {
                program: "marcel-local-exec-no-such-program",
                args: &["-c"],
                prelude: "",
            }
            .script("echo hi"),
            "echo hi"
        );
    }

    /// Windows 上生产 shell 固定是 Windows PowerShell **5.1**（`powershell`）：
    /// 产品决定只用 5.1、不考虑 pwsh。谁要改这个决定，先改这条断言与它的名字。
    #[test]
    fn production_shell_is_windows_powershell_51_on_windows_and_bash_elsewhere() {
        let spec = ShellSpec::platform();
        #[cfg(windows)]
        {
            assert_eq!(spec.program, "powershell");
            assert!(spec.args.contains(&"-NoProfile"));
            assert!(spec.args.contains(&"-NonInteractive"));
            assert_eq!(spec.args.last(), Some(&"-Command"));
            assert!(spec.prelude.contains("OutputEncoding"));
            // 前置语句必须能直接接用户命令（用 `;` 分隔，语义不变）。
            assert_eq!(spec.script("whoami"), format!("{}whoami", spec.prelude));
            assert!(spec.prelude.ends_with(';'));
        }
        #[cfg(not(windows))]
        {
            assert_eq!(spec.program, "bash");
            assert_eq!(spec.args, POSIX_SHELL_ARGS);
            assert_eq!(spec.script("whoami"), "whoami");
        }
    }
}
