//! 分段并发下载器（HTTP Range）。
//!
//! **为什么需要它**：GitHub release 资产这类线路是「按单条连接限速」的 —— 实测
//! 同一网络下拉同一个包：单连接 0.02 MB/s、8 连接 0.13 MB/s、16 连接 0.25 MB/s，
//! 几乎随连接数线性增长。Motrix / aria2 之所以快，就是把一条流拆成十几条连接。
//! 应用内的安装包下载此前是单连接顺序流，因此只能跑到浏览器单连接的水平。
//!
//! **设计约束**（宁可慢也不能下坏）：
//! - 先探测**每个**候选源是否支持 Range（`Range: bytes=0-0` → 期望 `206`）与
//!   真实大小；不支持或大小未知 → 退回单连接顺序流，与旧实现同语义。Range
//!   能力是「按源」的属性：只支持整包 GET 的镜像不会被塞 Range 请求（那必然
//!   失败），而是留给分段全挂之后的整包兜底轮 —— 镜像兜底恰好在主源出问题时
//!   才用得上，不能在那时候失效；
//! - 分段写入同一个 `.part` 文件：**每段各自 open 一个独立句柄**再 seek 写，
//!   不用 `File::try_clone` —— Windows 上克隆出的句柄共享文件指针，并发写会互踩；
//! - 任一段失败先重试该段（最多 [`SEGMENT_ATTEMPTS`] 次，仍不行再换候选源），
//!   全都不行才整次失败（不留半成品，与旧实现一致）；
//! - 每个响应都要能对上「请求的区间」与「应有的长度」：206 必须带与请求逐字节
//!   一致的 `Content-Range`，整包响应必须写满已知总大小。少了这两道校验，中间层
//!   按自己的块边界返回、或 close-delimited 响应被悄悄截断，都会干净地写进文件，
//!   最后只以一句「更新包校验失败（内容不完整或被篡改）」收场 —— 重试无效，还
//!   把中间层的问题栽给用户；
//! - 取消由调用方以闭包传入，每个 chunk 检查一次；取消后删 `.part` 并返回
//!   [`DownloadOutcome::Cancelled`]（不是失败，调用方不该弹红色错误）；
//! - **不做校验**：分段下载无法边下边算整体 sha256，由调用方在下载完成后对文件
//!   做 sha256 / minisign 校验（多一次读盘换并发加速是划算的）。

use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use futures::future::join_all;
use reqwest::header::{CONTENT_RANGE, RANGE};

/// 并发连接上限。实测到 GitHub 资产的线路基本「每连接各自限速」，所以连接数就是
/// 倍率（8 → 7.1x，16 → 12.3x）；16 与 Motrix 默认值一致，再往上收益递减且更容易
/// 被线路打回。
pub const MAX_SEGMENTS: usize = 16;

/// 每个分段的最少字节数：太小不值得分段（调度开销与失败面都变大）。
const MIN_SEGMENT_BYTES: u64 = 256 * 1024;

/// 单个 chunk 的读超时（秒）——防止某条连接卡死拖住整次下载。
const CHUNK_READ_TIMEOUT_SECS: u64 = 60;

/// 进度回调节流间隔。
const PROGRESS_INTERVAL: Duration = Duration::from_millis(500);

/// 单段最多尝试次数（网络抖动重试，不因此放弃整次下载）。
const SEGMENT_ATTEMPTS: usize = 3;

/// 建立连接的超时（秒）。
const CONNECT_TIMEOUT_SECS: u64 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadOutcome {
    /// 文件已完整写入 `part_path`（校验与改名由调用方负责）。
    Done,
    /// 被调用方要求停止：`.part` 已删除。
    Cancelled,
}

/// 源探测结果。
struct Probe {
    url: String,
    size: u64,
    accepts_ranges: bool,
}

enum FetchError {
    Cancelled,
    Failed { message: String, written: u64 },
}

impl FetchError {
    fn failed(message: impl Into<String>) -> Self {
        FetchError::Failed {
            message: message.into(),
            written: 0,
        }
    }
}

/// 把总长度切成至多 `max_segments` 段（闭区间，覆盖完整且不重叠）。
///
/// 文件很小（不足一段的最小字节数）时只返回一段 —— 调用方据此走单连接路径。
/// 拆成独立纯函数是为了能直接单测：分段算错等于「下出来的文件对不上 sha256」，
/// 这种错误在真实网络里很难稳定复现。
pub fn plan_segments(total: u64, max_segments: usize) -> Vec<(u64, u64)> {
    if total == 0 {
        return vec![];
    }
    let wanted = (total / MIN_SEGMENT_BYTES).max(1) as usize;
    let count = wanted.min(max_segments.max(1)).min(total as usize);
    if count <= 1 {
        return vec![(0, total - 1)];
    }
    let base = total / count as u64;
    let remainder = total % count as u64;
    let mut ranges = Vec::with_capacity(count);
    let mut start = 0u64;
    for i in 0..count {
        // 前 remainder 段各多分 1 字节，保证恰好铺满且无空洞
        let len = base + if (i as u64) < remainder { 1 } else { 0 };
        ranges.push((start, start + len - 1));
        start += len;
    }
    ranges
}

/// 从 `Content-Range: bytes 0-0/9748248` 里取出总大小。
fn parse_content_range_total(value: &str) -> Option<u64> {
    value.rsplit('/').next()?.trim().parse::<u64>().ok()
}

/// 从 `Content-Range: bytes 123-456/9748248` 里取出区间 `(123, 456)`。
///
/// 只认 `bytes` 单位与「起-止/…」这一种形态；解析不出来一律返回 `None`，调用方
/// 据此拒绝这次响应 —— 「大概是这一个区间吧」那种猜测会让别人区间的字节写进文件。
fn parse_content_range_span(value: &str) -> Option<(u64, u64)> {
    let rest = value.trim().strip_prefix("bytes")?.trim_start();
    let span = rest.split('/').next()?.trim();
    let (start, end) = span.split_once('-')?;
    Some((start.trim().parse().ok()?, end.trim().parse().ok()?))
}

pub struct SegmentedDownload<'a> {
    /// 候选下载源，按优先级排列：支持 Range 的源先做分段并发（主源优先），全挂
    /// 或都不支持分段时退到整包单连接，逐个用它们兜底。
    pub urls: Vec<String>,
    /// 半成品落盘位置。
    pub part_path: PathBuf,
    /// `latest.json` 声明的大小（探测拿不到大小时用它）。
    pub expected_size: u64,
    /// 调用方的取消查询（每个 chunk 一次）。
    pub cancel: &'a (dyn Fn() -> bool + Sync),
    /// 进度回调 `(已下载, 总大小)`。
    pub progress: &'a (dyn Fn(u64, u64) + Sync),
}

impl SegmentedDownload<'_> {
    pub async fn run(&self) -> Result<DownloadOutcome, String> {
        if (self.cancel)() {
            return Ok(DownloadOutcome::Cancelled);
        }
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
            .user_agent(concat!("Marcel-SSH/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| format!("无法创建下载客户端: {}", e))?;

        // 探测**全部**候选源，而不只是第一个可达的：Range 能力是「按源」的属性，
        // 只按第一个可达源定策略的话，主源支持分段、镜像只支持整包时，镜像会一直
        // 收到满足不了的 Range 请求 —— 「镜像兜底」恰好在最需要它的时候失效。
        let probes = self.probe_all(&client).await?;
        let total = probes
            .iter()
            .map(|p| p.size)
            .find(|size| *size > 0)
            .unwrap_or(self.expected_size);
        if total == 0 {
            return Err("更新包大小未知".into());
        }
        let ranged_sources: Vec<String> = probes
            .iter()
            .filter(|p| p.accepts_ranges)
            .map(|p| p.url.clone())
            .collect();

        let downloaded = AtomicU64::new(0);
        let cancelled = AtomicBool::new(false);
        let last_emit = Mutex::new(Instant::now() - PROGRESS_INTERVAL);
        // 预分配：分段并发写要求文件先有最终长度（各段 seek 到自己的偏移写）
        self.reset_part_file(total, &downloaded, &cancelled, &last_emit)?;

        let mut segmented_error: Option<String> = None;
        if ranged_sources.is_empty() {
            log::info!("候选源均不支持分段（HTTP Range），改用单连接顺序下载");
        } else {
            // 第一轮：支持 Range 的源上分段并发（只切出一段时即普通单连接 GET）。
            let ranges = plan_segments(total, MAX_SEGMENTS);
            let segmented = ranges.len() > 1;
            log::info!(
                "开始下载：{} 字节，{} 段{}",
                total,
                ranges.len(),
                if segmented {
                    "（并发）"
                } else {
                    "（单连接）"
                }
            );
            match self
                .fetch_all(
                    &client,
                    &ranged_sources,
                    &ranges,
                    segmented,
                    total,
                    &downloaded,
                    &cancelled,
                    &last_emit,
                )
                .await
            {
                Ok(()) => {
                    (self.progress)(downloaded.load(Ordering::Relaxed), total);
                    return Ok(DownloadOutcome::Done);
                }
                Err(FetchError::Cancelled) => {
                    let _ = std::fs::remove_file(&self.part_path);
                    return Ok(DownloadOutcome::Cancelled);
                }
                Err(FetchError::Failed { message, .. }) => {
                    log::warn!("分段下载失败（{}），改用整包单连接重试", message);
                    segmented_error = Some(message);
                }
            }
        }

        // 第二轮：整包单连接（不送 Range 头）。走到这里有两种原因：候选源都不支持
        // 分段；或分段全部失败 —— 后一种情形下，只支持整包 GET 的镜像正是唯一还能
        // 把包拿回来的路。候选按配置优先级排列，探测阶段就被判不可达的源也再给一次
        // 机会（探测用的 Range 请求本身可能被拒）。
        log::info!("整包单连接下载（候选源 {} 个）", self.urls.len());
        self.reset_part_file(total, &downloaded, &cancelled, &last_emit)?;
        let ranges = [(0u64, total - 1)];
        match self
            .fetch_all(
                &client,
                &self.urls,
                &ranges,
                false,
                total,
                &downloaded,
                &cancelled,
                &last_emit,
            )
            .await
        {
            Ok(()) => {
                (self.progress)(downloaded.load(Ordering::Relaxed), total);
                Ok(DownloadOutcome::Done)
            }
            Err(FetchError::Cancelled) => {
                let _ = std::fs::remove_file(&self.part_path);
                Ok(DownloadOutcome::Cancelled)
            }
            Err(FetchError::Failed { message, .. }) => {
                let _ = std::fs::remove_file(&self.part_path);
                Err(match segmented_error {
                    Some(seg) => format!("{}；改用整包下载后仍失败: {}", seg, message),
                    None => message,
                })
            }
        }
    }

    /// 把 `.part` 复位成「长度 = total 的空文件」并清空进度/取消标志：第一轮开始
    /// 前做一次（分段并发写要求文件先有最终长度），整包兜底轮开始前再做一次（把上
    /// 一轮写进去的数据整体丢弃，避免新旧字节混在一起）。
    fn reset_part_file(
        &self,
        total: u64,
        downloaded: &AtomicU64,
        cancelled: &AtomicBool,
        last_emit: &Mutex<Instant>,
    ) -> Result<(), String> {
        let file = std::fs::File::create(&self.part_path)
            .map_err(|e| format!("无法写入临时文件: {}", e))?;
        file.set_len(total)
            .map_err(|e| format!("无法预分配空间: {}", e))?;
        downloaded.store(0, Ordering::Relaxed);
        cancelled.store(false, Ordering::Relaxed);
        if let Ok(mut last) = last_emit.lock() {
            *last = Instant::now() - PROGRESS_INTERVAL;
        }
        Ok(())
    }

    /// 探测所有候选源：返回**按给定优先级排列的可达源**（各自带 Range 能力与大
    /// 小），探测失败的源不进这个列表（它们仍会出现在整包兜底轮的候选里）。
    ///
    /// 并发探测：串行的话，一个黑洞镜像会把「开始下载」拖到几十秒之后（每个不可达
    /// 源各付一次连接超时），而它本来就只在主源失败时才用得上。
    async fn probe_all(&self, client: &reqwest::Client) -> Result<Vec<Probe>, String> {
        let results = join_all(self.urls.iter().map(|url| self.probe_one(client, url))).await;
        let mut last_err = "所有下载源均不可达".to_string();
        let mut probes = Vec::new();
        for (url, result) in self.urls.iter().zip(results) {
            match result {
                Ok(probe) => probes.push(probe),
                Err(e) => {
                    log::warn!("下载源不可用 {}：{}，尝试下一个", url, e);
                    last_err = e;
                }
            }
        }
        if probes.is_empty() {
            Err(last_err)
        } else {
            Ok(probes)
        }
    }

    /// 探测单个源：`Range: bytes=0-0` 得到 206 说明支持分段；200（忽略了 Range，
    /// 返回整包）说明只能单连接；其他状态码 / 网络错误视为该源当前不可用。
    async fn probe_one(&self, client: &reqwest::Client, url: &str) -> Result<Probe, String> {
        let resp = client
            .get(url)
            .header(RANGE, "bytes=0-0")
            .send()
            .await
            .map_err(|e| format!("下载请求失败: {}", e))?;
        let status = resp.status();
        if status == reqwest::StatusCode::PARTIAL_CONTENT {
            let size = resp
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(parse_content_range_total)
                .unwrap_or(self.expected_size);
            return Ok(Probe {
                url: url.to_string(),
                size,
                accepts_ranges: true,
            });
        }
        if status.is_success() {
            // 服务器忽略了 Range（返回整包）：能用，但只能单连接
            return Ok(Probe {
                url: url.to_string(),
                size: resp.content_length().unwrap_or(self.expected_size),
                accepts_ranges: false,
            });
        }
        Err(format!("下载服务器返回 {}", status))
    }

    /// 跑完计划里的所有分段（`join_all` 在同一任务上并发 poll，网络等待天然重叠）。
    /// `sources` 就是这一轮允许使用的候选源，每段按顺序重试与换源。
    #[allow(clippy::too_many_arguments)]
    async fn fetch_all(
        &self,
        client: &reqwest::Client,
        sources: &[String],
        ranges: &[(u64, u64)],
        segmented: bool,
        total: u64,
        downloaded: &AtomicU64,
        cancelled: &AtomicBool,
        last_emit: &Mutex<Instant>,
    ) -> Result<(), FetchError> {
        let futures = ranges.iter().enumerate().map(|(index, (start, end))| {
            let range = if segmented {
                Some((*start, *end))
            } else {
                None
            };
            self.fetch_segment(
                client, sources, index, range, *start, total, downloaded, cancelled, last_emit,
            )
        });

        let results = join_all(futures).await;
        // 取消优先：只要有一段是因取消结束，整次就是「取消」而不是「失败」
        if cancelled.load(Ordering::Relaxed) {
            return Err(FetchError::Cancelled);
        }
        for r in results {
            r?;
        }
        Ok(())
    }

    /// 单段（带重试与换源）。
    #[allow(clippy::too_many_arguments)]
    async fn fetch_segment(
        &self,
        client: &reqwest::Client,
        sources: &[String],
        index: usize,
        range: Option<(u64, u64)>,
        offset: u64,
        total: u64,
        downloaded: &AtomicU64,
        cancelled: &AtomicBool,
        last_emit: &Mutex<Instant>,
    ) -> Result<(), FetchError> {
        let mut last_err = "未知错误".to_string();
        for source in sources {
            for attempt in 0..SEGMENT_ATTEMPTS {
                if (self.cancel)() {
                    cancelled.store(true, Ordering::Relaxed);
                    return Err(FetchError::Cancelled);
                }
                match self
                    .fetch_segment_once(
                        client, source, range, offset, total, downloaded, cancelled, last_emit,
                    )
                    .await
                {
                    Ok(()) => return Ok(()),
                    Err(FetchError::Cancelled) => {
                        cancelled.store(true, Ordering::Relaxed);
                        return Err(FetchError::Cancelled);
                    }
                    Err(FetchError::Failed { message, written }) => {
                        // 重试前把这段已计入的字节退回去，否则进度会虚高
                        downloaded.fetch_sub(written, Ordering::Relaxed);
                        last_err = format!("第 {} 段: {}", index + 1, message);
                        log::warn!(
                            "{}（第 {}/{} 次尝试，源 {}）",
                            last_err,
                            attempt + 1,
                            SEGMENT_ATTEMPTS,
                            source
                        );
                    }
                }
                tokio::time::sleep(Duration::from_millis(300 * (attempt as u64 + 1))).await;
            }
        }
        Err(FetchError::Failed {
            message: last_err,
            written: 0,
        })
    }

    /// 真正的一次请求 + 写盘。
    #[allow(clippy::too_many_arguments)]
    async fn fetch_segment_once(
        &self,
        client: &reqwest::Client,
        url: &str,
        range: Option<(u64, u64)>,
        offset: u64,
        total: u64,
        downloaded: &AtomicU64,
        cancelled: &AtomicBool,
        last_emit: &Mutex<Instant>,
    ) -> Result<(), FetchError> {
        let mut request = client.get(url);
        if let Some((start, end)) = range {
            request = request.header(RANGE, format!("bytes={}-{}", start, end));
        }
        let resp = match request.send().await {
            Ok(r) => r,
            Err(e) => return Err(FetchError::failed(format!("下载请求失败: {}", e))),
        };
        if let Some((start, end)) = range {
            if resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
                // 探测时支持、真正取分段时又不支持（中间层改写）：明确报出来，
                // 不静默写坏文件
                return Err(FetchError::failed(format!(
                    "下载源未按分段返回（HTTP {}）",
                    resp.status()
                )));
            }
            // 206 必须带 Content-Range，且区间要与请求的**逐字节一致**：只看
            // 「206 + 收到的字节数」的话，中间层/镜像按自己的块边界返回（或整体
            // 偏移一段）时，长度校验照样通过，写进去的却是别人区间的字节 —— 一路
            // 到最后才以「更新包校验失败（内容不完整或被篡改）」暴露，重试无效且
            // 把中间层的问题栽给用户。这里在写盘之前就拒掉：可重试、可换源，报错
            // 也直指区间不一致。
            let got = resp
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(parse_content_range_span);
            if got != Some((start, end)) {
                return Err(FetchError::failed(format!(
                    "下载源返回的分段区间与请求不一致（请求 {}-{}，实际 {}）",
                    start,
                    end,
                    got.map(|(s, e)| format!("{}-{}", s, e))
                        .unwrap_or_else(|| "缺失或无法解析".to_string())
                )));
            }
        } else if !resp.status().is_success() {
            return Err(FetchError::failed(format!(
                "下载服务器返回 {}",
                resp.status()
            )));
        }

        // 每段独立开句柄：Windows 上克隆句柄共享文件指针，并发写会互相踩
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .open(&self.part_path)
        {
            Ok(f) => f,
            Err(e) => return Err(FetchError::failed(format!("无法打开临时文件: {}", e))),
        };
        if let Err(e) = file.seek(SeekFrom::Start(offset)) {
            return Err(FetchError::failed(format!("定位写入位置失败: {}", e)));
        }

        let mut written = 0u64;
        let mut stream = resp;
        loop {
            if (self.cancel)() {
                cancelled.store(true, Ordering::Relaxed);
                return Err(FetchError::Cancelled);
            }
            let chunk = match tokio::time::timeout(
                Duration::from_secs(CHUNK_READ_TIMEOUT_SECS),
                stream.chunk(),
            )
            .await
            {
                Err(_) => {
                    return Err(FetchError::Failed {
                        message: "下载超时（连接停滞）".into(),
                        written,
                    })
                }
                Ok(Err(e)) => {
                    return Err(FetchError::Failed {
                        message: format!("下载中断: {}", e),
                        written,
                    })
                }
                Ok(Ok(chunk)) => chunk,
            };
            let Some(bytes) = chunk else { break };
            if let Err(e) = file.write_all(&bytes) {
                return Err(FetchError::Failed {
                    message: format!("写入失败（磁盘空间不足？）: {}", e),
                    written,
                });
            }
            written += bytes.len() as u64;
            downloaded.fetch_add(bytes.len() as u64, Ordering::Relaxed);
            emit_progress_if_due(downloaded, total, last_emit, self.progress);
        }
        file.flush().ok();

        // 写够了才算这一段成功，**两种请求都要校验**：只校验分段区间的话，整包
        // 响应被提前掐断时（close-delimited、无 Content-Length、无 chunked）会干净
        // 地 EOF 并被当成「下载完成」—— 重试没了、流量白费，残留的预分配零字节与
        // 真数据混在一起，最后只留下一句含糊的「校验失败（内容不完整或被篡改）」。
        // 有 Range：期望区间长度；无 Range（整包）：期望已知总大小。
        let want = match range {
            Some((start, end)) => end - start + 1,
            None => total,
        };
        if written != want {
            return Err(FetchError::Failed {
                message: if range.is_some() {
                    format!("分段长度不足（期望 {} 字节，实收 {}）", want, written)
                } else {
                    format!("下载被截断（期望 {} 字节，实收 {}）", want, written)
                },
                written,
            });
        }
        Ok(())
    }
}

/// 进度节流：多个分段共享同一个「上次回调时间」。
fn emit_progress_if_due(
    downloaded: &AtomicU64,
    total: u64,
    last_emit: &Mutex<Instant>,
    progress: &(dyn Fn(u64, u64) + Sync),
) {
    let Ok(mut last) = last_emit.lock() else {
        return;
    };
    if last.elapsed() < PROGRESS_INTERVAL {
        return;
    }
    *last = Instant::now();
    let done = downloaded.load(Ordering::Relaxed);
    drop(last);
    progress(done, total);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn assert_covers_exactly(ranges: &[(u64, u64)], total: u64) {
        assert!(!ranges.is_empty(), "分段不能为空");
        assert_eq!(ranges[0].0, 0, "第一段必须从 0 开始");
        assert_eq!(
            ranges.last().unwrap().1,
            total - 1,
            "最后一段必须落在最后一个字节"
        );
        for (i, (start, end)) in ranges.iter().enumerate() {
            assert!(start <= end, "第 {} 段起点大于终点", i + 1);
            if i > 0 {
                assert_eq!(
                    ranges[i - 1].1 + 1,
                    *start,
                    "第 {} 段与上一段之间有空洞或重叠",
                    i + 1
                );
            }
        }
    }

    #[test]
    fn plan_segments_covers_every_byte_without_overlap() {
        // 真实安装包（9.7MB 桌面 / 26MB 安卓）该切满 16 段；其余尺寸只要求
        // 「铺满且不重叠」
        let ranges = plan_segments(9_748_248, MAX_SEGMENTS);
        assert_eq!(ranges.len(), MAX_SEGMENTS);
        assert_covers_exactly(&ranges, 9_748_248);
        let ranges = plan_segments(26_337_407, MAX_SEGMENTS);
        assert_eq!(ranges.len(), MAX_SEGMENTS);
        assert_covers_exactly(&ranges, 26_337_407);

        for total in [1_048_577u64, 8_000_000, 262_144, 300_000] {
            let ranges = plan_segments(total, MAX_SEGMENTS);
            assert_covers_exactly(&ranges, total);
        }
    }

    #[test]
    fn plan_segments_handles_uneven_split() {
        // 不能被段数整除时前几段各多 1 字节，不能少下也不能重复下
        let ranges = plan_segments(1000 * 1024, 3);
        assert_eq!(ranges.len(), 3);
        assert_covers_exactly(&ranges, 1000 * 1024);
    }

    #[test]
    fn plan_segments_keeps_small_files_single() {
        // 小于一段最小字节数的文件不分段
        assert_eq!(
            plan_segments(64 * 1024, MAX_SEGMENTS),
            vec![(0, 64 * 1024 - 1)]
        );
        assert_eq!(plan_segments(1, MAX_SEGMENTS), vec![(0, 0)]);
        assert!(plan_segments(0, MAX_SEGMENTS).is_empty());
    }

    #[test]
    fn plan_segments_respects_segment_floor() {
        // 1MB 只能切 4 段（每段 ≥256KB），不能硬凑 16 段
        assert_eq!(plan_segments(1024 * 1024, MAX_SEGMENTS).len(), 4);
    }

    #[test]
    fn parse_content_range_total_reads_size() {
        assert_eq!(
            parse_content_range_total("bytes 0-0/9748248"),
            Some(9_748_248)
        );
        assert_eq!(parse_content_range_total("bytes 0-0/*"), None);
        assert_eq!(parse_content_range_total("garbage"), None);
    }

    #[test]
    fn parse_content_range_span_reads_interval() {
        assert_eq!(
            parse_content_range_span("bytes 123-456/9748248"),
            Some((123, 456))
        );
        assert_eq!(parse_content_range_span("bytes 0-0/*"), Some((0, 0)));
        // 解析不出来（缺失 / 单位不对 / 半个区间）一律 None：调用方据此拒绝响应
        assert_eq!(parse_content_range_span(""), None);
        assert_eq!(parse_content_range_span("bytes */9748248"), None);
        assert_eq!(parse_content_range_span("items 1-2/3"), None);
        assert_eq!(parse_content_range_span("bytes 1-/3"), None);
    }

    // ── 端到端：本地起一个最小 HTTP 服务，验证真的下出来是对的 ──────────
    //
    // 分段写盘最容易出的错是「偏移算错 / 并发写互相踩」，而这类错误不会报错，
    // 只会让 sha256 对不上（在真实网络里很难稳定复现）。所以这里用本地服务把
    // 整条路径跑通并逐字节比对。

    /// 起一个最小 HTTP/1.1 服务：支持 `Range` 时返回 206 分片，不支持时返回整包。
    /// 返回 (base_url, 已服务请求数)。`chunk_delay_ms` 用来模拟慢速连接。
    async fn spawn_server(
        body: Vec<u8>,
        honor_range: bool,
        chunk_delay_ms: u64,
    ) -> (String, std::sync::Arc<AtomicU64>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = std::sync::Arc::new(AtomicU64::new(0));
        let hits_for_task = hits.clone();
        let body = std::sync::Arc::new(body);

        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let body = body.clone();
                let hits = hits_for_task.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = match socket.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    hits.fetch_add(1, Ordering::Relaxed);

                    let range = req
                        .lines()
                        .find(|l| l.to_ascii_lowercase().starts_with("range:"))
                        .and_then(|l| l.split('=').nth(1))
                        .map(|v| v.trim().to_string());

                    if honor_range {
                        // 严格按请求的区间返回（只回 start..end，不是 start..文件尾）：
                        // 否则「分段写错位置」这类 bug 会被重复写入的相同字节掩盖
                        let (start, end) = match range.as_deref().and_then(|v| {
                            let mut it = v.split('-');
                            let s = it.next()?.trim().parse::<usize>().ok()?;
                            let e = it
                                .next()
                                .and_then(|x| x.trim().parse::<usize>().ok())
                                .unwrap_or(body.len() - 1);
                            Some((s, e))
                        }) {
                            Some((s, e)) => (s, e.min(body.len() - 1)),
                            None => (0, body.len() - 1),
                        };
                        let head = format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                            end - start + 1,
                            start,
                            end,
                            body.len()
                        );
                        if socket.write_all(head.as_bytes()).await.is_err() {
                            return;
                        }
                        // 小片慢发，让取消/并发能真的交错发生
                        for piece in body[start..=end].chunks(16 * 1024) {
                            if socket.write_all(piece).await.is_err() {
                                return;
                            }
                            let _ = socket.flush().await;
                            if chunk_delay_ms > 0 {
                                tokio::time::sleep(Duration::from_millis(chunk_delay_ms)).await;
                            }
                        }
                    } else {
                        let head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        if socket.write_all(head.as_bytes()).await.is_err() {
                            return;
                        }
                        let _ = socket.write_all(&body).await;
                    }
                    let _ = socket.shutdown().await;
                });
            }
        });

        (format!("http://{}", addr), hits)
    }

    /// 一个「只认 Range 语法、按自己的块边界返回」的服务：请求多少字节就回多少
    /// 字节，但 Content-Range 与内容**永远从 0 起**（模拟中间层/镜像改写区间）。
    /// 不带 Range 的请求一律 500 —— 把整包兜底轮也堵死，好让「区间不一致」这个
    /// 失败原因浮到最外层（否则整包轮成功，就看不出分段校验起没起作用）。
    async fn spawn_shifted_range_server(body: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body = std::sync::Arc::new(body);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let body = body.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let Ok(n) = socket.read(&mut buf).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let span = req
                        .lines()
                        .find(|l| l.to_ascii_lowercase().starts_with("range:"))
                        .and_then(|l| l.split('=').nth(1))
                        .and_then(|v| {
                            let mut it = v.trim().split('-');
                            let s = it.next()?.parse::<usize>().ok()?;
                            let e = it.next()?.parse::<usize>().ok()?;
                            Some(e - s + 1)
                        });
                    let Some(want) = span else {
                        let _ = socket
                            .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                            .await;
                        let _ = socket.shutdown().await;
                        return;
                    };
                    let want = want.min(body.len());
                    let head = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes 0-{}/{}\r\nConnection: close\r\n\r\n",
                        want,
                        want - 1,
                        body.len()
                    );
                    if socket.write_all(head.as_bytes()).await.is_err() {
                        return;
                    }
                    let _ = socket.write_all(&body[..want]).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        format!("http://{}", addr)
    }

    /// 一个「close-delimited 且提前掐断」的服务：响应不带 Content-Length，写完
    /// `cut_at` 字节就直接关连接。客户端读到的是干净的 EOF —— 只有拿实际写入量
    /// 与已知总大小对照才能发现被截断。
    async fn spawn_truncating_server(body: Vec<u8>, cut_at: usize) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body = std::sync::Arc::new(body);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let body = body.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let Ok(n) = socket.read(&mut buf).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    let head = "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n";
                    if socket.write_all(head.as_bytes()).await.is_err() {
                        return;
                    }
                    let _ = socket.write_all(&body[..cut_at.min(body.len())]).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        format!("http://{}", addr)
    }

    /// 一个「探测能过、真取分段就挂」的源：只有 `Range: bytes=0-0` 回正确 206，
    /// 其余一律 500。用于验证「分段全挂后，整包兜底轮把只支持整包 GET 的镜像
    /// 当救兵」这条路径。
    async fn spawn_probe_only_server(body: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body = std::sync::Arc::new(body);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let body = body.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let Ok(n) = socket.read(&mut buf).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let probe = req
                        .lines()
                        .find(|l| l.to_ascii_lowercase().starts_with("range:"))
                        .map(|l| l.split('=').nth(1).unwrap_or("").trim() == "0-0")
                        .unwrap_or(false);
                    let head = if probe {
                        format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Length: 1\r\nContent-Range: bytes 0-0/{}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                    } else {
                        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                    };
                    if socket.write_all(head.as_bytes()).await.is_err() {
                        return;
                    }
                    if probe {
                        let _ = socket.write_all(&body[..1]).await;
                    }
                    let _ = socket.shutdown().await;
                });
            }
        });
        format!("http://{}", addr)
    }

    fn tmp_part(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("marcel-dl-test");
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    /// 造一个受控内容（不是全零，避免「写错位置但恰好还是零」这种假通过）。
    fn payload(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    #[tokio::test]
    async fn segmented_download_reproduces_source_bytes() {
        let source = payload(3 * 1024 * 1024);
        let (url, hits) = spawn_server(source.clone(), true, 0).await;
        let part = tmp_part("segmented.part");
        let _ = std::fs::remove_file(&part);

        let no_cancel = || false;
        let no_progress = |_: u64, _: u64| {};
        let dl = SegmentedDownload {
            urls: vec![url],
            part_path: part.clone(),
            expected_size: source.len() as u64,
            cancel: &no_cancel,
            progress: &no_progress,
        };
        assert_eq!(dl.run().await.unwrap(), DownloadOutcome::Done);

        let written = std::fs::read(&part).unwrap();
        assert_eq!(written.len(), source.len(), "落盘长度必须等于源长度");
        assert!(written == source, "分段下载的内容必须与源逐字节一致");
        assert!(
            hits.load(Ordering::Relaxed) > MAX_SEGMENTS as u64 / 2,
            "应当真的走了多段并发（命中 {} 次）",
            hits.load(Ordering::Relaxed)
        );
        let _ = std::fs::remove_file(&part);
    }

    #[tokio::test]
    async fn download_falls_back_to_single_stream_without_range_support() {
        let source = payload(1024 * 1024);
        let (url, hits) = spawn_server(source.clone(), false, 0).await;
        let part = tmp_part("single.part");
        let _ = std::fs::remove_file(&part);

        let no_cancel = || false;
        let no_progress = |_: u64, _: u64| {};
        let dl = SegmentedDownload {
            urls: vec![url],
            part_path: part.clone(),
            expected_size: source.len() as u64,
            cancel: &no_cancel,
            progress: &no_progress,
        };
        assert_eq!(dl.run().await.unwrap(), DownloadOutcome::Done);
        assert!(
            std::fs::read(&part).unwrap() == source,
            "单连接回落也要下对"
        );
        // 探测 1 次 + 单连接整包 1 次
        assert_eq!(
            hits.load(Ordering::Relaxed),
            2,
            "不支持 Range 时只该有两次请求"
        );
        let _ = std::fs::remove_file(&part);
    }

    /// 回归：206 的 Content-Range 与请求不一致（中间层按自己的块边界返回）必须
    /// 当场拒绝、可按段重试与换源，而不是把别人的字节写进文件、最后报成「包被
    /// 篡改」——那条路重试无效，还会把中间层的问题栽给用户。
    #[tokio::test]
    async fn rejects_206_with_mismatched_content_range() {
        let source = payload(2 * 1024 * 1024);
        let url = spawn_shifted_range_server(source.clone()).await;
        // 独享临时目录：下面的用例都在并发跑，落盘位置不与其他用例共享
        let tmp = tempfile::tempdir().unwrap();
        let part = tmp.path().join("shifted-range.part");

        let no_cancel = || false;
        let no_progress = |_: u64, _: u64| {};
        let dl = SegmentedDownload {
            urls: vec![url],
            part_path: part.clone(),
            expected_size: source.len() as u64,
            cancel: &no_cancel,
            progress: &no_progress,
        };

        let err = dl.run().await.expect_err("区间不一致必须失败");
        assert!(
            err.contains("区间与请求不一致"),
            "错误文案要直指区间问题: {}",
            err
        );
        assert!(
            !err.contains("篡改"),
            "不得把区间问题归因成内容被篡改: {}",
            err
        );
        assert!(!part.exists(), "失败后不得留下 .part");
    }

    /// 回归：close-delimited（无 Content-Length、无 chunked）响应被提前掐断时
    /// 会干净地 EOF，必须靠「实际写入量 vs 已知总大小」发现，不能当下载完成。
    #[tokio::test]
    async fn rejects_truncated_close_delimited_response() {
        let source = payload(1024 * 1024);
        let url = spawn_truncating_server(source.clone(), source.len() / 2).await;
        let tmp = tempfile::tempdir().unwrap();
        let part = tmp.path().join("truncated.part");

        let no_cancel = || false;
        let no_progress = |_: u64, _: u64| {};
        let dl = SegmentedDownload {
            urls: vec![url],
            part_path: part.clone(),
            expected_size: source.len() as u64,
            cancel: &no_cancel,
            progress: &no_progress,
        };

        let err = dl.run().await.expect_err("被截断的响应必须失败");
        assert!(err.contains("截断"), "错误文案要指出截断: {}", err);
        assert!(!part.exists(), "失败后不得留下 .part");
    }

    /// 回归：主源支持分段但分段请求全挂，镜像只支持整包 GET —— 修复前每段都带着
    /// Range 头打到镜像上必然失败（「未按分段返回」），镜像兜底恰好在最需要它的
    /// 时候失效；修复后分段全挂会退回整包单连接，由镜像把包拿回来。
    #[tokio::test]
    async fn range_incapable_mirror_recovers_whole_file() {
        let source = payload(3 * 1024 * 1024);
        let primary = spawn_probe_only_server(source.clone()).await;
        let mirror = spawn_server(source.clone(), false, 0).await.0;
        let tmp = tempfile::tempdir().unwrap();
        let part = tmp.path().join("mirror-fallback.part");

        let no_cancel = || false;
        let no_progress = |_: u64, _: u64| {};
        let dl = SegmentedDownload {
            urls: vec![primary, mirror],
            part_path: part.clone(),
            expected_size: source.len() as u64,
            cancel: &no_cancel,
            progress: &no_progress,
        };

        assert_eq!(dl.run().await.unwrap(), DownloadOutcome::Done);
        assert!(
            std::fs::read(&part).unwrap() == source,
            "整包兜底也要与原文件逐字节一致"
        );
    }

    #[tokio::test]
    async fn cancel_removes_partial_file() {
        let source = payload(2 * 1024 * 1024);
        // 每片慢发 30ms，保证取消发生在下载中途而不是还没开始
        let (url, _) = spawn_server(source.clone(), true, 30).await;
        let part = tmp_part("cancelled.part");
        let _ = std::fs::remove_file(&part);

        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let stop_for_cancel = stop.clone();
        let cancel = move || stop_for_cancel.load(Ordering::Relaxed);
        let no_progress = |_: u64, _: u64| {};
        let dl = SegmentedDownload {
            urls: vec![url],
            part_path: part.clone(),
            expected_size: source.len() as u64,
            cancel: &cancel,
            progress: &no_progress,
        };

        let stop_for_timer = stop.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            stop_for_timer.store(true, Ordering::Relaxed);
        });

        assert_eq!(dl.run().await.unwrap(), DownloadOutcome::Cancelled);
        assert!(!part.exists(), "取消后必须删掉半成品 .part");
    }

    /// 真实网络冒烟（默认忽略；手动跑：`cargo test --lib download:: -- --ignored --nocapture`）。
    ///
    /// 它验证的是本地服务测不到的那一段：GitHub release 直链会 302 跳到
    /// objects.githubusercontent.com，**跳转后 Range 必须仍然生效** —— 否则探测
    /// 会判定「不支持分段」而静默退回单连接，速度白丢。用 `MARCEL_DL_SMOKE_URL`
    /// 指定要下的资产；`MARCEL_DL_SMOKE_BYTES` 可只下前 N 字节（默认整包）。
    #[tokio::test]
    #[ignore]
    async fn real_url_smoke_uses_segments() {
        let Ok(url) = std::env::var("MARCEL_DL_SMOKE_URL") else {
            eprintln!("跳过：未设置 MARCEL_DL_SMOKE_URL");
            return;
        };
        let part = tmp_part("smoke.part");
        let _ = std::fs::remove_file(&part);
        let no_cancel = || false;
        let progress = |done: u64, total: u64| {
            eprintln!(
                "进度 {}/{} ({:.1}%)",
                done,
                total,
                done as f64 / total as f64 * 100.0
            );
        };
        let dl = SegmentedDownload {
            urls: vec![url],
            part_path: part.clone(),
            expected_size: 0,
            cancel: &no_cancel,
            progress: &progress,
        };
        let started = Instant::now();
        let outcome = dl.run().await.expect("真实下载应成功");
        let size = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        let secs = started.elapsed().as_secs_f64();
        eprintln!(
            "结果={:?} 大小={} 用时={:.1}s 平均={:.2} MB/s",
            outcome,
            size,
            secs,
            size as f64 / 1048576.0 / secs.max(0.001)
        );
        assert_eq!(outcome, DownloadOutcome::Done);
        assert!(size > 0);
        let _ = std::fs::remove_file(&part);
    }
}
