//! TypeSafe「Jev」客户端——System One 决策模型，**不是** chat 模型。
//!
//! 它为什么是独立模块而不是 `openai.rs` 的一个 provider：
//!
//! - 请求体只有 `state` / `model` / `questions` 三个顶层字段。没有 `messages`、
//!   没有 `system`、没有 `tools`、没有 `stream`。
//! - 响应是一次性 JSON（`{model, answers, usage}`），没有 SSE、没有增量分块。
//! - 问题类型是 `choice` / `score` / `noul`，返回的是选项、分数与概率分布，
//!   模型**不生成任何文本**，因此没有「从散文里抠 JSON」这一步，也不存在
//!   chat 路径那种「模型返回未知决策」的解析失败。
//!
//! 所以 `OpenAiProvider` 的 URL 拼接（写死 `/chat/completions`）、SSE 解析、
//! 消息组装在这里一条都用不上。官方契约见 <https://docs.typesafe.ai/api>。
//!
//! # 重试与超时：与 chat 路径同源
//!
//! 本模块**直接持有全局 [`NetPolicy`] 本身**（而不是把它的字段抄一遍），
//! 于是「重试次数 / 重试延迟 / 哪些状态码重试 / 超时是否重试」与
//! `registry.rs::build_resolved` 灌进 `LlmConfig` 的是同一份数据、同一个来源。
//! 设置页改一次，两个引擎同时生效；也不存在第二套重试配置可以漂移。
//!
//! 重试判定复用 `LlmError::is_retryable` + `parse_retry_conditions`，预算口径
//! 复用 `max_retries + 1`。唯一的增量是：Jev 用非标准状态码 `529 Overloaded`，
//! 且官方建议遵守响应里的 `retry-after`——所以带 `retry-after` 时按它等待，
//! 没有时回落到 `NetPolicy.retry_delay_secs`。

use std::collections::BTreeMap;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE, RETRY_AFTER};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::watch;

use crate::llm::error::{format_reqwest_error, parse_retry_conditions, LlmError, RequestPhase};
use crate::llm::registry::NetPolicy;

/// Jev 的 API 根地址。真实端点 = `{base_url}/v1/systemone`。
pub const JEV_BASE_URL: &str = "https://api.typesafe.ai";

/// 默认模型 ID。
///
/// 刻意**钉版本号而不是 `jev-latest` 别名**：官方文档明说别名会随发布前移
/// （"An alias moves when a new release ships, so the answers behind it can
/// change without a change on your side"）。审批是个安全闸门，行为不该在
/// 用户不知情的情况下变。
pub const JEV_DEFAULT_MODEL: &str = "jev-1.13.0";

/// Jev 请求的响应体。
///
/// `answers` 与 `usage` 都刻意按 `Value` 反序列化而不是强类型，理由是同一个：
/// **这两个字段的形状不参与任何判定**，不该有能力让整包判废。
///
/// - `answers`：单个答案形状不对时，希望由调用方（它知道自己要问什么）判定为
///   「哑火」并重试，而不是让整个响应反序列化失败。
/// - `usage`：**纯统计展示**字段，全工程没有任何判定读它。上游把 token 数写成
///   浮点/字符串、或把 `usage` 写成 `null`，都不能让整包判废 —— 反序列化一旦失败，
///   `ask_once` 会把 HTTP 200 的响应当成「哑火」，重试预算耗尽后变成
///   `ParseError`，结果是**每一条 bash 都被拦下**。为一个只做展示的字段付这个
///   代价没有道理。
#[derive(Debug, Clone, Deserialize)]
pub struct JevResponse {
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub answers: BTreeMap<String, Value>,
    /// token 用量，只做统计展示（缺省 / 类型不符都不影响判定）。
    #[serde(default)]
    pub usage: Value,
}

impl JevResponse {
    /// 读一个 Choice 答案：`(选中项, confidence)`。
    ///
    /// 返回 `None` = 这个 id 没有可用答案（缺 key、类型不符、字段缺失），
    /// 调用方应视作一次哑火。
    pub fn choice(&self, id: &str) -> Option<(&str, Option<f64>)> {
        let answer = self.answers.get(id)?;
        if answer.get("type").and_then(Value::as_str) != Some("choice") {
            return None;
        }
        let choice = answer.get("choice").and_then(Value::as_str)?;
        Some((choice, answer.get("confidence").and_then(Value::as_f64)))
    }

    /// 读一个 Noul 答案（「是」的概率，0–1）。
    pub fn noul(&self, id: &str) -> Option<f64> {
        let answer = self.answers.get(id)?;
        if answer.get("type").and_then(Value::as_str) != Some("noul") {
            return None;
        }
        answer.get("noul").and_then(Value::as_f64)
    }
}

/// Jev 客户端配置。
#[derive(Debug, Clone)]
pub struct JevConfig {
    pub api_key: String,
    pub model_id: String,
    pub base_url: String,
    /// 与 chat 路径同源的全局网络/重试策略。
    pub net: NetPolicy,
}

impl JevConfig {
    pub fn new(api_key: String, model_id: String, net: NetPolicy) -> Self {
        Self {
            api_key,
            model_id: if model_id.trim().is_empty() {
                JEV_DEFAULT_MODEL.to_string()
            } else {
                model_id
            },
            base_url: JEV_BASE_URL.to_string(),
            net,
        }
    }

    /// 覆盖 API 根地址（企业代理 / 自建网关 / 指向本地 mock 做端到端测试）。
    ///
    /// 语义刻意是「空 = 不动」而不是「空 = 清空」：
    /// - 空字符串或纯空白 → 保留当前值（默认 `JEV_BASE_URL`），不报错、不改现状；
    /// - 非空 → 去掉首尾空白与结尾的 `/`，避免拼出 `//v1/systemone`。
    ///
    /// **不做 scheme 校验、也不因为格式可疑就静默回落默认值**：静默回落会让用户
    /// 以为请求打到了自己配的网关，其实打到了官方地址。格式不对就照原样带上，
    /// 让第一次请求以一个指名道姓的网络错误暴露出来。
    pub fn with_base_url(mut self, base_url: &str) -> Self {
        let trimmed = base_url.trim().trim_end_matches('/');
        if !trimmed.is_empty() {
            self.base_url = trimmed.to_string();
        }
        self
    }

    /// 是否真的能发请求（没 key 就不该构造出可用的客户端）。
    pub fn has_key(&self) -> bool {
        !self.api_key.trim().is_empty()
    }

    /// 本次请求用的完整端点。
    ///
    /// 抽成纯函数是为了能直接断言「自定义根地址确实作用到了请求上」——
    /// 否则这个设置项写错时，只能靠真的发请求才发现，而这类错误表现为
    /// 「每条 bash 都被拦下」，代价太大。
    pub fn endpoint_url(&self) -> String {
        format!("{}/v1/systemone", self.base_url.trim_end_matches('/'))
    }

    /// 是否在用非官方地址（用于在日志里点明现状）。
    pub fn is_custom_base_url(&self) -> bool {
        self.base_url.trim_end_matches('/') != JEV_BASE_URL
    }
}

/// 一次尝试的结果。
enum Attempt {
    /// 拿到 HTTP 200 且响应体是合法 JSON。
    Ok(Box<JevResponse>),
    /// HTTP 200 但响应体不可用（非法 JSON / 缺 `answers`）。语义等同于 chat 路径
    /// `is_effectively_empty_response` 的「模型哑火」：消费同一份重试预算，
    /// 而不是立刻判失败。
    Unusable(String),
    /// 传输层或 HTTP 状态错误，交给 `LlmError::is_retryable` 判定。
    Failed(LlmError),
}

/// Jev 客户端。无内部状态变更，可跨请求复用（连接池在 `reqwest::Client` 里）。
pub struct JevClient {
    config: JevConfig,
    client: reqwest::Client,
}

impl JevClient {
    pub fn new(config: JevConfig) -> Result<Self, LlmError> {
        let client = reqwest::Client::builder()
            // 连接池复用；单次请求的总超时按请求逐次设置（见 `ask_once`）。
            .pool_idle_timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| LlmError::Config(format!("Jev HTTP 客户端初始化失败: {}", e)))?;
        Ok(Self { config, client })
    }

    pub fn config(&self) -> &JevConfig {
        &self.config
    }

    /// 本次调用实际可用的总尝试次数（= 重试次数 + 首次）。
    ///
    /// 与 `LlmManager::stream_chat_internal` 的 `max_retries + 1` 同一口径。
    /// 暴露出来是为了让测试能钉住「预算确实来自 `NetPolicy`」——否则以后有人
    /// 给 Jev 悄悄加一套独立的重试设置，两个引擎的行为就会分叉而没人发现。
    pub(crate) fn retry_budget(&self) -> u32 {
        self.config.net.max_retries + 1
    }

    /// 重试间隔（`NetPolicy` 的固定延迟）。
    pub(crate) fn retry_delay(&self) -> Duration {
        Duration::from_secs_f32(self.config.net.retry_delay_secs.max(0.0))
    }

    /// 单次请求的总超时（沿用 chat 路径的 20–250s 夹取区间）。
    pub(crate) fn request_timeout(&self) -> Duration {
        Duration::from_secs(self.config.net.first_byte_timeout_secs.clamp(20, 250))
    }

    /// 向 Jev 提问，返回通过 `validate` 校验的响应。
    ///
    /// `validate` 由调用方给出「这个响应能不能用」的判据（它才知道自己要哪个
    /// 问题的答案）。校验不通过与「HTTP 200 但响应体坏掉」走同一条路：计入
    /// 可重试预算，预算耗尽返回 `LlmError::ParseError`。
    pub async fn ask(
        &self,
        state: &Value,
        questions: Value,
        validate: &(dyn Fn(&JevResponse) -> Result<(), String> + Send + Sync),
        mut cancel_rx: Option<&mut watch::Receiver<bool>>,
    ) -> Result<JevResponse, LlmError> {
        let conditions = parse_retry_conditions(&self.config.net.retry_http_statuses);
        let base_delay = self.retry_delay();
        // 与 `LlmManager::stream_chat_internal` 同一个预算口径。
        let max_attempts = self.retry_budget();
        let mut attempt: u32 = 0;

        loop {
            if let Some(rx) = cancel_rx.as_ref() {
                if *rx.borrow() {
                    return Err(LlmError::Cancelled);
                }
            }

            attempt += 1;
            let (outcome, retry_after) = self.ask_once(state, &questions).await;

            // 「哑火」与校验失败合并处理：两者都是「这次回答不可用」。
            let unusable: Option<String> = match outcome {
                Attempt::Ok(resp) => match validate(&resp) {
                    Ok(()) => return Ok(*resp),
                    Err(msg) => Some(msg),
                },
                Attempt::Unusable(msg) => Some(msg),
                Attempt::Failed(err) => {
                    let retryable = err.is_retryable(
                        // 非流式：在响应体到手之前不存在「首包已到」这个中间态，
                        // 所以全程按 Probing 判定（也正因如此，这里不会出现
                        // chat 路径那种「流已经开始就不能重试」的约束）。
                        RequestPhase::Probing,
                        &conditions,
                        self.config.net.retry_on_timeout,
                    );
                    if attempt < max_attempts && retryable {
                        let wait = retry_after.unwrap_or(base_delay);
                        log::warn!(
                            "Jev 请求失败 (尝试 {}/{}): {}，{}s 后重试",
                            attempt,
                            max_attempts,
                            err,
                            wait.as_secs_f32(),
                        );
                        if !sleep_or_cancel(wait, &mut cancel_rx).await {
                            return Err(LlmError::Cancelled);
                        }
                        continue;
                    }
                    return Err(err);
                }
            };

            let Some(msg) = unusable else { unreachable!() };
            if attempt >= max_attempts {
                return Err(LlmError::ParseError(msg));
            }
            log::warn!(
                "Jev 响应不可用 (尝试 {}/{}): {}，{}s 后重试",
                attempt,
                max_attempts,
                msg,
                base_delay.as_secs_f32(),
            );
            if !sleep_or_cancel(base_delay, &mut cancel_rx).await {
                return Err(LlmError::Cancelled);
            }
        }
    }

    /// 发一次请求。返回 `(结果, 响应里的 retry-after)`。
    async fn ask_once(&self, state: &Value, questions: &Value) -> (Attempt, Option<Duration>) {
        let url = self.config.endpoint_url();
        let body = json!({
            "state": state,
            "model": self.config.model_id,
            "questions": questions,
        });

        let headers = match self.build_headers() {
            Ok(h) => h,
            Err(e) => return (Attempt::Failed(e), None),
        };

        // 非流式没有「首字」概念，`first_byte_timeout_secs` 作为整个请求的总超时，
        // 沿用与 chat 路径相同的夹取区间。
        let total_timeout = self.request_timeout();

        let sent = self
            .client
            .post(&url)
            .headers(headers)
            .timeout(total_timeout)
            .json(&body)
            .send()
            .await;

        let response = match sent {
            Ok(r) => r,
            Err(e) => {
                let err = if e.is_timeout() {
                    LlmError::Timeout {
                        detail: format!(
                            "Jev 请求超时（{}s）",
                            self.config.net.first_byte_timeout_secs
                        ),
                    }
                } else {
                    LlmError::Network(format_reqwest_error(&e))
                };
                return (Attempt::Failed(err), None);
            }
        };

        let status = response.status();
        // 官方建议遵守 retry-after；没有该头时回落到 NetPolicy 的固定延迟。
        let retry_after = parse_retry_after(response.headers());

        if !status.is_success() {
            let body_text = response.text().await.unwrap_or_default();
            let err = LlmError::HttpStatus {
                status: status.as_u16(),
                body: truncate_body(&body_text),
            };
            return (Attempt::Failed(err), retry_after);
        }

        match response.json::<JevResponse>().await {
            Ok(resp) => {
                if resp.answers.is_empty() {
                    return (
                        Attempt::Unusable("Jev 响应没有 answers 字段".to_string()),
                        retry_after,
                    );
                }
                (Attempt::Ok(Box::new(resp)), retry_after)
            }
            Err(e) => (
                Attempt::Unusable(format!("Jev 响应体解析失败: {}", e)),
                retry_after,
            ),
        }
    }

    fn build_headers(&self) -> Result<HeaderMap, LlmError> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        let auth = format!("Bearer {}", self.config.api_key);
        let auth_header = HeaderValue::from_str(&auth)
            .map_err(|e| LlmError::Config(format!("Jev API Key 含非法字符: {}", e)))?;
        headers.insert(AUTHORIZATION, auth_header);
        Ok(headers)
    }
}

/// 等待 `wait`，期间响应取消信号。返回 `false` = 被取消。
async fn sleep_or_cancel(
    wait: Duration,
    cancel_rx: &mut Option<&mut watch::Receiver<bool>>,
) -> bool {
    match cancel_rx {
        Some(rx) => tokio::select! {
            _ = tokio::time::sleep(wait) => true,
            _ = rx.changed() => false,
        },
        None => {
            tokio::time::sleep(wait).await;
            true
        }
    }
}

/// 解析 `retry-after`。只接受秒数形式——官方文档承诺的是秒；HTTP-date 形式
/// 这里不认识就回落到 `NetPolicy` 的固定延迟，不会误判成一个巨大的等待。
fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    let raw = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    let secs: f32 = raw.parse().ok()?;
    if secs <= 0.0 {
        return None;
    }
    // 上限夹在总超时区间内，避免服务端给一个荒谬的值把任务挂死。
    Some(Duration::from_secs_f32(secs.min(250.0)))
}

/// 错误正文可能很长，截断后再放进 `LlmError`（避免日志与 UI 被刷屏）。
fn truncate_body(body: &str) -> String {
    const MAX: usize = 500;
    if body.len() <= MAX {
        return body.to_string();
    }
    let mut end = MAX;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…（已截断）", &body[..end])
}

/// 脚本化的假 Jev 端点，**只给测试用**。
///
/// 为什么值得手搓一个 HTTP 服务而不是继续加纯函数测试：本模块大部分代码
/// （URL 拼接、鉴权头、状态码分类、`retry-after` 解析、预算递减、取消打断）
/// 只在真的发过一次请求之后才会执行。纯函数测试触不到这些路径，于是
/// 「401 会不会被误判成可重试」「预算到底用没用完」这类问题只能等上线才暴露，
/// 而它暴露的代价是每条 bash 都被拦下。
#[cfg(test)]
pub(crate) mod mock_server {
    use std::sync::{Arc, Mutex, MutexGuard};

    use serde_json::Value;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// 一次脚本化应答。
    #[derive(Clone)]
    pub(crate) struct Reply {
        status: u16,
        retry_after: Option<String>,
        body: String,
    }

    impl Reply {
        /// 正常应答：JSON 原样回。
        pub(crate) fn json(status: u16, body: Value) -> Self {
            Self {
                status,
                retry_after: None,
                body: body.to_string(),
            }
        }

        /// 原始应答体（用来造非法 JSON 这类畸形响应）。
        pub(crate) fn raw(status: u16, body: &str) -> Self {
            Self {
                status,
                retry_after: None,
                body: body.to_string(),
            }
        }

        pub(crate) fn with_retry_after(mut self, secs: &str) -> Self {
            self.retry_after = Some(secs.to_string());
            self
        }
    }

    #[derive(Default)]
    struct Received {
        targets: Vec<String>,
        auth: Vec<String>,
        bodies: Vec<Value>,
    }

    /// 脚本化的 Jev 端点：`replies` 按收到的顺序逐条应答，用完后重复最后一条。
    pub(crate) struct MockJev {
        /// 可以直接喂给 `JevConfig::with_base_url`。
        pub(crate) base_url: String,
        received: Arc<Mutex<Received>>,
    }

    impl MockJev {
        pub(crate) async fn start(replies: Vec<Reply>) -> Self {
            assert!(!replies.is_empty(), "mock 至少要有一条应答");
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("mock 端点绑定 127.0.0.1 失败");
            let addr = listener.local_addr().expect("读取 mock 端口失败");
            let received = Arc::new(Mutex::new(Received::default()));
            let sink = Arc::clone(&received);

            tokio::spawn(async move {
                let mut index = 0usize;
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        return;
                    };
                    let Ok((head, body)) = read_request(&mut stream).await else {
                        continue;
                    };
                    {
                        let mut got = sink.lock().expect("mock 记录锁被毒化");
                        got.targets.push(request_target(&head));
                        got.auth
                            .push(header_value(&head, "authorization").unwrap_or_default());
                        if let Ok(value) = serde_json::from_str::<Value>(&body) {
                            got.bodies.push(value);
                        }
                    }

                    let reply = replies
                        .get(index)
                        .or_else(|| replies.last())
                        .expect("脚本非空")
                        .clone();
                    index += 1;
                    let _ = stream.write_all(render(&reply).as_bytes()).await;
                    let _ = stream.flush().await;
                    let _ = stream.shutdown().await;
                }
            });

            Self {
                base_url: format!("http://{addr}"),
                received,
            }
        }

        /// 收到过几次请求——**重试预算要用它断言**，而不是「调用返回了没有」。
        pub(crate) fn request_count(&self) -> usize {
            self.lock().targets.len()
        }

        pub(crate) fn request_targets(&self) -> Vec<String> {
            self.lock().targets.clone()
        }

        pub(crate) fn auth_headers(&self) -> Vec<String> {
            self.lock().auth.clone()
        }

        pub(crate) fn request_bodies(&self) -> Vec<Value> {
            self.lock().bodies.clone()
        }

        fn lock(&self) -> MutexGuard<'_, Received> {
            self.received.lock().expect("mock 记录锁被毒化")
        }
    }

    fn render(reply: &Reply) -> String {
        let mut out = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
            reply.status,
            reason_phrase(reply.status),
            // `String::len` 是字节数，正对 Content-Length
            reply.body.len(),
        );
        if let Some(after) = &reply.retry_after {
            out.push_str(&format!("Retry-After: {after}\r\n"));
        }
        out.push_str("\r\n");
        out.push_str(&reply.body);
        out
    }

    fn reason_phrase(status: u16) -> &'static str {
        match status {
            200 => "OK",
            400 => "Bad Request",
            401 => "Unauthorized",
            404 => "Not Found",
            422 => "Unprocessable Entity",
            429 => "Too Many Requests",
            500 => "Internal Server Error",
            503 => "Service Unavailable",
            _ => "Status",
        }
    }

    /// 读一个完整请求（含 `Content-Length` 指明的那段 body）。
    async fn read_request(stream: &mut TcpStream) -> std::io::Result<(String, String)> {
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            if let Some(head_end) = find(&buf, b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
                let body_start = head_end + 4;
                let want = header_value(&head, "content-length")
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if buf.len() >= body_start + want {
                    let body =
                        String::from_utf8_lossy(&buf[body_start..body_start + want]).into_owned();
                    return Ok((head, body));
                }
            }
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                // 对端提前关了连接：把读到的原样交出去，别在这儿干等。
                return Ok((String::from_utf8_lossy(&buf).into_owned(), String::new()));
            }
            buf.extend_from_slice(&chunk[..n]);
        }
    }

    /// 请求行里的 path（`POST /v1/systemone HTTP/1.1` → `/v1/systemone`）。
    fn request_target(head: &str) -> String {
        head.lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or_default()
            .to_string()
    }

    fn header_value(head: &str, name: &str) -> Option<String> {
        head.lines().skip(1).find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_string())
        })
    }

    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn resp(json: Value) -> JevResponse {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn reads_choice_answer_with_confidence() {
        let r = resp(json!({
            "model": "jev-1.13.0",
            "answers": {
                "decision": {
                    "type": "choice",
                    "choice": "route_to_human",
                    "confidence": 0.62,
                    "probabilities": {"approve": 0.2, "route_to_human": 0.62, "block": 0.18}
                }
            }
        }));
        let (choice, confidence) = r.choice("decision").unwrap();
        assert_eq!(choice, "route_to_human");
        assert_eq!(confidence, Some(0.62));
    }

    #[test]
    fn choice_missing_or_wrong_type_is_none() {
        // 缺这个 id
        let r = resp(json!({"answers": {}}));
        assert!(r.choice("decision").is_none());
        // type 不符（同 id 返回了 noul）
        let r = resp(json!({"answers": {"decision": {"type": "noul", "noul": 1.0}}}));
        assert!(r.choice("decision").is_none());
        // type 对但缺 choice 字段
        let r = resp(json!({"answers": {"decision": {"type": "choice"}}}));
        assert!(r.choice("decision").is_none());
    }

    #[test]
    fn confidence_is_optional() {
        // 官方只在 Choice/Score 给 confidence；缺失不应导致解析失败。
        let r = resp(json!({"answers": {"decision": {"type": "choice", "choice": "approve"}}}));
        assert_eq!(r.choice("decision"), Some(("approve", None)));
    }

    #[test]
    fn reads_noul_answer() {
        let r = resp(json!({
            "answers": {
                "deletes_data": {"type": "noul", "noul": 0.93},
                "changes_system": {"type": "noul", "noul": 0.02},
            }
        }));
        assert_eq!(r.noul("deletes_data"), Some(0.93));
        assert_eq!(r.noul("changes_system"), Some(0.02));
        assert!(r.noul("missing").is_none());
    }

    #[test]
    fn parses_usage_as_untyped_stats() {
        // 缺 usage：保持缺失，不伪造形状（Null 就是 Null）。
        let r = resp(json!({"answers": {"d": {"type": "noul", "noul": 0.5}}}));
        assert!(r.usage.is_null());

        let r = resp(json!({
            "model": "jev-1.13.0",
            "answers": {"d": {"type": "noul", "noul": 0.5}},
            "usage": {"input_tokens": 392, "output_tokens": 65}
        }));
        assert_eq!(r.model, "jev-1.13.0");
        assert_eq!(r.usage["input_tokens"], json!(392));
        assert_eq!(r.usage["output_tokens"], json!(65));
    }

    /// **回归：`usage` 的形状变化不得让整包判废。**
    ///
    /// `usage` 全工程无人读取（只做统计展示），却曾是强类型 —— 上游把 token 数
    /// 写成浮点/字符串、或把 `usage` 写成 `null`，都会让 `response.json::<JevResponse>()`
    /// 失败，走「哑火」重试、耗尽预算后变成 `ParseError`，于是每条 bash 都被拦。
    /// 这个测试钉住 `answers` 在这些形状下依然读得出来。
    #[test]
    fn usage_shape_never_invalidates_the_answers() {
        // 上游可能出现的形状：null / 数字 / 数组 / token 数换型 / 换字段名。
        let shapes = [
            json!(null),
            json!(12345),
            json!([1, 2]),
            json!({"input_tokens": 392.5, "output_tokens": "65"}),
            json!({"inputTokens": 392}),
        ];
        for usage in shapes {
            let r = resp(json!({
                "model": "jev-1.13.0",
                "answers": {"decision": {"type": "choice", "choice": "approve"}},
                "usage": usage,
            }));
            assert_eq!(
                r.choice("decision"),
                Some(("approve", None)),
                "usage 换成 {usage} 不该影响 answers 的解析"
            );
        }

        // 字段整体缺失同样可用（Noul 之类也一样读得出来）。
        let r = resp(json!({"answers": {"d": {"type": "noul", "noul": 0.93}}}));
        assert_eq!(r.noul("d"), Some(0.93));
    }

    #[test]
    fn retry_after_reads_seconds() {
        let mut h = HeaderMap::new();
        h.insert(RETRY_AFTER, HeaderValue::from_static("7"));
        assert_eq!(parse_retry_after(&h), Some(Duration::from_secs(7)));
    }

    #[test]
    fn retry_after_ignores_garbage_and_clamps() {
        let mut h = HeaderMap::new();
        // HTTP-date 形式：不认识就回落固定延迟，不能当成一个巨大的等待
        h.insert(
            RETRY_AFTER,
            HeaderValue::from_static("Wed, 21 Oct 2026 07:28:00 GMT"),
        );
        assert_eq!(parse_retry_after(&h), None);

        h.insert(RETRY_AFTER, HeaderValue::from_static("0"));
        assert_eq!(parse_retry_after(&h), None);

        h.insert(RETRY_AFTER, HeaderValue::from_static("100000"));
        assert_eq!(parse_retry_after(&h), Some(Duration::from_secs(250)));

        assert_eq!(parse_retry_after(&HeaderMap::new()), None);
    }

    #[test]
    fn empty_model_falls_back_to_pinned_default() {
        let cfg = JevConfig::new("k".into(), "   ".into(), NetPolicy::default());
        assert_eq!(cfg.model_id, JEV_DEFAULT_MODEL);
    }

    // ── 根地址覆盖 ────────────────────────────────────────────────

    #[test]
    fn endpoint_url_defaults_to_official() {
        let cfg = JevConfig::new("k".into(), JEV_DEFAULT_MODEL.into(), NetPolicy::default());
        assert_eq!(cfg.endpoint_url(), "https://api.typesafe.ai/v1/systemone");
        assert!(!cfg.is_custom_base_url());
    }

    #[test]
    fn custom_base_url_is_used_and_trailing_slash_trimmed() {
        let cfg = JevConfig::new("k".into(), JEV_DEFAULT_MODEL.into(), NetPolicy::default())
            .with_base_url("https://jev-gw.corp.example.com/");
        // 结尾斜杠必须去掉，否则会拼出 `//v1/systemone`。
        assert_eq!(
            cfg.endpoint_url(),
            "https://jev-gw.corp.example.com/v1/systemone"
        );
        assert!(cfg.is_custom_base_url());
    }

    #[test]
    fn custom_base_url_accepts_localhost_for_mock_testing() {
        let cfg = JevConfig::new("k".into(), JEV_DEFAULT_MODEL.into(), NetPolicy::default())
            .with_base_url("http://127.0.0.1:8787");
        assert_eq!(cfg.endpoint_url(), "http://127.0.0.1:8787/v1/systemone");
    }

    /// 「空 = 不动」而不是「空 = 清空」：旧配置/未填时保持官方地址，
    /// 不因为一个空字符串把请求打到一个空主机上。
    #[test]
    fn empty_or_whitespace_base_url_keeps_official() {
        for raw in ["", "   ", "\t"] {
            let cfg = JevConfig::new("k".into(), JEV_DEFAULT_MODEL.into(), NetPolicy::default())
                .with_base_url(raw);
            assert_eq!(
                cfg.endpoint_url(),
                "https://api.typesafe.ai/v1/systemone",
                "空值 {:?} 不得覆盖默认地址",
                raw,
            );
            assert!(!cfg.is_custom_base_url());
        }
    }

    /// 覆盖后再传空值，保留的是**上一次的设置**而不是回到官方地址——
    /// 同样体现「空即不动」。
    #[test]
    fn later_empty_override_does_not_clear_previous_value() {
        let cfg = JevConfig::new("k".into(), JEV_DEFAULT_MODEL.into(), NetPolicy::default())
            .with_base_url("https://gw.example.com")
            .with_base_url("");
        assert_eq!(cfg.base_url, "https://gw.example.com");
    }

    /// 格式可疑（缺 scheme）**不静默回落默认地址**：回落会让用户以为请求打到了
    /// 自己配的网关，其实打到了官方。照原样带上，让第一次请求以指名道姓的网络
    /// 错误暴露出来。
    #[test]
    fn malformed_url_is_kept_not_silently_replaced() {
        let cfg = JevConfig::new("k".into(), JEV_DEFAULT_MODEL.into(), NetPolicy::default())
            .with_base_url("jev-gw.corp.example.com");
        assert_eq!(cfg.base_url, "jev-gw.corp.example.com");
        assert!(cfg.is_custom_base_url());
    }

    #[test]
    fn net_policy_is_held_not_copied() {
        // 这个测试的价值在于「改了策略立刻生效」是构造性成立的：JevConfig 持有
        // NetPolicy 本身，客户端每次调用都从它读，不存在构造时快照一份的可能。
        let net = NetPolicy {
            max_retries: 7,
            retry_delay_secs: 1.5,
            ..NetPolicy::default()
        };
        let cfg = JevConfig::new("k".into(), JEV_DEFAULT_MODEL.into(), net.clone());
        assert_eq!(cfg.net, net);
        // 与 chat 路径同源：两者都从同一个 NetPolicy 取值。
        let conditions = parse_retry_conditions(&cfg.net.retry_http_statuses);
        assert!(!conditions.is_empty(), "默认策略必须包含可重试状态码");
    }

    #[test]
    fn truncate_body_respects_char_boundary() {
        let long = "危".repeat(400); // 每个字 3 字节
        let out = truncate_body(&long);
        assert!(out.ends_with("已截断）"));
        assert!(!out.contains('\u{FFFD}'));
    }

    /// 护栏：重试预算、重试间隔、总超时**必须**从全局 `NetPolicy` 取，
    /// 且预算口径与 chat 路径的 `max_retries + 1` 一致。
    ///
    /// 这条测试的意义是防止以后有人给 Jev 加一套独立的重试配置：那样一来
    /// 「重试」这条规则就有了两个权威来源，用户在设置页改的次数只对其中一个生效。
    #[test]
    fn retry_policy_comes_from_net_policy() {
        let net = NetPolicy {
            max_retries: 3,
            retry_delay_secs: 2.5,
            first_byte_timeout_secs: 90,
            // 与 chat 路径同一份状态码配置
            retry_http_statuses: "429, 500-599, 529".into(),
            retry_on_timeout: false,
        };
        let client = JevClient::new(JevConfig::new("k".into(), JEV_DEFAULT_MODEL.into(), net))
            .expect("客户端构造应成功");

        assert_eq!(client.retry_budget(), 4, "预算必须是 max_retries + 1");
        assert_eq!(client.retry_delay(), Duration::from_millis(2500));
        assert_eq!(client.request_timeout(), Duration::from_secs(90));

        // 重试分类复用 chat 路径的同一套判定函数（此处只验证它按 Probing 语义工作）。
        let conditions = parse_retry_conditions(&client.config().net.retry_http_statuses);
        assert!(parse_retry_conditions("429, 500-599, 529")
            .iter()
            .all(|c| conditions.contains(c)));
        assert!(!LlmError::Timeout { detail: "x".into() }.is_retryable(
            RequestPhase::Probing,
            &conditions,
            false,
        ));
    }

    #[test]
    fn request_timeout_follows_the_shared_clamp() {
        let mk = |secs: u64| {
            JevClient::new(JevConfig::new(
                "k".into(),
                JEV_DEFAULT_MODEL.into(),
                NetPolicy {
                    first_byte_timeout_secs: secs,
                    ..NetPolicy::default()
                },
            ))
            .unwrap()
            .request_timeout()
        };
        // 与 chat 路径相同的 20–250s 夹取区间
        assert_eq!(mk(1), Duration::from_secs(20));
        assert_eq!(mk(90), Duration::from_secs(90));
        assert_eq!(mk(100_000), Duration::from_secs(250));
    }

    // ── 端到端：真发请求、真解析、真重试（本地 mock 端点，不需要 Key） ──────
    //
    // 这一组补的是「HTTP 路径从没真正执行过」这个缺口。上面那些纯函数测试证明的是
    // 「函数算得对」，这里证明的是「这条路真的走得通」——两者缺一不可。

    use super::mock_server::{MockJev, Reply};

    fn ok_body(choice: &str) -> Value {
        json!({
            "model": JEV_DEFAULT_MODEL,
            "answers": {
                "decision": {"type": "choice", "choice": choice, "confidence": 0.9},
                "deletes_data": {"type": "noul", "noul": 0.1},
            },
            "usage": {"input_tokens": 120, "output_tokens": 4},
        })
    }

    fn requires_decision(resp: &JevResponse) -> Result<(), String> {
        resp.choice("decision")
            .map(|_| ())
            .ok_or_else(|| "缺少 decision".to_string())
    }

    fn client_at(base_url: &str, net: NetPolicy) -> JevClient {
        JevClient::new(
            JevConfig::new("test-key".into(), JEV_DEFAULT_MODEL.into(), net)
                .with_base_url(base_url),
        )
        .expect("客户端构造应成功")
    }

    /// 重试延迟置 0 的快速策略（测试不想真的等）——状态码配置与线上同源写法一致。
    fn fast_net(max_retries: u32) -> NetPolicy {
        NetPolicy {
            max_retries,
            retry_delay_secs: 0.0,
            retry_http_statuses: "429, 500-599, 529".into(),
            retry_on_timeout: false,
            ..NetPolicy::default()
        }
    }

    #[tokio::test]
    async fn request_goes_to_the_configured_endpoint_with_the_expected_shape() {
        let mock = MockJev::start(vec![Reply::json(200, ok_body("approve"))]).await;
        let client = client_at(&mock.base_url, fast_net(0));

        let state = json!({"command": "ls", "conversation": []});
        let questions = json!({"decision": {"type": "choice", "instructions": "判断这条命令"}});
        let resp = client
            .ask(&state, questions, &requires_decision, None)
            .await
            .expect("mock 回了正常响应，不该失败");

        assert_eq!(resp.choice("decision"), Some(("approve", Some(0.9))));

        // 路径来自 `endpoint_url()` 而不是写死的字符串
        assert_eq!(mock.request_targets(), vec!["/v1/systemone".to_string()]);
        // 鉴权头用的就是配置里的 key
        assert_eq!(mock.auth_headers(), vec!["Bearer test-key".to_string()]);

        // 请求体形状 = 官方契约：顶层只有 state / model / questions
        let body = &mock.request_bodies()[0];
        assert_eq!(body["state"]["command"], "ls");
        assert_eq!(body["model"], JEV_DEFAULT_MODEL);
        assert_eq!(body["questions"]["decision"]["type"], "choice");
        assert!(
            body.get("messages").is_none(),
            "Jev 不是 chat 模型，请求体里不该出现 messages"
        );
        assert!(
            body.get("stream").is_none(),
            "Jev 非流式，请求体里不该出现 stream"
        );
    }

    /// `retry-after` 必须真的被遵守。
    ///
    /// 手法：把固定延迟设成 30s、应答里给 1s。实现如果忽略了 `retry-after`，
    /// 这个测试会真的等 30 秒——用耗时把它钉死（改坏了一定会红得很难看）。
    #[tokio::test]
    async fn retry_after_header_is_honoured_instead_of_the_fixed_delay() {
        let mock = MockJev::start(vec![
            Reply::json(429, json!({"detail": "slow down"})).with_retry_after("1"),
            Reply::json(200, ok_body("approve")),
        ])
        .await;
        let net = NetPolicy {
            max_retries: 1,
            retry_delay_secs: 30.0,
            retry_http_statuses: "429, 500-599, 529".into(),
            ..NetPolicy::default()
        };
        let client = client_at(&mock.base_url, net);

        let started = Instant::now();
        let resp = client
            .ask(&json!({}), json!({}), &requires_decision, None)
            .await
            .expect("第二次应答是正常的，该成功");
        let elapsed = started.elapsed();

        assert_eq!(mock.request_count(), 2, "429 应触发一次重试");
        assert_eq!(resp.choice("decision").unwrap().0, "approve");
        assert!(
            elapsed < Duration::from_secs(10),
            "必须按 retry-after(1s) 等待而不是固定延迟(30s)，实际 {elapsed:?}"
        );
    }

    /// 401 不可重试：一次就返回，后面的重试预算一点都不能动。
    #[tokio::test]
    async fn unauthorized_is_not_retried() {
        let mock = MockJev::start(vec![
            Reply::raw(401, r#"{"detail":{"message":"invalid api key"}}"#),
            Reply::json(200, ok_body("approve")),
        ])
        .await;
        let client = client_at(&mock.base_url, fast_net(3));

        let err = client
            .ask(&json!({}), json!({}), &requires_decision, None)
            .await
            .expect_err("401 必须失败");

        assert!(
            matches!(err, LlmError::HttpStatus { status: 401, .. }),
            "实际 {err:?}"
        );
        assert_eq!(mock.request_count(), 1, "401 不该重试");
    }

    /// 5xx 可重试，且预算**恰好**是 `NetPolicy.max_retries + 1`，不多不少。
    #[tokio::test]
    async fn retryable_status_exhausts_exactly_the_net_policy_budget() {
        let mock = MockJev::start(vec![Reply::json(503, json!({"detail": "overloaded"}))]).await;
        let client = client_at(&mock.base_url, fast_net(2));

        let err = client
            .ask(&json!({}), json!({}), &requires_decision, None)
            .await
            .expect_err("一直 503 必须最终失败");

        assert!(
            matches!(err, LlmError::HttpStatus { status: 503, .. }),
            "实际 {err:?}"
        );
        assert_eq!(mock.request_count(), 3, "max_retries=2 → 总共 3 次尝试");
    }

    /// HTTP 200 但响应不可用 = chat 路径的「模型哑火」：消费同一份重试预算，
    /// 耗尽后 fail-closed 报错（而不是当成一次空判定放行）。
    ///
    /// 三种不可用形态都试一遍，因为它们在实现里走的是不同的判定分支。
    #[tokio::test]
    async fn unusable_200_response_consumes_the_same_budget() {
        // ① 非法 JSON
        let mock = MockJev::start(vec![Reply::raw(200, "这不是 JSON")]).await;
        let client = client_at(&mock.base_url, fast_net(1));
        let err = client
            .ask(&json!({}), json!({}), &requires_decision, None)
            .await
            .expect_err("非法 JSON 必须最终失败");
        assert!(matches!(err, LlmError::ParseError(_)), "实际 {err:?}");
        assert_eq!(mock.request_count(), 2, "哑火要消耗重试预算");

        // ② 合法 JSON 但 answers 为空
        let mock = MockJev::start(vec![Reply::json(200, json!({"answers": {}}))]).await;
        let client = client_at(&mock.base_url, fast_net(1));
        let err = client
            .ask(&json!({}), json!({}), &requires_decision, None)
            .await
            .expect_err("answers 为空必须最终失败");
        assert!(matches!(err, LlmError::ParseError(_)), "实际 {err:?}");
        assert_eq!(mock.request_count(), 2);

        // ③ 合法 JSON、answers 非空，但校验器要的那个答案不在里面
        let mock = MockJev::start(vec![Reply::json(
            200,
            json!({"answers": {"other": {"type": "noul", "noul": 0.4}}}),
        )])
        .await;
        let client = client_at(&mock.base_url, fast_net(1));
        let err = client
            .ask(&json!({}), json!({}), &requires_decision, None)
            .await
            .expect_err("缺 decision 必须最终失败");
        assert!(matches!(err, LlmError::ParseError(_)), "实际 {err:?}");
        assert_eq!(mock.request_count(), 2);
    }

    /// 取消信号必须能立刻打断重试等待。
    ///
    /// 注意现状：`JevApprover` 目前传 `None`（与 chat 引擎一致——审批调用不响应
    /// 停止），所以这条路径在生产上还没接上。它是 `ask` 的公开能力，一旦接上就
    /// 必须是对的，所以在这里钉住；顺带也让「审批调用不响应停止」这个已知缺口
    /// 在测试里留个明确的记号。
    #[tokio::test]
    async fn cancellation_interrupts_the_retry_wait() {
        let mock = MockJev::start(vec![Reply::json(503, json!({"detail": "overloaded"}))]).await;
        // 固定延迟 30s：取消若没生效，这个测试会一路等下去。
        let net = NetPolicy {
            max_retries: 5,
            retry_delay_secs: 30.0,
            retry_http_statuses: "429, 500-599, 529".into(),
            ..NetPolicy::default()
        };
        let client = client_at(&mock.base_url, net);
        let (tx, rx) = watch::channel(false);

        let started = Instant::now();
        let handle = tokio::spawn(async move {
            let mut rx = rx;
            let validate = requires_decision;
            client
                .ask(&json!({}), json!({}), &validate, Some(&mut rx))
                .await
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        tx.send(true).expect("取消信号应送达");

        let err = handle
            .await
            .expect("任务不该 panic")
            .expect_err("取消应返回错误");
        let elapsed = started.elapsed();

        assert!(matches!(err, LlmError::Cancelled), "实际 {err:?}");
        assert!(
            elapsed < Duration::from_secs(10),
            "取消应立刻打断等待，实际 {elapsed:?}"
        );
        assert_eq!(mock.request_count(), 1, "取消后不该再发请求");
    }
}
