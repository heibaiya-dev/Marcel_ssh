//! DeepSeek 官方「工具类」REST 接口（`/v1/tools/search`、`/v1/tools/fetch`）的共用客户端。
//! 文档：<https://api-docs.deepseek.com/zh-cn/guides/search>
//!
//! 搜索与网页获取是同一家的一对接口：认证、计费口径、限速、错误体、重试规则完全一致，
//! 所以只在这里写一份，`web_search` 与 `http_get` 各自只管请求体和响应体。
//!
//! 要点：
//! - **密钥是独立槽位**（keychain `deepseek_tools_api_key`），搜索与获取共用。刻意不借
//!   `web_search_api_key`：它存的是 Brave/Tavily 的 Key，借来就会把别家的 Key 发给 DeepSeek。
//! - **按成功请求计费、账号级 5 QPS**：agent 一轮里会并发多个调用，429 是预期内的回答，
//!   按官方建议退避重试，而不是直接抛给用户。
//! - **请求由 DeepSeek 的服务器发出**，这与本地浏览器/裸 HTTP 获取的"从这台电脑发起"是
//!   两回事——哪些地址允许交给它，由调用方决定（见 `http_get::is_private_target`）。

use std::time::Duration;

use reqwest::{Client, StatusCode};
use serde_json::Value;

use crate::agent::tools::web_result::truncate_error_body;
use crate::config::keychain::{self, SecretSlot};
use crate::error::AppError;

const TOOLS_BASE: &str = "https://api.deepseek.com/v1/tools";
const RETRY_BACKOFF: Duration = Duration::from_millis(500);

/// 读取 DeepSeek 工具类 Key；没配或为空白时给出能指向设置页的错误。
pub(crate) fn load_api_key() -> Result<String, AppError> {
    keychain::get(&SecretSlot::DeepseekToolsApiKey)?
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty())
        .ok_or_else(|| {
            AppError::Agent(
                "DeepSeek API key is not configured; set it in Settings → Agent tools".into(),
            )
        })
}

/// `POST {TOOLS_BASE}/{path}`，成功返回响应正文（JSON 文本）。
pub(crate) async fn post_json(
    path: &str,
    api_key: &str,
    payload: &Value,
    timeout: Duration,
) -> Result<String, AppError> {
    post_json_at(TOOLS_BASE, path, api_key, payload, timeout, RETRY_BACKOFF).await
}

/// 基址与退避可注入，只为让重试逻辑能用本机回环服务器真实地跑一遍。
async fn post_json_at(
    base: &str,
    path: &str,
    api_key: &str,
    payload: &Value,
    timeout: Duration,
    backoff: Duration,
) -> Result<String, AppError> {
    let url = format!("{}/{}", base, path);
    let who = format!("DeepSeek {}", path);
    post_with_retry(
        &url,
        &who,
        api_key,
        payload,
        timeout,
        backoff,
        &|status, body| http_error(path, status, body),
    )
    .await
}

/// 用户自备地址、按 DeepSeek 协议收发的 POST（搜索与抓取共用）：同样的 408/429/5xx 退避重试，
/// 但没有 DeepSeek 专属的处置提示（"去 DeepSeek 充值"对别人的服务毫无意义），Key 可以为空
/// （自建服务常常不鉴权）——空则不发 `Authorization` 头。`who` 是错误文案里的服务称呼。
pub(crate) async fn post_json_custom(
    url: &str,
    who: &'static str,
    api_key: &str,
    payload: &Value,
    timeout: Duration,
) -> Result<String, AppError> {
    post_with_retry(
        url,
        who,
        api_key,
        payload,
        timeout,
        RETRY_BACKOFF,
        &|status, body| {
            AppError::Agent(format!(
                "{} HTTP {}: {}",
                who,
                status,
                truncate_error_body(&error_detail(body))
            ))
        },
    )
    .await
}

/// 用户自己填的地址：信任它可以指向任何地方，但每个请求都带着 Key，所以收得很窄——
/// 只收有主机名的 `http(s)` 地址，且**不许内嵌账号密码**（那会明文落进 settings.json，
/// Key 有自己的密钥链字段）。返回规范化后的地址。`what` 是错误文案里对该设置项的称呼。
pub(crate) fn validate_endpoint_url(raw: &str, what: &str) -> Result<String, AppError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(AppError::Agent(format!(
            "{what} URL is not configured; set it in Settings → Agent tools"
        )));
    }
    let parsed = url::Url::parse(raw)
        .map_err(|e| AppError::Agent(format!("{what} URL is invalid: {}", e)))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host().is_none() {
        return Err(AppError::Agent(format!(
            "{what} URL must be an http:// or https:// address"
        )));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(AppError::Agent(format!(
            "{what} URL must not embed a username or password; put the key in the API Key field"
        )));
    }
    Ok(parsed.to_string())
}

async fn post_with_retry(
    url: &str,
    who: &str,
    api_key: &str,
    payload: &Value,
    timeout: Duration,
    backoff: Duration,
    to_error: &(dyn Fn(StatusCode, &str) -> AppError + Sync),
) -> Result<String, AppError> {
    let client = Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| AppError::Agent(format!("failed to create HTTP client: {}", e)))?;
    let key = api_key.trim();

    let mut attempt = 0usize;
    loop {
        attempt += 1;
        let mut request = client.post(url).json(payload);
        if !key.is_empty() {
            request = request.bearer_auth(key);
        }
        let resp = request
            .send()
            .await
            .map_err(|e| AppError::Agent(format!("{} request failed: {}", who, e)))?;

        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| AppError::Agent(format!("failed to read {} response: {}", who, e)))?;

        if status.is_success() {
            return Ok(body);
        }
        if attempt < max_attempts(status) {
            tokio::time::sleep(backoff * attempt as u32).await;
            continue;
        }
        return Err(to_error(status, &body));
    }
}

/// 错误体里人能读的那句话：OpenAI 风格的 `error.message`、Tavily 的 `detail.error`，
/// 或字符串形式的 `detail`；都不是就原样用正文。
fn error_detail(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v["error"]["message"]
                .as_str()
                .or_else(|| v["detail"]["error"].as_str())
                .or_else(|| v["detail"].as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.to_string())
}

/// 各状态码允许的总尝试次数（含第一次）。官方指引：408/429/5xx 是暂时性的，等一会儿
/// 重试；其余 4xx 不改请求就再发一次也是同样的结果，只会白白多花一次请求。
pub(crate) fn max_attempts(status: StatusCode) -> usize {
    match status.as_u16() {
        408 | 429 => 3,
        500..=599 => 2,
        _ => 1,
    }
}

/// 把非 2xx 回复变成用户能照着办的错误。响应体是 OpenAI 风格的
/// `{"error":{"message":..}}`；401/402/429 附上处置提示——"检查 Key"和"去充值"是两码事。
pub(crate) fn http_error(path: &str, status: StatusCode, body: &str) -> AppError {
    let detail = error_detail(body);
    let hint = match status.as_u16() {
        401 => " — the API key is missing or invalid; use your DeepSeek API key",
        402 => " — insufficient DeepSeek balance; top up the account and retry",
        429 => " — over the account-level rate limit (5 QPS); retry shortly",
        _ => "",
    };
    AppError::Agent(format!(
        "DeepSeek {} HTTP {}: {}{}",
        path,
        status,
        truncate_error_body(&detail),
        hint
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// 依次回放预设的 HTTP 响应，并记下每次收到的请求头+体；返回基址与请求记录。
    fn serve_sequence(responses: Vec<(u16, &'static str)>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_in_thread = Arc::clone(&seen);
        std::thread::spawn(move || {
            for (status, body) in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).unwrap_or(0);
                seen_in_thread
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf[..n]).into_owned());
                let reply = format!(
                    "HTTP/1.1 {} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    status,
                    body.len(),
                    body
                );
                let _ = stream.write_all(reply.as_bytes());
            }
        });
        (format!("http://{}", addr), seen)
    }

    async fn post(base: &str) -> Result<String, AppError> {
        post_json_at(
            base,
            "search",
            "  sk-test-key \n",
            &serde_json::json!({"query": "q"}),
            Duration::from_secs(5),
            Duration::ZERO,
        )
        .await
    }

    #[tokio::test]
    async fn sends_the_trimmed_key_as_a_bearer_token_to_the_tool_path() {
        let (base, seen) = serve_sequence(vec![(200, r#"{"results":[]}"#)]);
        let body = post(&base).await.expect("200");
        assert_eq!(body, r#"{"results":[]}"#);

        let request = seen.lock().unwrap()[0].to_ascii_lowercase();
        assert!(request.starts_with("post /search "), "{request}");
        assert!(
            request.contains("authorization: bearer sk-test-key\r\n"),
            "{request}"
        );
        assert!(request.contains(r#"{"query":"q"}"#), "{request}");
    }

    #[tokio::test]
    async fn rate_limit_is_retried_until_it_clears() {
        let (base, seen) = serve_sequence(vec![
            (429, r#"{"error":{"message":"slow down"}}"#),
            (429, r#"{"error":{"message":"slow down"}}"#),
            (200, r#"{"results":[1]}"#),
        ]);
        let body = post(&base).await.expect("third attempt succeeds");
        assert_eq!(body, r#"{"results":[1]}"#);
        assert_eq!(seen.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn a_permanent_rate_limit_gives_up_with_the_hint() {
        let (base, seen) = serve_sequence(vec![(429, "{}"), (429, "{}"), (429, "{}"), (200, "{}")]);
        let err = post(&base).await.expect_err("three 429s");
        assert!(err.to_string().contains("5 QPS"), "{err}");
        assert_eq!(seen.lock().unwrap().len(), 3, "no fourth attempt");
    }

    #[tokio::test]
    async fn auth_failures_are_not_retried() {
        let (base, seen) = serve_sequence(vec![
            (401, r#"{"error":{"message":"Authentication Fails"}}"#),
            (200, "{}"),
        ]);
        let err = post(&base).await.expect_err("401");
        let msg = err.to_string();
        assert!(
            msg.contains("401") && msg.contains("Authentication Fails"),
            "{msg}"
        );
        assert!(msg.contains("DeepSeek API key"), "{msg}");
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn retries_only_transient_statuses() {
        let attempts = |code: u16| max_attempts(StatusCode::from_u16(code).unwrap());
        assert_eq!(attempts(429), 3);
        assert_eq!(attempts(408), 3);
        assert_eq!(attempts(500), 2);
        assert_eq!(attempts(504), 2);
        // Retrying these would just spend another request on the same answer.
        for code in [400, 401, 402, 403, 404, 413] {
            assert_eq!(attempts(code), 1, "{code}");
        }
    }

    #[test]
    fn errors_carry_the_api_message_and_an_actionable_hint() {
        let body = r#"{"error":{"message":"Insufficient Balance","type":"invalid_request_error","param":null,"code":"invalid_request_error"}}"#;
        let msg = http_error("fetch", StatusCode::PAYMENT_REQUIRED, body).to_string();
        assert!(msg.contains("DeepSeek fetch HTTP 402"), "{msg}");
        assert!(msg.contains("Insufficient Balance"), "{msg}");
        assert!(msg.contains("top up"), "{msg}");

        // A non-JSON body (gateway page) is shown as-is rather than swallowed.
        let msg = http_error(
            "search",
            StatusCode::BAD_GATEWAY,
            "<html>bad gateway</html>",
        )
        .to_string();
        assert!(msg.contains("bad gateway"), "{msg}");

        // Chinese error text must not panic the truncation.
        let body = format!(r#"{{"error":{{"message":"{}"}}}}"#, "错".repeat(500));
        let msg = http_error("fetch", StatusCode::FORBIDDEN, &body).to_string();
        assert!(msg.contains('…'), "{msg}");
    }

    #[tokio::test]
    async fn custom_endpoint_sends_no_auth_header_without_a_key_and_names_itself_in_errors() {
        let (base, seen) = serve_sequence(vec![(200, r#"{"ok":true}"#)]);
        let body = post_json_custom(
            &format!("{base}/fetch"),
            "custom fetch service",
            "  ",
            &serde_json::json!({"url": "https://example.com"}),
            Duration::from_secs(5),
        )
        .await
        .expect("200");
        assert_eq!(body, r#"{"ok":true}"#);
        let request = seen.lock().unwrap()[0].to_ascii_lowercase();
        assert!(request.starts_with("post /fetch "), "{request}");
        assert!(!request.contains("authorization"), "{request}");

        let (base, seen) =
            serve_sequence(vec![(401, r#"{"error":{"message":"nope"}}"#), (200, "{}")]);
        let err = post_json_custom(
            &base,
            "custom fetch service",
            "k",
            &serde_json::json!({}),
            Duration::from_secs(5),
        )
        .await
        .expect_err("401");
        let msg = err.to_string();
        assert!(
            msg.contains("custom fetch service HTTP 401") && msg.contains("nope"),
            "{msg}"
        );
        assert!(!msg.contains("DeepSeek"), "{msg}");
        assert!(seen.lock().unwrap()[0]
            .to_ascii_lowercase()
            .contains("authorization: bearer k"));
        assert_eq!(seen.lock().unwrap().len(), 1, "401 is not retried");
    }

    #[test]
    fn error_detail_reads_openai_and_tavily_error_bodies() {
        assert_eq!(
            error_detail(r#"{"error":{"message":"no funds"}}"#),
            "no funds"
        );
        assert_eq!(
            error_detail(r#"{"detail":{"error":"Unauthorized: missing or invalid API key."}}"#),
            "Unauthorized: missing or invalid API key."
        );
        assert_eq!(
            error_detail(r#"{"detail":"Too many requests"}"#),
            "Too many requests"
        );
        assert_eq!(error_detail("<html>502</html>"), "<html>502</html>");
    }

    #[test]
    fn endpoint_urls_are_validated_with_the_callers_wording() {
        assert_eq!(
            validate_endpoint_url(" https://x.example/f?a=1 ", "custom fetch API").unwrap(),
            "https://x.example/f?a=1"
        );
        for (bad, needle) in [
            ("", "not configured"),
            ("nope", "invalid"),
            ("ftp://x.example/f", "http:// or https://"),
            ("https://u:p@x.example/f", "must not embed"),
        ] {
            let msg = validate_endpoint_url(bad, "custom fetch API")
                .unwrap_err()
                .to_string();
            assert!(msg.contains("custom fetch API URL"), "{bad:?}: {msg}");
            assert!(msg.contains(needle), "{bad:?}: {msg}");
        }
    }
}
