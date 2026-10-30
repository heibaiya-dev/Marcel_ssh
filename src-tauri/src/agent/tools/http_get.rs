//! `http_get` — fetch the full content of one or more web pages.
//!
//! Backend follows settings → experimentalSettings.httpFetchMode:
//! - `browser`: local headless Chrome/Edge via CDP (rendered DOM)
//! - `html`: bare HTTP GET via reqwest
//! - `custom`: the user's own fetch service, speaking either DeepSeek's or
//!   Tavily's wire format (their choice); internal addresses stay local unless
//!   the user opted in.
//! - `deepseek` / `tavily`: DeepSeek's (`/v1/tools/fetch`) or Tavily's (`/extract`)
//!   server-side fetch, which return Markdown directly. Only **public** URLs are
//!   sent to them; private/LAN/loopback addresses and URLs carrying credentials
//!   are read locally (see [`is_private_target`]), and a failure degrades to a
//!   local HTTP GET with the reason stated in the result.
//!
//! Supports batch fetching: pass a single `url` or an array of `urls`.
//! When multiple URLs are provided, they are fetched concurrently (HTTP)
//! or sequentially in one browser session (browser mode).
//!
//! Typical workflow:
//!   1. Call `web_search` to find relevant URLs
//!   2. Call `http_get` with multiple URLs at once to read them all

use async_trait::async_trait;
use futures::future::join_all;
use reqwest::header::CONTENT_TYPE;
use reqwest::Client;
use scraper::{Html, Selector};
use serde::Deserialize;
use serde_json::json;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::time::Duration;
use tauri::Manager;

use crate::agent::risk::Disposition;
use crate::agent::tools::browser_cdp;
use crate::agent::tools::deepseek_api;
use crate::agent::tools::web_result::{
    is_blank_content, is_bot_block, truncate_error_body, FallbackNote, WebBackend,
};
use crate::agent::tools::{truncate_output, AgentTool, ToolContext, ToolOutput};
use crate::config::keychain::{self, SecretSlot};
use crate::config::settings::{
    CustomFetchApi, FetchApiProtocol, HttpFetchMode, WebSearchApiProvider,
};
use crate::error::AppError;

const MAX_OUTPUT_BYTES: usize = 24_000;
const TIMEOUT_SECS: u64 = 20;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OutputFormat {
    Markdown,
    Text,
}

struct FetchedPage {
    requested_url: String,
    final_url: String,
    /// Real HTTP status. `None` in browser mode when the navigation produced no
    /// HTTP response at all (blocked, refused, DNS failure, error interstitial);
    /// inventing a `200` here is what once made a verification page look fine.
    status: Option<u16>,
    status_text: String,
    content_type: String,
    title: Option<String>,
    content: String,
    source_bytes: usize,
    markdown_bytes: usize,
    redirected: bool,
    http_error: bool,
    /// Backend that actually served this page.
    provider: &'static str,
    /// Set when the page is a bot-verification interstitial rather than the
    /// document that was requested.
    challenge: Option<&'static str>,
    /// Whether the converted content carries nothing a reader could use.
    blank_content: bool,
}

struct PageChunk {
    content: String,
    offset: usize,
    chunk_size: usize,
    next_offset: Option<usize>,
    total_bytes: usize,
    truncated: bool,
}

pub struct HttpGetTool;
impl HttpGetTool {
    pub fn new() -> Self {
        Self
    }
}
impl Default for HttpGetTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AgentTool for HttpGetTool {
    fn name(&self) -> &str {
        "http_get"
    }

    fn description(&self) -> &str {
        "Fetch the full content of one or more web pages. \
         Pass a single `url` OR an array of `urls` to fetch multiple pages \
         (HTTP mode is concurrent; browser mode reuses one local browser session). \
         Use this to read detailed content from URLs returned by the `web_search` tool. \
         Backend follows Settings → 网页获取方式: browser uses local Chrome/Edge, \
         html uses bare HTTP GET, deepseek / tavily have DeepSeek's / Tavily's servers fetch public pages, custom \
         uses the user's own fetch service (private/LAN/localhost addresses are fetched locally unless \
         the user opted in for their own service). \
         Requests are sent from this computer (where Marcel SSH runs), not from the \
         remote server; in deepseek/tavily/custom mode pages are fetched by that remote service instead. \
         IMPORTANT: When you need to read multiple pages, ALWAYS use the `urls` array \
         instead of calling this tool repeatedly. Returns readable Markdown by default, \
         preserving headings, lists, code blocks, tables, links, and basic HTTP metadata. \
         For long pages, use `offset` and `chunk_size` with a single `url` to continue reading."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "A single URL to fetch (use this OR urls, not both)"
                },
                "urls": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "An array of URLs to fetch concurrently (up to 5 at a time)"
                },
                "max_length": {
                    "type": "integer",
                    "description": "Backward-compatible alias for chunk_size in bytes (default: 24000, max: 48000)",
                    "default": 24000
                },
                "chunk_size": {
                    "type": "integer",
                    "description": "Maximum returned content chunk size in bytes (default: 24000, max: 48000)",
                    "default": 24000
                },
                "offset": {
                    "type": "integer",
                    "description": "Byte offset into the converted Markdown/text content. Only supported with a single url.",
                    "default": 0
                },
                "format": {
                    "type": "string",
                    "enum": ["markdown", "text"],
                    "description": "Output format for HTML pages (default: markdown; the deepseek backend always returns markdown, tavily/custom services may honour text)",
                    "default": "markdown"
                },
                "include_metadata": {
                    "type": "boolean",
                    "description": "Include HTTP status, content type, final URL, title, and chunk info in the textual output (default: true)",
                    "default": true
                }
            }
        })
    }

    fn disposition(&self) -> Disposition {
        Disposition::Allow
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput, AppError> {
        let chunk_size = params
            .get("chunk_size")
            .or_else(|| params.get("max_length"))
            .and_then(|v| v.as_u64())
            .unwrap_or(MAX_OUTPUT_BYTES as u64)
            .clamp(2000, 48000) as usize;
        let offset = params.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let include_metadata = params
            .get("include_metadata")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let format = parse_output_format(params.get("format").and_then(|v| v.as_str()));

        // Determine if single or batch mode
        let single_url = params.get("url").and_then(|v| v.as_str()).map(str::trim);
        let url_array = params.get("urls").and_then(|v| v.as_array());

        let urls_to_fetch: Vec<&str> = match (single_url, url_array) {
            (Some(url), _) if !url.is_empty() => vec![url],
            (_, Some(arr)) => arr
                .iter()
                .filter_map(|v| v.as_str().map(str::trim))
                .filter(|u| !u.is_empty())
                .collect(),
            _ => {
                return Ok(ToolOutput::fail(
                    "http_get",
                    "missing 'url' or 'urls' parameter",
                ));
            }
        };

        if urls_to_fetch.is_empty() {
            return Ok(ToolOutput::fail("http_get", "no valid URLs provided"));
        }

        if urls_to_fetch.len() > 1 && offset > 0 {
            return Ok(ToolOutput::fail(
                "http_get",
                "offset pagination is only supported with a single 'url', not 'urls'",
            ));
        }

        // Validate all URLs
        for url in &urls_to_fetch {
            if !url.starts_with("http://") && !url.starts_with("https://") {
                return Ok(ToolOutput::fail(
                    "http_get",
                    format!("invalid URL '{}': must start with http:// or https://", url),
                ));
            }
        }

        let mode = resolve_fetch_mode(ctx).await;
        // Batch mode treats chunk_size as a per-page limit. The final combined
        // output still has a safety cap to avoid flooding the model context.
        let combined_output_limit = chunk_size
            .saturating_mul(urls_to_fetch.len().max(1))
            .min(96_000);

        let owned: Vec<String> = urls_to_fetch.iter().map(|u| (*u).to_string()).collect();
        let batch = match mode {
            HttpFetchMode::Browser => fetch_all_browser(&owned, format).await,
            HttpFetchMode::Deepseek => fetch_all_deepseek(&owned, format).await,
            HttpFetchMode::Tavily => {
                let search_provider = resolve_search_provider(ctx).await;
                fetch_all_tavily(&owned, format, search_provider).await
            }
            HttpFetchMode::Custom => {
                let config = resolve_custom_fetch(ctx).await;
                fetch_all_custom(&owned, format, &config).await
            }
            HttpFetchMode::Html => FetchBatch {
                results: fetch_all_html(&urls_to_fetch, format).await,
                fallback: None,
            },
        };
        let provider = batch.provider();
        let FetchBatch { results, fallback } = batch;

        // Build combined output
        let mut sections = Vec::new();
        let mut total_source_bytes = 0;
        let mut total_content_bytes = 0;
        let mut success_count = 0;
        let mut fail_count = 0;
        let mut blocked_count = 0;
        let mut pages_metadata = Vec::new();

        for (i, (url, result)) in urls_to_fetch.iter().zip(results.iter()).enumerate() {
            let domain = extract_domain(url);
            match result {
                Ok(page) => {
                    // A verification interstitial is a failed fetch, not a page.
                    if page.http_error || page.challenge.is_some() {
                        fail_count += 1;
                        if page.challenge.is_some() {
                            blocked_count += 1;
                        }
                    } else {
                        success_count += 1;
                    }
                    total_source_bytes += page.source_bytes;
                    total_content_bytes += page.markdown_bytes;
                    let page_offset = if urls_to_fetch.len() == 1 { offset } else { 0 };
                    let page_chunk_size = chunk_size;
                    let chunk = make_chunk(&page.content, page_offset, page_chunk_size);
                    pages_metadata.push(page_metadata(page, &chunk));
                    let rendered = format_page_output(page, &chunk, include_metadata);
                    if urls_to_fetch.len() > 1 {
                        sections.push(format!(
                            "## Page {}/{}: {}\n\n{}",
                            i + 1,
                            urls_to_fetch.len(),
                            domain,
                            rendered
                        ));
                    } else {
                        sections.push(rendered);
                    }
                }
                Err(e) => {
                    fail_count += 1;
                    if urls_to_fetch.len() > 1 {
                        sections.push(format!(
                            "=== Page {}/{}: {} ===\nError: {}",
                            i + 1,
                            urls_to_fetch.len(),
                            domain,
                            e
                        ));
                    } else {
                        // Keep diagnostics in metadata even when both backends
                        // failed; the same result path serves single and batch.
                        sections.push(format!("fetch failed (provider={}): {}", provider, e));
                    }
                }
            }
        }

        let combined = sections.join("\n\n");
        let output = truncate_output(combined, combined_output_limit);

        let hint = "\n\n---\nTip: This page may contain links. Use `http_get` again with any URL to get its full content.";
        let final_output = format!("{}{}", output, hint);

        let fallback_suffix = fallback
            .as_ref()
            .map(FallbackNote::summary_suffix)
            .unwrap_or_default();

        let summary = if urls_to_fetch.len() == 1 {
            format!(
                "http_get {} ({} via {})",
                extract_domain(urls_to_fetch[0]),
                format_bytes(total_content_bytes),
                provider
            )
        } else {
            format!(
                "http_get ({} pages: {} ok, {} failed, {} via {})",
                urls_to_fetch.len(),
                success_count,
                fail_count,
                format_bytes(total_content_bytes),
                provider
            )
        };

        let mut metadata = json!({
            "provider": provider,
            // An expected file download uses HTTP without being a degradation.
            // Explicit evidence overrides legacy provider-mismatch inference.
            "degraded": fallback.is_some(),
            "requested_mode": match mode {
                HttpFetchMode::Browser => WebBackend::Browser.label(),
                HttpFetchMode::Html => WebBackend::Html.label(),
                HttpFetchMode::Deepseek => WebBackend::Deepseek.label(),
                HttpFetchMode::Tavily => WebBackend::Tavily.label(),
                HttpFetchMode::Custom => WebBackend::Custom.label(),
            },
            "urls_fetched": urls_to_fetch.len(),
            "success": success_count,
            "failed": fail_count,
            "blocked": blocked_count,
            "source_bytes": total_source_bytes,
            "content_bytes": total_content_bytes,
            "format": match format {
                OutputFormat::Markdown => "markdown",
                OutputFormat::Text => "text",
            },
            "pages": pages_metadata
        });
        if let (Some(note), Some(map)) = (&fallback, metadata.as_object_mut()) {
            map.insert("fallback".to_string(), note.to_json());
        }

        // Nothing usable came back: a failure, even though some pages may have
        // "succeeded" with an empty body.
        let nothing_usable = success_count == 0 && fail_count > 0;
        let all_blank = success_count > 0
            && pages_metadata
                .iter()
                .filter(|page| page["http_error"] != json!(true))
                .all(|page| page["blank_content"] == json!(true));

        if nothing_usable || all_blank {
            let reason = if blocked_count > 0 {
                format!(
                    "{} of {} page(s) were intercepted by a bot-verification page",
                    blocked_count,
                    urls_to_fetch.len()
                )
            } else if all_blank {
                "the page(s) loaded but contained no readable content".to_string()
            } else {
                format!("all {} page(s) failed", urls_to_fetch.len())
            };
            return Ok(ToolOutput::fail(
                summary,
                format!("{}{}\n\n{}", final_output, fallback_suffix, reason),
            )
            .with_metadata(metadata));
        }

        Ok(ToolOutput::ok(summary, final_output).with_metadata(metadata))
    }
}

/// Pages in request order, with diagnostics for actual browser failures.
struct FetchBatch {
    results: Vec<Result<FetchedPage, AppError>>,
    fallback: Option<FallbackNote>,
}

impl FetchBatch {
    fn provider(&self) -> &'static str {
        let mut providers = self
            .results
            .iter()
            .filter_map(|r| r.as_ref().ok().map(|p| p.provider));
        // If no response came back, every browser failure has already been
        // attempted over HTTP. Report that last attempted transport.
        let first = providers.next().unwrap_or(WebBackend::Html.label());
        if providers.any(|provider| provider != first) {
            WebBackend::Mixed.label()
        } else {
            first
        }
    }
}

/// Cap on one browser pass over a batch.
///
/// Per-step timeouts inside `browser_cdp` bound a healthy session, but a wedged
/// Chromium could otherwise hold the tool through every step in sequence. The
/// cap guarantees the fallback gets a turn in bounded time.
const BROWSER_BATCH_BUDGET: Duration = Duration::from_secs(75);
const BROWSER_SINGLE_BUDGET: Duration = Duration::from_secs(45);
/// Cap on the fallback pass; each HTTP request already has its own timeout.
const HTML_BATCH_BUDGET: Duration = Duration::from_secs(45);

/// Fetch a batch over plain HTTP under one overall cap, so a slow site cannot
/// hold the tool past its budget. Per-request timeouts still apply underneath.
async fn fetch_all_html(urls: &[&str], format: OutputFormat) -> Vec<Result<FetchedPage, AppError>> {
    let fetches: Vec<_> = urls.iter().map(|u| fetch_page_http(u, format)).collect();
    match tokio::time::timeout(HTML_BATCH_BUDGET, join_all(fetches)).await {
        Ok(results) => results,
        Err(_) => urls
            .iter()
            .map(|u| {
                Err(AppError::Agent(format!(
                    "HTTP fetch exceeded its {:.0}s budget for {}",
                    HTML_BATCH_BUDGET.as_secs_f32(),
                    extract_domain(u)
                )))
            })
            .collect(),
    }
}

/// Drive the browser backend, then decide whether a fallback is warranted.
async fn fetch_all_browser(urls: &[String], format: OutputFormat) -> FetchBatch {
    let budget = if urls.len() == 1 {
        BROWSER_SINGLE_BUDGET
    } else {
        BROWSER_BATCH_BUDGET
    };

    let attempt = tokio::time::timeout(budget, browser_cdp::fetch_html_many(urls)).await;
    let raw = match attempt {
        Ok(results) => results,
        Err(_elapsed) => urls
            .iter()
            .map(|_| {
                Err(browser_cdp::CdpFailure {
                    stage: browser_cdp::CdpStage::Session,
                    message: format!(
                        "browser session exceeded its {:.0}s budget",
                        budget.as_secs_f32()
                    ),
                })
            })
            .collect(),
    };
    finish_browser_batch(urls, raw, format).await
}

/// Browser pages stay intact; only URLs without a document need an HTTP pass.
/// Download responses are expected handoffs, identified by CDP's isDownload,
/// never by an error string or a file extension. This also preserves per-page
/// HTTP error/challenge/blank classification instead of declaring a retry OK.
async fn finish_browser_batch(
    urls: &[String],
    raw: Vec<Result<browser_cdp::BrowserPage, browser_cdp::CdpFailure>>,
    format: OutputFormat,
) -> FetchBatch {
    let failures: Vec<_> = raw
        .iter()
        .filter_map(|r| r.as_ref().err())
        .filter(|failure| failure.stage != browser_cdp::CdpStage::Download)
        .collect();
    let fallback = failures.first().map(|reason| {
        FallbackNote::new(
            WebBackend::Browser.label(),
            WebBackend::Html.label(),
            format!(
                "{} of {} page(s) failed in the browser: {}",
                failures.len(),
                urls.len(),
                reason
            ),
        )
    });
    let retry_urls: Vec<_> = urls
        .iter()
        .zip(&raw)
        .filter(|(_, result)| result.is_err())
        .map(|(url, _)| url.as_str())
        .collect();
    let mut retries = fetch_all_html(&retry_urls, format).await.into_iter();
    let results = raw
        .into_iter()
        .map(|result| match result {
            Ok(page) => Ok(browser_page_to_fetched(page, format)),
            Err(_) => retries.next().expect("one HTTP result per retry URL"),
        })
        .collect();
    FetchBatch { results, fallback }
}

// ── Server-side fetch (DeepSeek / custom service) ───────────────────────────
// https://api-docs.deepseek.com/zh-cn/guides/search  (POST /v1/tools/fetch)
//
// The page is fetched by someone else's servers, not by this computer. That changes
// two things the other backends never had to think about:
//
// - **What may leave the machine.** A URL handed to a third party is disclosed to
//   it. Private/LAN/loopback addresses (which a remote service could not reach
//   anyway, and which name internal hosts) and URLs with embedded credentials are
//   read locally, never sent. See [`is_private_target`]. The one exception is the
//   user's own custom service with `send_private` switched on (a fetcher living on
//   the same LAN); credentials in a URL still never leave.
// - **What a failure means.** A datacenter IP is blocked more often than a home
//   one, and the account can run out of balance or rate limit, so any remote
//   failure — including a verification page or an empty body — degrades to a
//   local HTTP GET with the reason in the result, the same contract as the
//   browser backend.
//
// There are two wire formats, and the custom service lets the user pick either:
//
// - DeepSeek: `{"url"}` in, `{"title","content"}` out.
// - Tavily Extract (https://docs.tavily.com/documentation/api-reference/endpoint/extract):
//   `{"urls":[..],"format"}` in, `{"results":[{"url","raw_content"}],
//   "failed_results":[{"url","error"}]}` out. Pages still go one request each — the
//   response order is not guaranteed, and a per-page request keeps the per-page
//   budget, retry and local-fallback bookkeeping identical for every service.
//
// Both end in `remote_page`, so what a page looks like afterwards does not depend on
// which service produced it.

/// Pages in flight at once. Fetch is limited to 5 QPS per account (plus a
/// per-target-domain limit); a wider fan-out only trades pages for 429s.
const REMOTE_FETCH_CONCURRENCY: usize = 4;
/// Timeout of one request; the 429/5xx retry in `deepseek_api` sits underneath.
const REMOTE_FETCH_TIMEOUT: Duration = Duration::from_secs(30);
/// Cap per page including retries, so one stuck page cannot hold the batch.
const REMOTE_PAGE_BUDGET: Duration = Duration::from_secs(50);

#[derive(Debug, Deserialize)]
struct RemoteFetchResponse {
    title: Option<String>,
    content: Option<String>,
}

/// Tavily's page-extraction endpoint (the "Tavily" mode; "custom" brings its own address).
const TAVILY_EXTRACT_URL: &str = "https://api.tavily.com/extract";

/// The service the remote part of a batch goes to, with what it needs to be called.
struct RemoteFetcher {
    backend: WebBackend,
    /// How requests are shaped and responses read.
    protocol: FetchApiProtocol,
    /// `None` = DeepSeek's official endpoint; `Some` = Tavily's or the user's validated address.
    endpoint: Option<String>,
    /// May be empty for a custom service (then no `Authorization` header is sent).
    api_key: String,
    /// What the model asked for; only the Tavily format can carry it.
    format: OutputFormat,
}

impl RemoteFetcher {
    /// How request errors refer to the service.
    fn name(&self) -> &'static str {
        match self.backend {
            WebBackend::Custom => "custom fetch service",
            WebBackend::Tavily => "Tavily extract",
            _ => "DeepSeek fetch",
        }
    }
}

/// How the fallback note and parse errors refer to the service that served (or failed) a page.
fn service_name(backend: WebBackend) -> &'static str {
    match backend {
        WebBackend::Custom => "the custom fetch service",
        WebBackend::Tavily => "Tavily",
        _ => "DeepSeek",
    }
}

/// Fetch a batch through DeepSeek, then read locally whatever never left the
/// machine or that DeepSeek could not deliver.
async fn fetch_all_deepseek(urls: &[String], format: OutputFormat) -> FetchBatch {
    let remote: Vec<usize> = (0..urls.len())
        .filter(|&i| !is_private_target(&urls[i]))
        .collect();
    let fetcher = deepseek_api::load_api_key()
        .map(|api_key| RemoteFetcher {
            backend: WebBackend::Deepseek,
            protocol: FetchApiProtocol::Deepseek,
            endpoint: None,
            api_key,
            format,
        })
        .map_err(|e| e.to_string());
    fetch_all_remote(urls, format, WebBackend::Deepseek, remote, fetcher).await
}

/// Same, through Tavily Extract. Public URLs only; everything else is read locally.
async fn fetch_all_tavily(
    urls: &[String],
    format: OutputFormat,
    search_provider: WebSearchApiProvider,
) -> FetchBatch {
    let remote: Vec<usize> = (0..urls.len())
        .filter(|&i| !is_private_target(&urls[i]))
        .collect();
    let fetcher = tavily_fetcher(format, search_provider).map_err(|e| e.to_string());
    fetch_all_remote(urls, format, WebBackend::Tavily, remote, fetcher).await
}

fn tavily_fetcher(
    format: OutputFormat,
    search_provider: WebSearchApiProvider,
) -> Result<RemoteFetcher, AppError> {
    let own = keychain::get(&SecretSlot::TavilyFetchApiKey)?;
    // Only worth reading when it could be used; never touch the shared slot for Brave.
    let shared = if search_provider == WebSearchApiProvider::Tavily {
        keychain::get(&SecretSlot::WebSearchApiKey)?
    } else {
        None
    };
    let api_key = pick_tavily_fetch_key(own, shared, search_provider).ok_or_else(|| {
        AppError::Agent(
            "Tavily API key is not configured for page fetch; set it in Settings → Agent tools"
                .into(),
        )
    })?;
    Ok(RemoteFetcher {
        backend: WebBackend::Tavily,
        protocol: FetchApiProtocol::Tavily,
        endpoint: Some(TAVILY_EXTRACT_URL.to_string()),
        api_key,
        format,
    })
}

/// The key Tavily Extract is called with: its own slot wins; otherwise the Tavily
/// *search* key, but only while Tavily is the search provider. The shared search slot
/// holds whichever vendor's key was saved last, so with Brave selected it is Brave's key
/// and must never be sent to Tavily.
fn pick_tavily_fetch_key(
    own: Option<String>,
    shared_search: Option<String>,
    search_provider: WebSearchApiProvider,
) -> Option<String> {
    let usable = |key: Option<String>| key.map(|k| k.trim().to_string()).filter(|k| !k.is_empty());
    usable(own).or_else(|| {
        if search_provider == WebSearchApiProvider::Tavily {
            usable(shared_search)
        } else {
            None
        }
    })
}

/// Same, through the user's own service. A bad address or an unreadable key is a
/// failure of the remote part (stated in the result, pages read locally instead),
/// not of the tool call.
async fn fetch_all_custom(
    urls: &[String],
    format: OutputFormat,
    config: &CustomFetchApi,
) -> FetchBatch {
    let remote: Vec<usize> = (0..urls.len())
        .filter(|&i| !must_stay_local(&urls[i], config.send_private))
        .collect();
    let fetcher = custom_fetcher(config, format).map_err(|e| e.to_string());
    fetch_all_remote(urls, format, WebBackend::Custom, remote, fetcher).await
}

fn custom_fetcher(
    config: &CustomFetchApi,
    format: OutputFormat,
) -> Result<RemoteFetcher, AppError> {
    let endpoint = deepseek_api::validate_endpoint_url(&config.url, "custom fetch API")?;
    // The key is optional: self-hosted services are often unauthenticated.
    let api_key = keychain::get(&SecretSlot::CustomFetchApiKey)?
        .map(|key| key.trim().to_string())
        .unwrap_or_default();
    Ok(RemoteFetcher {
        backend: WebBackend::Custom,
        protocol: config.protocol,
        endpoint: Some(endpoint),
        api_key,
        format,
    })
}

async fn fetch_all_remote(
    urls: &[String],
    format: OutputFormat,
    backend: WebBackend,
    remote: Vec<usize>,
    fetcher: Result<RemoteFetcher, String>,
) -> FetchBatch {
    let mut outcomes: Vec<Option<Result<FetchedPage, String>>> =
        (0..urls.len()).map(|_| None).collect();

    if !remote.is_empty() {
        let fetched = match fetcher {
            Ok(fetcher) => {
                let targets: Vec<&str> = remote.iter().map(|&i| urls[i].as_str()).collect();
                fetch_many_remote(&fetcher, targets).await
            }
            Err(reason) => remote.iter().map(|_| Err(reason.clone())).collect(),
        };
        for (&i, outcome) in remote.iter().zip(fetched) {
            outcomes[i] = Some(outcome);
        }
    }
    finish_remote_batch(urls, outcomes, format, backend).await
}

/// Results in request order. Pages go out in waves of [`REMOTE_FETCH_CONCURRENCY`]
/// rather than all at once; a batch is normally at most a handful of URLs.
async fn fetch_many_remote(
    fetcher: &RemoteFetcher,
    urls: Vec<&str>,
) -> Vec<Result<FetchedPage, String>> {
    let mut results = Vec::with_capacity(urls.len());
    for wave in urls.chunks(REMOTE_FETCH_CONCURRENCY) {
        let fetches = wave
            .iter()
            .map(|url| fetch_page_remote_budgeted(fetcher, url));
        results.extend(join_all(fetches).await);
    }
    results
}

async fn fetch_page_remote_budgeted(
    fetcher: &RemoteFetcher,
    url: &str,
) -> Result<FetchedPage, String> {
    match tokio::time::timeout(REMOTE_PAGE_BUDGET, fetch_page_remote(fetcher, url)).await {
        Ok(result) => result.map_err(|e| e.to_string()),
        Err(_elapsed) => Err(format!(
            "{} exceeded its {:.0}s budget for {}",
            fetcher.name(),
            REMOTE_PAGE_BUDGET.as_secs_f32(),
            extract_domain(url)
        )),
    }
}

async fn fetch_page_remote(fetcher: &RemoteFetcher, url: &str) -> Result<FetchedPage, AppError> {
    let payload = match fetcher.protocol {
        FetchApiProtocol::Deepseek => json!({ "url": url }),
        FetchApiProtocol::Tavily => json!({
            "urls": [url],
            "format": match fetcher.format {
                OutputFormat::Markdown => "markdown",
                OutputFormat::Text => "text",
            },
            // `advanced` costs twice the credits and more latency; a failed page falls
            // back to a local read anyway.
            "extract_depth": "basic",
        }),
    };
    let body = match &fetcher.endpoint {
        None => {
            deepseek_api::post_json("fetch", &fetcher.api_key, &payload, REMOTE_FETCH_TIMEOUT)
                .await?
        }
        Some(endpoint) => {
            deepseek_api::post_json_custom(
                endpoint,
                fetcher.name(),
                &fetcher.api_key,
                &payload,
                REMOTE_FETCH_TIMEOUT,
            )
            .await?
        }
    };
    let page = match fetcher.protocol {
        FetchApiProtocol::Deepseek => parse_remote_page(url, &body, fetcher.backend)?,
        FetchApiProtocol::Tavily => parse_tavily_page(url, &body, fetcher.backend, fetcher.format)?,
    };
    // A verification page or an empty body is a failed fetch, not a page: reporting
    // it as success would hide the local HTTP GET that may well get through.
    if let Some(vendor) = page.challenge {
        return Err(AppError::Agent(format!(
            "{} received a bot-verification page ({vendor}) instead of the content",
            fetcher.name()
        )));
    }
    if page.blank_content {
        return Err(AppError::Agent(format!(
            "{} returned a page with no readable content",
            fetcher.name()
        )));
    }
    Ok(page)
}

/// Read a DeepSeek-format response (`{"title","content"}`).
fn parse_remote_page(url: &str, body: &str, backend: WebBackend) -> Result<FetchedPage, AppError> {
    let parsed: RemoteFetchResponse = serde_json::from_str(body).map_err(|e| {
        AppError::Agent(format!(
            "invalid {} response: {}",
            service_name(backend).trim_start_matches("the "),
            e
        ))
    })?;
    Ok(remote_page(
        url,
        parsed.title,
        parsed.content.unwrap_or_default(),
        backend,
        "text/markdown; charset=utf-8",
    ))
}

/// Tavily Extract's answer to one requested URL.
#[derive(Debug, Deserialize)]
struct TavilyExtractResponse {
    #[serde(default)]
    results: Vec<TavilyExtractResult>,
    #[serde(default)]
    failed_results: Vec<TavilyExtractFailure>,
}

#[derive(Debug, Deserialize)]
struct TavilyExtractResult {
    raw_content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TavilyExtractFailure {
    error: Option<String>,
}

/// Read a Tavily-format response for the single URL that was requested. HTTP 200 does
/// not mean the page came back: a URL Tavily could not extract is listed in
/// `failed_results`, and that reason is what the user needs to see.
fn parse_tavily_page(
    url: &str,
    body: &str,
    backend: WebBackend,
    format: OutputFormat,
) -> Result<FetchedPage, AppError> {
    let service = service_name(backend).trim_start_matches("the ");
    let parsed: TavilyExtractResponse = serde_json::from_str(body)
        .map_err(|e| AppError::Agent(format!("invalid {service} response: {e}")))?;
    let Some(result) = parsed.results.into_iter().next() else {
        let reason = parsed
            .failed_results
            .into_iter()
            .find_map(|f| f.error.filter(|e| !e.trim().is_empty()))
            .map(|e| truncate_error_body(&e))
            .unwrap_or_else(|| "no result for the requested URL".to_string());
        return Err(AppError::Agent(format!(
            "{service} could not extract {}: {reason}",
            extract_domain(url)
        )));
    };
    // Tavily's extraction carries no page title.
    Ok(remote_page(
        url,
        None,
        result.raw_content.unwrap_or_default(),
        backend,
        match format {
            OutputFormat::Markdown => "text/markdown; charset=utf-8",
            OutputFormat::Text => "text/plain; charset=utf-8",
        },
    ))
}

/// What a page looks like once any remote service has delivered it.
fn remote_page(
    url: &str,
    title: Option<String>,
    content: String,
    backend: WebBackend,
    content_type: &str,
) -> FetchedPage {
    let content = cleanup_markdown(&content);
    let title = title
        .map(|t| cleanup_whitespace(&t))
        .filter(|t| !t.is_empty());
    let markdown_bytes = content.len();
    let blank_content = is_blank_content(&content);
    let challenge = is_bot_block(&content, title.as_deref()).map(|c| c.vendor);

    FetchedPage {
        requested_url: url.to_string(),
        // The service reports neither redirects nor the page's own status code, so
        // neither is invented here: `status: None` renders as "unknown".
        final_url: url.to_string(),
        status: None,
        status_text: status_text_for(None, backend),
        content_type: content_type.to_string(),
        title,
        content,
        // What we received is already the converted Markdown; the original HTML size
        // is not reported.
        source_bytes: markdown_bytes,
        markdown_bytes,
        redirected: false,
        http_error: false,
        provider: backend.label(),
        challenge,
        blank_content,
    }
}

/// `outcomes[i]`: `None` = never sent to the remote service (kept local), `Some(Ok)` =
/// its page, `Some(Err)` = it failed. The last two are the only ones that count
/// towards the fallback note; the first is by design, not a degradation.
async fn finish_remote_batch(
    urls: &[String],
    outcomes: Vec<Option<Result<FetchedPage, String>>>,
    format: OutputFormat,
    backend: WebBackend,
) -> FetchBatch {
    let sent = outcomes.iter().flatten().count();
    let reasons: Vec<&str> = outcomes
        .iter()
        .filter_map(|o| match o {
            Some(Err(reason)) => Some(reason.as_str()),
            _ => None,
        })
        .collect();
    let service = service_name(backend);
    let fallback = reasons.first().map(|first| {
        FallbackNote::new(
            backend.label(),
            WebBackend::Html.label(),
            format!(
                "{} of {} page(s) failed on {}: {}",
                reasons.len(),
                sent,
                service,
                first
            ),
        )
    });

    let local_urls: Vec<&str> = urls
        .iter()
        .zip(&outcomes)
        .filter(|(_, outcome)| !matches!(outcome, Some(Ok(_))))
        .map(|(url, _)| url.as_str())
        .collect();
    let mut local = fetch_all_html(&local_urls, format).await.into_iter();
    let results = outcomes
        .into_iter()
        .map(|outcome| match outcome {
            Some(Ok(page)) => Ok(page),
            _ => local.next().expect("one local result per local URL"),
        })
        .collect();
    FetchBatch { results, fallback }
}

/// Whether `target` stays on this machine for the custom service. Without
/// `send_private` this is exactly [`is_private_target`]. With it, internal addresses
/// may go to the user's own service, but anything we cannot vouch for as a plain
/// web address — other schemes, unparsable text, embedded credentials — still never
/// leaves.
fn must_stay_local(target: &str, send_private: bool) -> bool {
    if !send_private {
        return is_private_target(target);
    }
    match url::Url::parse(target) {
        Ok(parsed) => {
            !matches!(parsed.scheme(), "http" | "https")
                || parsed.host().is_none()
                || !parsed.username().is_empty()
                || parsed.password().is_some()
        }
        Err(_) => true,
    }
}

/// Whether `target` must be read from this machine instead of being handed to a
/// third-party fetch service.
///
/// True for anything that is not a plain public web address: loopback, RFC 1918,
/// link-local, CGNAT (Tailscale lives in 100.64/10), IPv6 ULA/link-local, LAN-style
/// names (`nas`, `*.local`, `*.internal`, …), anything unparsable, and any URL with
/// embedded credentials. Leaning towards "local" is deliberate: the only cost of a
/// false positive is that DeepSeek's cleaner Markdown is not used for that page,
/// while a false negative discloses an internal host name to a third party.
pub(crate) fn is_private_target(target: &str) -> bool {
    let Ok(parsed) = url::Url::parse(target) else {
        return true;
    };
    if !matches!(parsed.scheme(), "http" | "https")
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return true;
    }
    match parsed.host() {
        None => true,
        Some(url::Host::Ipv4(ip)) => is_private_v4(ip),
        Some(url::Host::Ipv6(ip)) => is_private_v6(ip),
        Some(url::Host::Domain(domain)) => is_private_domain(domain),
    }
}

fn is_private_domain(domain: &str) -> bool {
    let host = domain.trim_end_matches('.').to_ascii_lowercase();
    // A single-label name ("nas", "router") only resolves on a LAN or search domain.
    if !host.contains('.') {
        return true;
    }
    [
        "localhost",
        "local",
        "internal",
        "lan",
        "home",
        "corp",
        "intranet",
        "private",
        "home.arpa",
    ]
    .iter()
    .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")))
}

fn is_private_v4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || a == 0
        // multicast and reserved space
        || a >= 224
        // CGNAT 100.64.0.0/10
        || (a == 100 && (64..128).contains(&b))
}

fn is_private_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_private_v4(v4);
    }
    let seg = ip.segments();
    // 64:ff9b::/96 (NAT64) carries an IPv4 address in its last 32 bits.
    if seg[0] == 0x64 && seg[1] == 0xff9b && seg[2..6].iter().all(|s| *s == 0) {
        let [a, b] = seg[6].to_be_bytes();
        let [c, d] = seg[7].to_be_bytes();
        return is_private_v4(Ipv4Addr::new(a, b, c, d));
    }
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        // fc00::/7 unique local, fe80::/10 link-local
        || (seg[0] & 0xfe00) == 0xfc00
        || (seg[0] & 0xffc0) == 0xfe80
}

async fn resolve_custom_fetch(ctx: &ToolContext) -> CustomFetchApi {
    match ctx.app_handle.try_state::<crate::AppState>() {
        Some(state) => state
            .settings
            .read()
            .await
            .experimental_settings
            .http_fetch_custom
            .clone(),
        None => CustomFetchApi::default(),
    }
}

/// The current search-API vendor: it decides whether the Tavily *search* key may also
/// serve Tavily page fetch (see [`pick_tavily_fetch_key`]).
async fn resolve_search_provider(ctx: &ToolContext) -> WebSearchApiProvider {
    match ctx.app_handle.try_state::<crate::AppState>() {
        Some(state) => {
            state
                .settings
                .read()
                .await
                .experimental_settings
                .web_search_api_provider
        }
        None => WebSearchApiProvider::default(),
    }
}

async fn resolve_fetch_mode(ctx: &ToolContext) -> HttpFetchMode {
    // Mobile (Android) 无法启动本地 Chrome/Edge 走 CDP：browser 设置必然失败，
    // 一律落到裸 HTTP GET（html）。DeepSeek 获取是纯 HTTP，手机上完全可用，按设置生效。
    #[cfg(mobile)]
    {
        if let Some(state) = ctx.app_handle.try_state::<crate::AppState>() {
            let settings = state.settings.read().await;
            // 纯 HTTP 的几种服务端抓取手机上都能用，按设置生效。
            let mode = settings.experimental_settings.http_fetch_mode;
            if matches!(
                mode,
                HttpFetchMode::Deepseek | HttpFetchMode::Tavily | HttpFetchMode::Custom
            ) {
                return mode;
            }
        }
        return HttpFetchMode::Html;
    }

    #[cfg(desktop)]
    {
        if let Some(state) = ctx.app_handle.try_state::<crate::AppState>() {
            let settings = state.settings.read().await;
            return settings.experimental_settings.http_fetch_mode;
        }
        HttpFetchMode::default()
    }
}

fn browser_page_to_fetched(page: browser_cdp::BrowserPage, format: OutputFormat) -> FetchedPage {
    let source_bytes = page.html.len();
    let title = page.title.clone().or_else(|| extract_title(&page.html));
    let content = {
        let readable_html = extract_readable_html(&page.html);
        match format {
            OutputFormat::Markdown => html_to_markdown(&readable_html),
            OutputFormat::Text => strip_html(&readable_html, usize::MAX),
        }
    };
    let content = cleanup_markdown(&content);
    let markdown_bytes = content.len();
    let redirected = normalize_url_for_compare(&page.requested_url)
        != normalize_url_for_compare(&page.final_url);

    // A browser only tells us the real status when the navigation produced an
    // HTTP response; anything else (blocked, refused, error interstitial) stays
    // `None` rather than being reported as a 200.
    let http_error = page.status.is_some_and(|code| !(200..300).contains(&code));
    let blank_content = is_blank_content(&content);
    // Gated on whether the page had anything to read, like the HTML path: a page
    // that rendered text is a page, whatever words happen to appear in it.
    let challenge = is_bot_block(&page.html, title.as_deref()).map(|challenge| challenge.vendor);

    FetchedPage {
        requested_url: page.requested_url,
        final_url: page.final_url,
        status: page.status,
        status_text: status_text_for(page.status, WebBackend::Browser),
        content_type: "text/html; charset=utf-8".to_string(),
        title,
        content,
        source_bytes,
        markdown_bytes,
        redirected,
        http_error,
        provider: WebBackend::Browser.label(),
        challenge,
        blank_content,
    }
}

/// Honest status text: a real code gets its canonical reason, an unknown code
/// says so, and no response at all is described as such.
fn status_text_for(status: Option<u16>, backend: WebBackend) -> String {
    match status {
        Some(200) => match backend {
            WebBackend::Browser => "OK (browser)".to_string(),
            _ => "OK".to_string(),
        },
        Some(code) => reqwest::StatusCode::from_u16(code)
            .ok()
            .and_then(|s| s.canonical_reason())
            .map(str::to_string)
            .unwrap_or_else(|| "Unknown status".to_string()),
        // DeepSeek hands back extracted Markdown, never the page's own HTTP status.
        None if backend == WebBackend::Deepseek => {
            "page status not reported (fetched by DeepSeek)".to_string()
        }
        None if backend == WebBackend::Tavily => {
            "page status not reported (fetched by Tavily)".to_string()
        }
        None if backend == WebBackend::Custom => {
            "page status not reported (fetched by the custom fetch service)".to_string()
        }
        None => "no HTTP response (browser)".to_string(),
    }
}

async fn fetch_page_http(url: &str, format: OutputFormat) -> Result<FetchedPage, AppError> {
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|e| AppError::Agent(format!("failed to create HTTP client: {}", e)))?;

    let resp = client
        .get(url)
        .header(
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
        )
        .send()
        .await
        .map_err(|e| AppError::Agent(format!("HTTP request failed: {}", e)))?;

    let status = resp.status();
    let final_url = resp.url().to_string();
    let content_type = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let body = resp
        .text()
        .await
        .map_err(|e| AppError::Agent(format!("failed to read response: {}", e)))?;
    let source_bytes = body.len();
    let title = extract_title(&body);

    let content = if is_html(&content_type, &body) {
        let readable_html = extract_readable_html(&body);
        match format {
            OutputFormat::Markdown => html_to_markdown(&readable_html),
            OutputFormat::Text => strip_html(&readable_html, usize::MAX),
        }
    } else {
        body.clone()
    };
    let content = cleanup_markdown(&content);
    let markdown_bytes = content.len();
    let redirected = normalize_url_for_compare(url) != normalize_url_for_compare(&final_url);
    let http_error = !status.is_success();
    let blank_content = is_blank_content(&content);
    // Only a page with nothing to read can be a block page. Matching markers alone
    // condemned every article that merely mentions a captcha — the page was
    // reported as "intercepted by a bot-verification page" while its own text sat
    // in the same response, and the whole fetch counted as a failure.
    let challenge = is_bot_block(&body, title.as_deref()).map(|c| c.vendor);

    Ok(FetchedPage {
        requested_url: url.to_string(),
        final_url,
        status: Some(status.as_u16()),
        status_text: status_text_for(Some(status.as_u16()), WebBackend::Html),
        content_type,
        title,
        content,
        source_bytes,
        markdown_bytes,
        redirected,
        http_error,
        provider: WebBackend::Html.label(),
        challenge,
        blank_content,
    })
}

fn parse_output_format(format: Option<&str>) -> OutputFormat {
    match format
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("text") => OutputFormat::Text,
        _ => OutputFormat::Markdown,
    }
}

fn is_html(content_type: &str, body: &str) -> bool {
    let content_type = content_type.to_ascii_lowercase();
    content_type.contains("text/html")
        || body.contains("<html")
        || body.contains("<HTML")
        || body.contains("<!DOCTYPE")
        || body.contains("<!doctype")
}

pub fn html_to_markdown(html: &str) -> String {
    html2md::parse_html(html)
}

fn extract_readable_html(html: &str) -> String {
    let cleaned = strip_noise_html(html);
    let document = Html::parse_document(&cleaned);

    for selector in readable_selectors() {
        if let Ok(selector) = Selector::parse(selector) {
            if let Some(element) = document.select(&selector).find(|e| {
                let text = cleanup_whitespace(&e.text().collect::<Vec<_>>().join(" "));
                text.len() >= 40
            }) {
                return element.inner_html();
            }
        }
    }

    if let Ok(selector) = Selector::parse("body") {
        if let Some(body) = document.select(&selector).next() {
            return body.inner_html();
        }
    }

    cleaned
}

fn readable_selectors() -> &'static [&'static str] {
    &[
        "main",
        "article",
        "[role=main]",
        "#main",
        "#content",
        "body",
    ]
}

fn strip_noise_html(html: &str) -> String {
    let mut cleaned = html.to_string();
    for tag in [
        "script", "style", "noscript", "template", "svg", "iframe", "canvas", "meta", "link",
    ] {
        cleaned = strip_html_tag(&cleaned, tag);
    }
    cleaned
}

fn strip_html_tag(html: &str, tag: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let open_prefix = format!("<{}", tag);
    let close = format!("</{}>", tag);
    let mut out = String::with_capacity(html.len());
    let mut pos = 0;

    while let Some(start_rel) = lower[pos..].find(&open_prefix) {
        let start = pos + start_rel;
        let after_open = start + open_prefix.len();
        let next = lower.as_bytes().get(after_open).copied();
        if !matches!(
            next,
            Some(b'>') | Some(b'/') | Some(b' ') | Some(b'\t') | Some(b'\r') | Some(b'\n')
        ) {
            out.push_str(&html[pos..after_open]);
            pos = after_open;
            continue;
        }

        out.push_str(&html[pos..start]);
        let Some(open_end_rel) = lower[start..].find('>') else {
            return out;
        };
        let open_end = start + open_end_rel + 1;

        if tag == "meta" || tag == "link" || lower[start..open_end].trim_end().ends_with("/>") {
            pos = open_end;
            continue;
        }

        if let Some(close_rel) = lower[open_end..].find(&close) {
            pos = open_end + close_rel + close.len();
        } else {
            pos = open_end;
        }
    }

    out.push_str(&html[pos..]);
    out
}

fn extract_title(html: &str) -> Option<String> {
    let document = Html::parse_document(html);
    let selector = Selector::parse("title").ok()?;
    let element = document.select(&selector).next()?;
    let text = element.text().collect::<Vec<_>>().join("");
    let text = cleanup_whitespace(&text);
    (!text.is_empty()).then_some(text)
}

fn make_chunk(content: &str, offset: usize, chunk_size: usize) -> PageChunk {
    if offset >= content.len() {
        return PageChunk {
            content: String::new(),
            offset,
            chunk_size,
            next_offset: None,
            total_bytes: content.len(),
            truncated: false,
        };
    }

    let start = previous_char_boundary(content, offset);
    let requested_end = start.saturating_add(chunk_size).min(content.len());
    let end = previous_char_boundary(content, requested_end);
    let truncated = end < content.len();

    PageChunk {
        content: content[start..end].to_string(),
        offset: start,
        chunk_size,
        next_offset: truncated.then_some(end),
        total_bytes: content.len(),
        truncated,
    }
}

fn previous_char_boundary(s: &str, mut index: usize) -> usize {
    index = index.min(s.len());
    while index > 0 && !s.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn format_page_output(page: &FetchedPage, chunk: &PageChunk, include_metadata: bool) -> String {
    let mut out = String::new();
    if include_metadata {
        out.push_str(&format!("URL: {}\n", page.requested_url));
        out.push_str(&format!("Final URL: {}\n", page.final_url));
        if page.redirected {
            out.push_str("Warning: requested URL redirected; content is from Final URL.\n");
        }
        if page.http_error {
            out.push_str(
                "Warning: HTTP status is not successful; showing returned error page content.\n",
            );
        }
        match page.status {
            Some(code) => out.push_str(&format!("Status: {} {}\n", code, page.status_text)),
            None => out.push_str(&format!("Status: unknown — {}\n", page.status_text)),
        }
        if !page.content_type.is_empty() {
            out.push_str(&format!("Content-Type: {}\n", page.content_type));
        }
        if let Some(title) = &page.title {
            out.push_str(&format!("Title: {}\n", title));
        }
        out.push_str(&format!(
            "Source-Length: {}\n",
            format_bytes(page.source_bytes)
        ));
        out.push_str(&format!(
            "Chunk: offset {}, {} bytes of {}\n\n---\n\n",
            chunk.offset, chunk.chunk_size, chunk.total_bytes
        ));
    }

    // A verification interstitial is the single most misleading thing this tool
    // can return, so it is called out before the (useless) body.
    if let Some(vendor) = page.challenge {
        out.push_str(&format!(
            "⚠ BLOCKED: this URL returned a bot-verification page ({}) instead of the \
             requested content. The page below is the verification interstitial, not the \
             article. Retrying may not help; try another source or a different URL.\n\n",
            vendor
        ));
    }

    if chunk.content.is_empty() {
        if page.blank_content && page.source_bytes > 0 {
            // Previously this was a neutral note, which read like a paging quirk
            // even when the real cause was a blocked or JS-only page.
            out.push_str(&format!(
                "[no readable content: the page returned {} of HTML but converted to \
                 nothing — it may require JavaScript, or be a placeholder/empty document]",
                format_bytes(page.source_bytes)
            ));
        } else {
            out.push_str("[empty chunk: offset is at or beyond the converted content length]");
        }
    } else {
        out.push_str(&chunk.content);
    }

    if let Some(next_offset) = chunk.next_offset {
        out.push_str(&format!(
            "\n\n---\n[chunk truncated: next offset {}; converted content {} bytes]",
            next_offset, chunk.total_bytes
        ));
    }

    out
}

fn page_metadata(page: &FetchedPage, chunk: &PageChunk) -> serde_json::Value {
    json!({
        "url": &page.requested_url,
        "final_url": &page.final_url,
        "status": page.status,
        "status_text": &page.status_text,
        "content_type": &page.content_type,
        "title": &page.title,
        "provider": page.provider,
        "challenge": page.challenge,
        "blank_content": page.blank_content,
        "source_bytes": page.source_bytes,
        "markdown_bytes": page.markdown_bytes,
        "redirected": page.redirected,
        "http_error": page.http_error,
        "offset": chunk.offset,
        "chunk_size": chunk.chunk_size,
        "next_offset": chunk.next_offset,
        "truncated": chunk.truncated
    })
}

/// Strip HTML tags and decode entities, preserving structure with newlines
fn strip_html(html: &str, max_len: usize) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut last_was_newline = false;

    let mut chars = html.chars().peekable();

    while let Some(c) = chars.next() {
        if out.len() >= max_len {
            break;
        }

        if c == '<' {
            in_tag = true;
            let rest: String = chars.clone().take(20).collect();
            if rest.starts_with('/')
                && (rest.contains("p>")
                    || rest.contains("div>")
                    || rest.contains("h1>")
                    || rest.contains("h2>")
                    || rest.contains("h3>")
                    || rest.contains("h4>")
                    || rest.contains("li>")
                    || rest.contains("br>"))
                && !last_was_newline
            {
                out.push('\n');
                last_was_newline = true;
            }
            continue;
        }

        if c == '>' {
            in_tag = false;
            continue;
        }

        if !in_tag {
            if c == '\n' || c == '\r' {
                if !last_was_newline {
                    out.push('\n');
                    last_was_newline = true;
                }
            } else {
                if last_was_newline && c == ' ' {
                    continue;
                }
                out.push(c);
                last_was_newline = false;
            }
        }
    }

    let out = decode_html_entities(&out);

    // Clean up excessive newlines
    let out = {
        let mut result = String::with_capacity(out.len());
        let mut consecutive_newlines = 0;
        for c in out.chars() {
            if c == '\n' {
                consecutive_newlines += 1;
                if consecutive_newlines <= 2 {
                    result.push(c);
                }
            } else {
                consecutive_newlines = 0;
                result.push(c);
            }
        }
        result
    };

    out.trim().to_string()
}

fn decode_html_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&#39;", "'")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&nbsp;", " ")
        .replace("&mdash;", "—")
        .replace("&ndash;", "–")
}

fn cleanup_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn cleanup_markdown(markdown: &str) -> String {
    let mut result = String::with_capacity(markdown.len());
    let mut consecutive_blank_lines = 0;

    for line in markdown.lines() {
        if line.trim().is_empty() {
            consecutive_blank_lines += 1;
            if consecutive_blank_lines <= 2 {
                result.push('\n');
            }
            continue;
        }

        consecutive_blank_lines = 0;
        result.push_str(line.trim_end());
        result.push('\n');
    }

    result.trim().to_string()
}

fn extract_domain(url: &str) -> String {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or(url)
        .to_string()
}

fn format_bytes(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn normalize_url_for_compare(url: &str) -> String {
    url.trim().trim_end_matches('/').to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serve one HTTP response on a loopback port, returning its URL and the
    /// server thread. Keeps the live-path tests from repeating the socket dance.
    fn serve_once(
        status: &str,
        content_type: &str,
        body: &str,
    ) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::thread;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let response = format!(
            "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            status,
            content_type,
            body.len(),
            body
        );
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(response.as_bytes());
        });
        (format!("http://{}/page", addr), handle)
    }

    /// A successful plain-HTTP page, for tests that only care about one field.
    fn fetched(content: &str) -> FetchedPage {
        FetchedPage {
            requested_url: "https://example.com/".to_string(),
            final_url: "https://example.com/".to_string(),
            status: Some(200),
            status_text: status_text_for(Some(200), WebBackend::Html),
            content_type: "text/html; charset=utf-8".to_string(),
            title: None,
            content: content.to_string(),
            source_bytes: content.len(),
            markdown_bytes: content.len(),
            redirected: false,
            http_error: false,
            provider: WebBackend::Html.label(),
            challenge: None,
            blank_content: is_blank_content(content),
        }
    }

    /// A browser page result, for tests that care about status/title only.
    fn browser_page(url: &str, html: &str, status: Option<u16>) -> browser_cdp::BrowserPage {
        browser_cdp::BrowserPage {
            requested_url: url.to_string(),
            final_url: url.to_string(),
            title: None,
            html: html.to_string(),
            status,
        }
    }

    #[test]
    fn extract_domain_works() {
        assert_eq!(extract_domain("https://example.com/page"), "example.com");
        assert_eq!(extract_domain("http://foo.bar/baz/qux"), "foo.bar");
    }

    #[test]
    fn format_bytes_formats() {
        assert!(format_bytes(500).contains("B"));
        assert!(format_bytes(2048).contains("KB"));
    }

    #[test]
    fn strip_html_removes_tags() {
        let html = "<html><body><p>Hello</p><div>World</div></body></html>";
        let text = strip_html(html, 1000);
        assert!(text.contains("Hello"));
        assert!(text.contains("World"));
        assert!(!text.contains("<"));
    }

    #[test]
    fn strip_html_decodes_entities() {
        let html = "Tom &amp; Jerry &quot;rock&quot;";
        assert!(strip_html(html, 1000).contains("Tom & Jerry"));
    }

    #[test]
    fn strip_html_respects_max_len() {
        let html = "<p>".repeat(10000);
        let text = strip_html(&html, 100);
        assert!(text.len() <= 100);
    }

    #[test]
    fn html_to_markdown_preserves_basic_structure() {
        let html = r#"
            <html><body>
                <h1>API Reference</h1>
                <h2>Fields</h2>
                <ul><li>id</li><li>name</li></ul>
                <pre><code>curl https://example.com</code></pre>
                <table><tr><th>Name</th><th>Type</th></tr><tr><td>id</td><td>string</td></tr></table>
            </body></html>
        "#;

        let markdown = cleanup_markdown(&html_to_markdown(html));

        assert!(markdown.contains("API Reference"), "{markdown}");
        assert!(markdown.contains("=========="), "{markdown}");
        assert!(markdown.contains("Fields"), "{markdown}");
        assert!(markdown.contains("----------"), "{markdown}");
        assert!(markdown.contains("id"), "{markdown}");
        assert!(markdown.contains("name"), "{markdown}");
        assert!(markdown.contains("curl https://example.com"), "{markdown}");
        assert!(markdown.contains("Name"), "{markdown}");
        assert!(markdown.contains("Type"), "{markdown}");
    }

    #[test]
    fn extract_title_decodes_entities_and_whitespace() {
        let html = "<html><head><title>Tom &amp; Jerry\n Docs</title></head></html>";
        assert_eq!(extract_title(html), Some("Tom & Jerry Docs".to_string()));
    }

    #[test]
    fn make_chunk_returns_next_offset() {
        let content = "abcdef";
        let chunk = make_chunk(content, 0, 3);

        assert_eq!(chunk.content, "abc");
        assert_eq!(chunk.next_offset, Some(3));
        assert!(chunk.truncated);
    }

    #[test]
    fn make_chunk_respects_non_ascii_boundaries() {
        let content = "αβγδε";
        let chunk = make_chunk(content, 1, 5);

        assert!(content.is_char_boundary(chunk.offset));
        assert!(chunk.content.is_char_boundary(chunk.content.len()));
    }

    #[test]
    fn format_page_output_includes_metadata_and_continuation() {
        let page = FetchedPage {
            requested_url: "https://example.com/docs".to_string(),
            final_url: "https://example.com/docs/".to_string(),
            title: Some("Docs".to_string()),
            content: "abcdef".to_string(),
            source_bytes: 100,
            markdown_bytes: 6,
            redirected: true,
            ..fetched("abcdef")
        };
        let chunk = make_chunk(&page.content, 0, 3);
        let output = format_page_output(&page, &chunk, true);

        assert!(output.contains("Final URL: https://example.com/docs/"));
        assert!(output.contains("Status: 200 OK"));
        assert!(output.contains("Content-Type: text/html; charset=utf-8"));
        assert!(output.contains("Title: Docs"));
        assert!(output.contains("requested URL redirected"));
        assert!(output.contains("next offset 3"));
    }

    #[test]
    fn format_page_output_marks_http_error_but_keeps_content() {
        let page = FetchedPage {
            requested_url: "https://example.com/missing".to_string(),
            final_url: "https://example.com/missing".to_string(),
            status: Some(404),
            status_text: status_text_for(Some(404), WebBackend::Html),
            content_type: "text/html".to_string(),
            title: Some("Not Found".to_string()),
            content: "# Not Found\n\nThe page is missing.".to_string(),
            source_bytes: 128,
            markdown_bytes: 32,
            http_error: true,
            ..fetched("")
        };
        let chunk = make_chunk(&page.content, 0, 2000);
        let output = format_page_output(&page, &chunk, true);

        assert!(output.contains("Status: 404 Not Found"), "{output}");
        assert!(output.contains("HTTP status is not successful"), "{output}");
        assert!(output.contains("The page is missing."), "{output}");
    }

    #[test]
    fn extract_readable_html_removes_script_style_and_json_noise() {
        let html = r#"
            <html>
                <head>
                    <style>.hidden { display: none; }</style>
                    <script type="application/ld+json">{"name":"Noise"}</script>
                    <script>window.digitalData = { page: "noise" };</script>
                </head>
                <body>
                    <main>
                        <h1>Product Specs</h1>
                        <table><tr><th>Name</th><th>Value</th></tr><tr><td>Battery</td><td>10 days</td></tr></table>
                        <p>This readable product specification content is long enough to be selected.</p>
                    </main>
                </body>
            </html>
        "#;

        let readable = extract_readable_html(html);
        let markdown = cleanup_markdown(&html_to_markdown(&readable));

        assert!(markdown.contains("Product Specs"), "{markdown}");
        assert!(markdown.contains("Battery"), "{markdown}");
        assert!(markdown.contains("10 days"), "{markdown}");
        assert!(!markdown.contains("window.digitalData"), "{markdown}");
        assert!(!markdown.contains("display: none"), "{markdown}");
        assert!(!markdown.contains("Noise"), "{markdown}");
    }

    #[test]
    fn extract_readable_html_prefers_main_over_navigation() {
        let html = r#"
            <html><body>
                <nav><a>Home</a><a>Products</a><a>Support</a></nav>
                <main><h1>Real Article</h1><p>This is the actual page body with enough readable text for extraction.</p></main>
            </body></html>
        "#;

        let readable = extract_readable_html(html);

        assert!(readable.contains("Real Article"), "{readable}");
        assert!(!readable.contains("Support"), "{readable}");
    }

    #[test]
    fn extract_readable_html_keeps_full_body_when_no_main_exists() {
        let html = r#"
            <html><body>
                <div class="toc">Table of contents</div>
                <section id="middle-specs">Middle section that should not become the only returned content.</section>
                <section id="end">Final section should still be present in fallback body extraction.</section>
            </body></html>
        "#;

        let readable = extract_readable_html(html);

        assert!(readable.contains("Table of contents"), "{readable}");
        assert!(readable.contains("Middle section"), "{readable}");
        assert!(readable.contains("Final section"), "{readable}");
    }

    #[test]
    fn make_chunk_uses_requested_size_per_page() {
        let content = "a".repeat(25_000);
        let chunk = make_chunk(&content, 0, 20_000);

        assert_eq!(chunk.content.len(), 20_000);
        assert_eq!(chunk.next_offset, Some(20_000));
    }

    #[test]
    fn strip_html_tag_handles_self_closing_and_paired_tags() {
        let html = r#"<html><head><meta name="x"><link href="x"><script>bad()</script></head><body>Good</body></html>"#;
        let cleaned = strip_noise_html(html);

        assert!(cleaned.contains("Good"), "{cleaned}");
        assert!(!cleaned.contains("bad()"), "{cleaned}");
        assert!(!cleaned.contains("<meta"), "{cleaned}");
        assert!(!cleaned.contains("<link"), "{cleaned}");
    }

    #[test]
    fn browser_page_to_fetched_converts_html_and_detects_redirect() {
        let html = r#"
                <html><head><title>Ignored</title></head>
                <body><main><h1>Title</h1><p>Readable body with enough text for extraction path.</p></main></body></html>
            "#;
        let page = browser_cdp::BrowserPage {
            requested_url: "https://example.com/a".into(),
            final_url: "https://example.com/b".into(),
            title: Some("Hello".into()),
            ..browser_page("https://example.com/a", html, Some(200))
        };
        let fetched = browser_page_to_fetched(page, OutputFormat::Markdown);
        assert!(fetched.redirected);
        assert_eq!(fetched.title.as_deref(), Some("Hello"));
        assert_eq!(fetched.status, Some(200));
        assert!(fetched.content.contains("Title") || fetched.content.contains("Readable"));
        assert!(!fetched.http_error);
        assert_eq!(fetched.content_type, "text/html; charset=utf-8");
        assert_eq!(fetched.provider, "browser");
    }

    #[test]
    fn browser_page_to_fetched_falls_back_to_html_title() {
        let page = browser_page(
            "https://example.com/",
            "<html><head><title>From HTML</title></head><body><main><p>Body text long enough here.</p></main></body></html>",
            Some(200),
        );
        let fetched = browser_page_to_fetched(page, OutputFormat::Text);
        assert_eq!(fetched.title.as_deref(), Some("From HTML"));
        assert!(!fetched.redirected);
    }

    /// The regression that produced "Status: 200 OK (browser)" on a blocked page:
    /// a browser navigation with no HTTP response must not be reported as 200.
    #[test]
    fn browser_page_without_an_http_response_reports_an_unknown_status() {
        let html = r#"<html><head><title>百度安全验证</title></head>
            <body><div>请完成安全验证</div></body></html>"#;
        let fetched = browser_page_to_fetched(
            browser_page("https://baike.baidu.com/item/绝区零", html, None),
            OutputFormat::Markdown,
        );

        assert_eq!(
            fetched.status, None,
            "no HTTP response must not become a fabricated 200"
        );
        assert_eq!(fetched.status_text, "no HTTP response (browser)");
        assert_eq!(fetched.challenge, Some("百度安全验证"));

        let chunk = make_chunk(&fetched.content, 0, 20_000);
        let rendered = format_page_output(&fetched, &chunk, true);
        assert!(rendered.contains("Status: unknown"), "{rendered}");
        assert!(!rendered.contains("Status: 200"), "{rendered}");
        assert!(rendered.contains("BLOCKED"), "{rendered}");
        assert!(
            rendered.contains("百度安全验证"),
            "the interstitial must name itself: {rendered}"
        );

        let meta = page_metadata(&fetched, &chunk);
        assert!(meta["status"].is_null(), "{meta}");
        assert_eq!(meta["challenge"], "百度安全验证");
    }

    /// A non-2xx browser response must be flagged instead of reporting success.
    #[test]
    fn browser_page_with_an_error_status_is_marked_as_an_http_error() {
        let fetched = browser_page_to_fetched(
            browser_page(
                "https://example.com/",
                "<html><body>gone</body></html>",
                Some(404),
            ),
            OutputFormat::Markdown,
        );
        assert!(fetched.http_error);
        assert_eq!(fetched.status, Some(404));
        assert!(
            fetched.status_text.contains("Not Found"),
            "{}",
            fetched.status_text
        );
    }

    /// A page that returns real HTML but converts to no readable text is the
    /// other half of the reported failure and must say so.
    #[test]
    fn blank_browser_page_explains_itself_instead_of_paging_metadata() {
        let fetched = browser_page_to_fetched(
            browser_page(
                "https://example.com/",
                "<html><head><script>var x=1;</script></head><body></body></html>",
                Some(200),
            ),
            OutputFormat::Markdown,
        );
        assert!(fetched.blank_content);
        let chunk = make_chunk(&fetched.content, 0, 20_000);
        let rendered = format_page_output(&fetched, &chunk, true);
        assert!(rendered.contains("no readable content"), "{rendered}");
        assert!(
            !rendered.contains("offset is at or beyond"),
            "the cause is not the offset: {rendered}"
        );
    }

    #[test]
    fn http_fetch_mode_default_is_browser() {
        assert_eq!(
            crate::config::settings::HttpFetchMode::default(),
            crate::config::settings::HttpFetchMode::Browser
        );
    }

    /// The regression: a page that merely *mentions* a challenge must be returned
    /// as content.
    ///
    /// Driven through the real `fetch_page_http` against a local server, because
    /// the bug was in the assembled path — conversion, blank detection and marker
    /// matching all had to agree — not in any one helper.
    #[tokio::test]
    async fn a_page_that_mentions_a_captcha_is_content_not_a_block() {
        let (url, handle) = serve_once(
            "200 OK",
            "text/html; charset=utf-8",
            r#"<!DOCTYPE html><html><head><title>如何配置验证码</title></head><body>
               <main><h1>Cloudflare 的验证码拦截是怎么工作的</h1>
               <p>这篇文档说明 captcha 与 Cloudflare 的 challenge-platform 在自行架站时的
               配置方式，正文足够长，会被可读内容提取器选中。文中反复出现这些词是正常的，
               因为它讲的就是这些机制本身，而不是被它们拦住了。</p>
               <p>第二段继续补充正文，确保这段内容明显超过一句验证提示的长度，从而落在
               拦截页与正常内容页之间那条界线正确的一侧。</p>
               <p>第三段同样只是为了让样本具备一篇真实文档该有的体量。</p></main></body></html>"#,
        );

        let page = fetch_page_http(&url, OutputFormat::Markdown)
            .await
            .expect("fetch local");
        assert!(
            !page.blank_content,
            "the extractor must find the article body, not nothing"
        );
        assert_eq!(
            page.challenge, None,
            "readable content outweighs the words inside it"
        );
        assert!(
            page.content.contains("Cloudflare"),
            "the body must reach the model: {}",
            page.content
        );
        handle.join().expect("server thread");
    }

    /// The other half: when the response really is an interstitial, it is still
    /// named as one, so the honest failure survives the gate.
    #[tokio::test]
    async fn an_interstitial_with_no_body_is_still_reported_as_a_block() {
        let (url, handle) = serve_once(
            "200 OK",
            "text/html; charset=utf-8",
            r#"<html><head><title>百度安全验证</title></head>
               <body><div id="waf">请完成安全验证</div></body></html>"#,
        );

        let page = fetch_page_http(&url, OutputFormat::Markdown)
            .await
            .expect("fetch local");
        assert_eq!(page.challenge, Some("百度安全验证"));
        handle.join().expect("server thread");
    }

    #[tokio::test]
    async fn fetch_page_http_reads_local_server() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::thread;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let body = r#"<!DOCTYPE html><html><head><title>Local</title></head>
                <body><main><h1>Hello Local</h1><p>Content from local fixture server.</p></main></body></html>"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes());
        });

        let url = format!("http://{}/page", addr);
        let page = fetch_page_http(&url, OutputFormat::Markdown)
            .await
            .expect("fetch local");
        assert_eq!(page.status, Some(200));
        assert!(!page.http_error);
        assert!(page.content.contains("Hello Local") || page.title.as_deref() == Some("Local"));
        handle.join().expect("server thread");
    }

    #[tokio::test]
    async fn fetch_page_http_marks_404_as_http_error() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::thread;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let body = "<html><body><h1>Not Found</h1></body></html>";
            let resp = format!(
                "HTTP/1.1 404 Not Found\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes());
        });

        let url = format!("http://{}/missing", addr);
        let page = fetch_page_http(&url, OutputFormat::Markdown)
            .await
            .expect("fetch 404 body");
        assert_eq!(page.status, Some(404));
        assert!(page.http_error);
        assert!(page.content.contains("Not Found") || page.title.is_some());
        handle.join().expect("server thread");
    }

    // ── Per-mode coverage for http_get ─────────────────────────────────

    #[test]
    fn http_mode_html_pipeline_via_local_page_struct() {
        // Html mode: raw HTTP body → FetchedPage fields used by tool output.
        // We simulate the post-fetch conversion path used after fetch_page_http.
        let page = FetchedPage {
            requested_url: "https://example.com/doc".into(),
            final_url: "https://example.com/doc".into(),
            content_type: "text/html".into(),
            title: Some("Doc".into()),
            content: "# Doc\n\nBody".into(),
            source_bytes: 100,
            markdown_bytes: 12,
            ..fetched("# Doc\n\nBody")
        };
        let chunk = make_chunk(&page.content, 0, 2000);
        let meta = page_metadata(&page, &chunk);
        assert_eq!(meta["status"], 200);
        assert_eq!(meta["http_error"], false);
        assert_eq!(meta["provider"], "html");
        assert_eq!(meta["blank_content"], false);
        let out = format_page_output(&page, &chunk, true);
        assert!(out.contains("Doc"));
        assert!(out.contains("Status: 200"));
    }

    #[test]
    fn http_mode_browser_pipeline_provider_and_status() {
        // Browser mode: BrowserPage → browser_page_to_fetched → metadata path.
        let html = r#"
                <html><head><title>Browser Page</title></head>
                <body><main><h1>Rendered</h1><p>SPA content long enough for readable extract.</p></main></body></html>
            "#;
        let page = browser_cdp::BrowserPage {
            title: Some("Browser Page".into()),
            ..browser_page("https://example.com/a", html, Some(200))
        };
        let fetched = browser_page_to_fetched(page, OutputFormat::Markdown);
        assert_eq!(fetched.status_text, "OK (browser)");
        assert_eq!(fetched.status, Some(200));
        assert!(!fetched.http_error);
        let chunk = make_chunk(&fetched.content, 0, 4000);
        let meta = page_metadata(&fetched, &chunk);
        assert_eq!(meta["status"], 200);
        assert_eq!(meta["provider"], "browser");
        assert!(
            fetched.content.contains("Rendered")
                || fetched.title.as_deref() == Some("Browser Page")
        );
    }

    #[test]
    fn http_fetch_mode_labels_for_tool_metadata() {
        // Tool metadata provider strings must match the shared backend labels.
        for (mode, expected) in [
            (HttpFetchMode::Browser, WebBackend::Browser.label()),
            (HttpFetchMode::Html, WebBackend::Html.label()),
            (HttpFetchMode::Deepseek, WebBackend::Deepseek.label()),
            (HttpFetchMode::Tavily, WebBackend::Tavily.label()),
            (HttpFetchMode::Custom, WebBackend::Custom.label()),
        ] {
            assert_eq!(format!("{:?}", mode).to_ascii_lowercase(), expected);
        }
        assert_eq!(WebBackend::Browser.label(), "browser");
        assert_eq!(WebBackend::Html.label(), "html");
        assert_eq!(WebBackend::Deepseek.label(), "deepseek");
        assert_eq!(WebBackend::Custom.label(), "custom");
    }

    // ── DeepSeek fetch ──────────────────────────────────────────────────────

    /// The sample response from the official docs.
    const DEEPSEEK_FETCH_SAMPLE: &str =
        r##"{"title":"Example article","content":"# Example article\n\nArticle content."}"##;

    const LOCAL_FIXTURE_PAGE: &str = "<html><head><title>Local</title></head><body><main>\
        <h1>Local page</h1><p>Served from the loopback fixture, long enough to be extracted.</p>\
        </main></body></html>";

    #[test]
    fn private_targets_stay_on_this_machine() {
        for target in [
            "http://localhost:8080/",
            "http://LOCALHOST/",
            "http://127.0.0.1/",
            "http://127.1/",
            "http://2130706433/",
            "http://10.0.0.5/",
            "http://172.16.3.4/x",
            "http://192.168.1.10:3000/",
            "http://169.254.169.254/latest/meta-data",
            // CGNAT: where Tailscale addresses live.
            "http://100.101.102.103/",
            "http://[::1]/",
            "http://[fd00::1]/",
            "http://[fe80::1]/",
            "http://[::ffff:192.168.0.1]/",
            "http://nas/",
            "http://router.local/",
            "http://grafana.internal/",
            "http://printer.lan./",
            // Credentials must never be handed to a third party.
            "https://user:pass@example.com/",
            "https://user@example.com/",
            "ftp://example.com/file",
            "not a url",
            "",
        ] {
            assert!(is_private_target(target), "{target:?} must stay local");
        }
    }

    #[test]
    fn public_targets_may_go_to_deepseek() {
        for target in [
            "https://example.com/a?b=1",
            "http://example.org",
            "https://docs.rs/tokio",
            "https://[2606:4700:4700::1111]/",
            "https://8.8.8.8/",
            // Just outside the private / CGNAT ranges.
            "https://172.32.0.1/",
            "https://100.63.0.1/",
            "https://100.128.0.1/",
            // "localhost" as a label of a public name is not the loopback.
            "https://localhost.example.com/",
        ] {
            assert!(!is_private_target(target), "{target:?} is public");
        }
    }

    #[test]
    fn deepseek_page_is_markdown_with_an_honest_unknown_status() {
        let page = parse_remote_page(
            "https://example.com/article",
            DEEPSEEK_FETCH_SAMPLE,
            WebBackend::Deepseek,
        )
        .unwrap();
        assert_eq!(page.provider, "deepseek");
        assert_eq!(page.title.as_deref(), Some("Example article"));
        assert!(page.content.contains("Article content."));
        assert_eq!(
            page.status, None,
            "DeepSeek never reports the page's status"
        );
        assert!(!page.http_error && !page.redirected && !page.blank_content);
        assert_eq!(page.challenge, None);

        let chunk = make_chunk(&page.content, 0, 20_000);
        let rendered = format_page_output(&page, &chunk, true);
        assert!(
            rendered.contains("Status: unknown — page status not reported (fetched by DeepSeek)"),
            "{rendered}"
        );
        assert!(!rendered.contains("Status: 200"), "{rendered}");
        assert!(
            rendered.contains("Content-Type: text/markdown"),
            "{rendered}"
        );

        let meta = page_metadata(&page, &chunk);
        assert_eq!(meta["provider"], "deepseek");
        assert!(meta["status"].is_null(), "{meta}");
    }

    #[test]
    fn deepseek_page_tolerates_missing_fields_and_rejects_garbage() {
        let page = parse_remote_page("https://example.com/", "{}", WebBackend::Deepseek).unwrap();
        assert!(page.blank_content);
        assert_eq!(page.title, None);
        assert!(parse_remote_page("https://example.com/", "oops", WebBackend::Deepseek).is_err());
    }

    #[test]
    fn deepseek_page_that_is_a_verification_interstitial_is_recognised() {
        let page = parse_remote_page(
            "https://baike.baidu.com/item/x",
            r#"{"title":"百度安全验证","content":"请完成安全验证"}"#,
            WebBackend::Deepseek,
        )
        .unwrap();
        assert_eq!(page.challenge, Some("百度安全验证"));
    }

    /// Private URLs never reach DeepSeek, and a DeepSeek failure is stated, not hidden.
    #[tokio::test]
    async fn deepseek_batch_reads_private_urls_locally_and_falls_back_loudly() {
        let (local_url, h1) = serve_once("200 OK", "text/html; charset=utf-8", LOCAL_FIXTURE_PAGE);
        let (failed_url, h2) = serve_once("200 OK", "text/html; charset=utf-8", LOCAL_FIXTURE_PAGE);
        let public = "https://example.com/article".to_string();
        let deepseek_page =
            parse_remote_page(&public, DEEPSEEK_FETCH_SAMPLE, WebBackend::Deepseek).unwrap();

        let urls = vec![local_url, public, failed_url];
        let outcomes = vec![
            None,
            Some(Ok(deepseek_page)),
            Some(Err(
                "DeepSeek fetch HTTP 402: Insufficient Balance".to_string()
            )),
        ];
        let batch = finish_remote_batch(
            &urls,
            outcomes,
            OutputFormat::Markdown,
            WebBackend::Deepseek,
        )
        .await;

        let providers: Vec<_> = batch
            .results
            .iter()
            .map(|r| r.as_ref().expect("every page resolved").provider)
            .collect();
        assert_eq!(providers, ["html", "deepseek", "html"]);
        assert_eq!(batch.provider(), "mixed");

        let note = batch
            .fallback
            .as_ref()
            .expect("a DeepSeek failure is stated");
        assert_eq!((note.from, note.to), ("deepseek", "html"));
        assert!(note.reason.contains("1 of 2 page(s)"), "{}", note.reason);
        assert!(
            note.reason.contains("Insufficient Balance"),
            "{}",
            note.reason
        );
        h1.join().expect("server thread");
        h2.join().expect("server thread");
    }

    /// Keeping a private URL local is the design, so it must not be flagged as a
    /// degraded fetch.
    #[tokio::test]
    async fn deepseek_batch_of_only_private_urls_is_not_a_degradation() {
        let (url, handle) = serve_once("200 OK", "text/html; charset=utf-8", LOCAL_FIXTURE_PAGE);
        let batch = finish_remote_batch(
            &[url],
            vec![None],
            OutputFormat::Markdown,
            WebBackend::Deepseek,
        )
        .await;

        assert!(batch.fallback.is_none());
        assert_eq!(batch.provider(), "html");
        handle.join().expect("server thread");
    }

    // ── Custom fetch service ────────────────────────────────────────────────

    #[test]
    fn custom_service_keeps_private_urls_local_unless_the_user_opts_in() {
        let private = [
            "http://localhost:8080/",
            "http://192.168.1.10/wiki",
            "http://nas/",
            "http://100.101.102.103/",
        ];
        for target in private {
            assert!(
                must_stay_local(target, false),
                "{target} stays local by default"
            );
            assert!(
                !must_stay_local(target, true),
                "{target} may go to the user's own service"
            );
        }
        // Public addresses go to the service either way.
        for send_private in [false, true] {
            assert!(!must_stay_local("https://example.com/a", send_private));
        }
        // Whatever we cannot vouch for as a plain web address never leaves, opt-in or not.
        for target in [
            "https://user:pass@example.com/",
            "https://user@192.168.1.5/",
            "ftp://example.com/file",
            "file:///etc/passwd",
            "not a url",
            "",
        ] {
            assert!(
                must_stay_local(target, true),
                "{target:?} must never be sent"
            );
            assert!(
                must_stay_local(target, false),
                "{target:?} must never be sent"
            );
        }
    }

    #[test]
    fn custom_page_is_labelled_and_worded_for_the_custom_service() {
        let page = parse_remote_page(
            "https://example.com/article",
            DEEPSEEK_FETCH_SAMPLE,
            WebBackend::Custom,
        )
        .unwrap();
        assert_eq!(page.provider, "custom");
        assert_eq!(page.status, None);
        let chunk = make_chunk(&page.content, 0, 20_000);
        let rendered = format_page_output(&page, &chunk, true);
        assert!(
            rendered.contains("page status not reported (fetched by the custom fetch service)"),
            "{rendered}"
        );
        assert!(!rendered.contains("DeepSeek"), "{rendered}");

        let err = match parse_remote_page("https://example.com/", "oops", WebBackend::Custom) {
            Err(e) => e,
            Ok(_) => panic!("garbage must not parse"),
        };
        assert!(err.to_string().contains("custom fetch service"), "{err}");
        assert!(!err.to_string().contains("DeepSeek"), "{err}");
    }

    #[tokio::test]
    async fn custom_batch_failure_is_attributed_to_the_custom_service() {
        let (local_url, handle) =
            serve_once("200 OK", "text/html; charset=utf-8", LOCAL_FIXTURE_PAGE);
        let batch = finish_remote_batch(
            &[local_url],
            vec![Some(Err(
                "custom fetch service HTTP 502 Bad Gateway: down".to_string()
            ))],
            OutputFormat::Markdown,
            WebBackend::Custom,
        )
        .await;

        let note = batch.fallback.as_ref().expect("a failure is stated");
        assert_eq!((note.from, note.to), ("custom", "html"));
        assert!(
            note.reason.contains("the custom fetch service"),
            "{}",
            note.reason
        );
        assert!(!note.reason.contains("DeepSeek"), "{}", note.reason);
        assert_eq!(batch.provider(), "html");
        handle.join().expect("server thread");
    }

    /// An unset address is a stated degradation: the page is still read locally.
    #[tokio::test]
    async fn custom_mode_without_an_address_degrades_loudly_to_a_local_read() {
        let (url, handle) = serve_once("200 OK", "text/html; charset=utf-8", LOCAL_FIXTURE_PAGE);
        // Loopback fixture + send_private: it *would* be sent, so this exercises the
        // "no usable address" path rather than the keep-local path.
        let config = CustomFetchApi {
            url: String::new(),
            send_private: true,
            ..Default::default()
        };
        let batch = fetch_all_custom(&[url], OutputFormat::Markdown, &config).await;

        let note = batch.fallback.as_ref().expect("missing address is stated");
        assert!(note.reason.contains("not configured"), "{}", note.reason);
        assert!(batch.results[0].is_ok(), "the page was read locally");
        handle.join().expect("server thread");
    }

    // ── Tavily Extract (official mode and the custom service's Tavily protocol) ──

    const TAVILY_EXTRACT_SAMPLE: &str = r##"{"results":[{"url":"https://example.com/article","raw_content":"# Example article\n\nArticle content from Tavily."}],"failed_results":[],"response_time":0.4}"##;

    #[test]
    fn tavily_page_is_markdown_with_an_honest_unknown_status() {
        let page = parse_tavily_page(
            "https://example.com/article",
            TAVILY_EXTRACT_SAMPLE,
            WebBackend::Tavily,
            OutputFormat::Markdown,
        )
        .unwrap();
        assert_eq!(page.provider, "tavily");
        assert_eq!(page.title, None, "Tavily extraction carries no title");
        assert!(page.content.contains("Article content from Tavily."));
        assert_eq!(page.status, None);
        assert_eq!(page.content_type, "text/markdown; charset=utf-8");
        assert!(!page.http_error && !page.redirected && !page.blank_content);

        let chunk = make_chunk(&page.content, 0, 20_000);
        let rendered = format_page_output(&page, &chunk, true);
        assert!(
            rendered.contains("page status not reported (fetched by Tavily)"),
            "{rendered}"
        );
        assert!(!rendered.contains("DeepSeek"), "{rendered}");

        let text = parse_tavily_page(
            "https://example.com/article",
            TAVILY_EXTRACT_SAMPLE,
            WebBackend::Tavily,
            OutputFormat::Text,
        )
        .unwrap();
        assert_eq!(text.content_type, "text/plain; charset=utf-8");
    }

    /// HTTP 200 with the URL listed under `failed_results` is a failed fetch whose
    /// reason reaches the user, not an empty page.
    #[test]
    fn tavily_failed_results_surface_their_reason() {
        let err = match parse_tavily_page(
            "https://example.com/gone",
            r#"{"results":[],"failed_results":[{"url":"https://example.com/gone","error":"Failed to fetch url"}]}"#,
            WebBackend::Tavily,
            OutputFormat::Markdown,
        ) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("a failed extraction is not a page"),
        };
        assert!(
            err.contains("Tavily could not extract example.com"),
            "{err}"
        );
        assert!(err.contains("Failed to fetch url"), "{err}");

        for body in [r#"{"results":[],"failed_results":[]}"#, "{}"] {
            let err = match parse_tavily_page(
                "https://example.com/",
                body,
                WebBackend::Tavily,
                OutputFormat::Markdown,
            ) {
                Err(e) => e.to_string(),
                Ok(_) => panic!("{body} has no page"),
            };
            assert!(err.contains("no result for the requested URL"), "{err}");
        }
    }

    #[test]
    fn tavily_garbage_and_naming_follow_the_backend() {
        let err = |backend| match parse_tavily_page(
            "https://example.com/",
            "oops",
            backend,
            OutputFormat::Markdown,
        ) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("garbage must not parse"),
        };
        assert!(err(WebBackend::Tavily).contains("invalid Tavily response"));
        let custom = err(WebBackend::Custom);
        assert!(custom.contains("custom fetch service"), "{custom}");
        assert!(
            !custom.contains("Tavily") && !custom.contains("DeepSeek"),
            "{custom}"
        );

        let page = parse_tavily_page(
            "https://example.com/article",
            TAVILY_EXTRACT_SAMPLE,
            WebBackend::Custom,
            OutputFormat::Markdown,
        )
        .unwrap();
        assert_eq!(page.provider, "custom");
        let chunk = make_chunk(&page.content, 0, 20_000);
        let rendered = format_page_output(&page, &chunk, true);
        assert!(
            rendered.contains("fetched by the custom fetch service"),
            "{rendered}"
        );
    }

    /// The shared search slot holds whichever vendor's key was saved last, so with Brave
    /// as the search provider it is Brave's key and must never reach Tavily.
    #[test]
    fn tavily_fetch_key_never_borrows_a_brave_key() {
        let key = |s: &str| Some(s.to_string());
        use WebSearchApiProvider::{Brave, Custom, Deepseek, Tavily};

        // Its own slot always wins.
        for provider in [Brave, Tavily, Deepseek, Custom] {
            assert_eq!(
                pick_tavily_fetch_key(key(" own "), key("shared"), provider),
                key("own"),
                "{provider:?}"
            );
        }
        // Shared slot only while Tavily is the search provider.
        assert_eq!(
            pick_tavily_fetch_key(None, key("shared"), Tavily),
            key("shared")
        );
        for provider in [Brave, Deepseek, Custom] {
            assert_eq!(
                pick_tavily_fetch_key(None, key("shared"), provider),
                None,
                "{provider:?} must not lend its key to Tavily"
            );
        }
        // Blank values do not count as keys.
        assert_eq!(
            pick_tavily_fetch_key(key("  "), key("shared"), Tavily),
            key("shared")
        );
        assert_eq!(pick_tavily_fetch_key(key(""), key(" "), Tavily), None);
    }

    /// Serve one canned reply and hand back the raw request (headers + body).
    fn serve_capturing(
        status: &str,
        body: &'static str,
    ) -> (String, std::thread::JoinHandle<String>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let reply = format!(
            "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            status,
            body.len(),
            body
        );
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = stream.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break;
                }
                raw.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&raw).to_string();
                if let Some(split) = text.find("\r\n\r\n") {
                    let length = text[..split]
                        .to_ascii_lowercase()
                        .lines()
                        .find_map(|l| {
                            l.strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if raw.len() >= split + 4 + length {
                        break;
                    }
                }
            }
            let _ = stream.write_all(reply.as_bytes());
            String::from_utf8_lossy(&raw).into_owned()
        });
        (format!("http://{}/extract", addr), handle)
    }

    #[tokio::test]
    async fn custom_service_with_the_tavily_protocol_sends_urls_and_reads_raw_content() {
        let (endpoint, handle) = serve_capturing("200 OK", TAVILY_EXTRACT_SAMPLE);
        let fetcher = RemoteFetcher {
            backend: WebBackend::Custom,
            protocol: FetchApiProtocol::Tavily,
            endpoint: Some(endpoint),
            api_key: "tvly-test".to_string(),
            format: OutputFormat::Text,
        };
        let page = fetch_page_remote(&fetcher, "https://example.com/article")
            .await
            .expect("page");
        assert_eq!(page.provider, "custom");
        assert!(page.content.contains("Article content from Tavily."));

        let request = handle.join().expect("server thread");
        let lower = request.to_ascii_lowercase();
        assert!(lower.starts_with("post /extract "), "{request}");
        assert!(
            lower.contains("authorization: bearer tvly-test"),
            "{request}"
        );
        assert!(
            request.contains(r#""urls":["https://example.com/article"]"#),
            "{request}"
        );
        assert!(request.contains(r#""format":"text""#), "{request}");
        // The DeepSeek-shaped body must not leak into the Tavily protocol.
        assert!(!request.contains(r#""url":"#), "{request}");
    }

    #[tokio::test]
    async fn custom_service_with_the_deepseek_protocol_still_sends_a_single_url() {
        let (endpoint, handle) =
            serve_capturing("200 OK", r#"{"title":"T","content":"Body text here."}"#);
        let fetcher = RemoteFetcher {
            backend: WebBackend::Custom,
            protocol: FetchApiProtocol::Deepseek,
            endpoint: Some(endpoint),
            api_key: String::new(),
            format: OutputFormat::Markdown,
        };
        let page = fetch_page_remote(&fetcher, "https://example.com/a")
            .await
            .expect("page");
        assert_eq!(page.title.as_deref(), Some("T"));
        let request = handle.join().expect("server thread");
        assert!(
            request.contains(r#"{"url":"https://example.com/a"}"#),
            "{request}"
        );
        assert!(
            !request.to_ascii_lowercase().contains("authorization"),
            "{request}"
        );
    }

    /// A Tavily error body (`{"detail":{"error":..}}`) reaches the user readable.
    #[tokio::test]
    async fn tavily_http_errors_are_readable_and_named() {
        let (endpoint, handle) = serve_capturing(
            "401 Unauthorized",
            r#"{"detail":{"error":"Unauthorized: missing or invalid API key."}}"#,
        );
        let fetcher = RemoteFetcher {
            backend: WebBackend::Tavily,
            protocol: FetchApiProtocol::Tavily,
            endpoint: Some(endpoint),
            api_key: "bad".to_string(),
            format: OutputFormat::Markdown,
        };
        let err = match fetch_page_remote(&fetcher, "https://example.com/").await {
            Err(e) => e.to_string(),
            Ok(_) => panic!("401 is a failure"),
        };
        handle.join().expect("server thread");
        assert!(err.contains("Tavily extract HTTP 401"), "{err}");
        assert!(err.contains("missing or invalid API key"), "{err}");
    }

    #[tokio::test]
    async fn tavily_batch_failure_is_attributed_to_tavily() {
        let (local_url, handle) =
            serve_once("200 OK", "text/html; charset=utf-8", LOCAL_FIXTURE_PAGE);
        let batch = finish_remote_batch(
            &[local_url],
            vec![Some(Err(
                "Tavily could not extract x.com: blocked".to_string()
            ))],
            OutputFormat::Markdown,
            WebBackend::Tavily,
        )
        .await;
        let note = batch.fallback.as_ref().expect("a failure is stated");
        assert_eq!((note.from, note.to), ("tavily", "html"));
        assert!(note.reason.contains("failed on Tavily"), "{}", note.reason);
        assert!(!note.reason.contains("DeepSeek"), "{}", note.reason);
        handle.join().expect("server thread");
    }

    #[test]
    fn http_mode_html_and_browser_produce_different_status_text() {
        let html_page = fetched("x");
        let browser_result = browser_page_to_fetched(
            browser_page(
                "https://e.com",
                "<html><body><main><p>Enough text for the readable html extractor path here.</p></main></body></html>",
                Some(200),
            ),
            OutputFormat::Text,
        );
        assert_eq!(html_page.status_text, "OK");
        assert_eq!(browser_result.status_text, "OK (browser)");
        assert_ne!(html_page.provider, browser_result.provider);
    }

    #[tokio::test]
    async fn http_mode_html_live_local_server_end_to_end() {
        // Full html-mode fetch path (fetch_page_http) already covered above;
        // this asserts multi-URL concurrent join path shape with one URL.
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::thread;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf);
            let body =
                "<html><body><main><h1>Batch</h1><p>ok page content here</p></main></body></html>";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes());
        });

        let url = format!("http://{}/one", addr);
        let results =
            futures::future::join_all(vec![fetch_page_http(&url, OutputFormat::Markdown)]).await;
        assert_eq!(results.len(), 1);
        let page = results[0].as_ref().expect("ok");
        assert!(!page.http_error);
        assert!(page.content.contains("Batch") || page.title.is_some());
        handle.join().expect("join");
    }

    /// Optional live browser fetch — ignored by default.
    #[tokio::test]
    #[ignore = "live Chrome/Edge; run with --ignored when available"]
    async fn http_mode_browser_live_fetch_smoke() {
        let pages = browser_cdp::fetch_html_many(&["https://example.com/".into()]).await;
        assert_eq!(pages.len(), 1);
        let page = pages.into_iter().next().unwrap().expect("browser fetch");
        let fetched = browser_page_to_fetched(page, OutputFormat::Markdown);
        assert!(!fetched.content.is_empty() || fetched.title.is_some());
    }

    // ── Browser → HTTP handoffs, exercising the real HTTP fetcher ────────

    async fn retry_fixture(
        status: &str,
        content_type: &str,
        body: &str,
        requests: usize,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len(),
        );
        let task = tokio::spawn(async move {
            let mut paths = Vec::new();
            for _ in 0..requests {
                let (mut stream, _) =
                    tokio::time::timeout(Duration::from_secs(5), listener.accept())
                        .await
                        .unwrap()
                        .unwrap();
                let mut buf = [0; 4096];
                let n = stream.read(&mut buf).await.unwrap();
                paths.push(
                    String::from_utf8_lossy(&buf[..n])
                        .lines()
                        .next()
                        .unwrap()
                        .to_string(),
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            paths
        });
        (url, task)
    }

    fn navigation_failure(
        stage: browser_cdp::CdpStage,
    ) -> Result<browser_cdp::BrowserPage, browser_cdp::CdpFailure> {
        Err(browser_cdp::CdpFailure {
            stage,
            message: "fixture navigation".into(),
        })
    }

    #[tokio::test]
    async fn browser_batch_retries_only_missing_pages_and_reports_mixed() {
        let (base, server) =
            retry_fixture("200 OK", "text/plain", "HTTP recovered content", 1).await;
        let urls = vec![format!("{base}/rendered"), format!("{base}/retry")];
        let batch = finish_browser_batch(
            &urls,
            vec![
                Ok(browser_page(
                    &urls[0],
                    "<html><body>Rendered original</body></html>",
                    Some(200),
                )),
                navigation_failure(browser_cdp::CdpStage::Navigate),
            ],
            OutputFormat::Markdown,
        )
        .await;
        assert_eq!(batch.provider(), "mixed");
        assert!(batch.fallback.as_ref().unwrap().reason.contains("1 of 2"));
        let first = batch.results[0].as_ref().unwrap();
        let second = batch.results[1].as_ref().unwrap();
        assert_eq!(first.requested_url, urls[0]);
        assert_eq!(second.requested_url, urls[1]);
        assert!(first.content.contains("Rendered original"));
        assert_eq!(first.provider, "browser");
        assert_eq!(second.provider, "html");
        assert_eq!(second.content, "HTTP recovered content");
        assert_eq!(server.await.unwrap(), ["GET /retry HTTP/1.1"]);
    }

    #[tokio::test]
    async fn browser_download_reads_source_without_a_degradation() {
        let source = "package billingexpr\n\nfunc Compile() {}";
        let (url, server) = retry_fixture("200 OK", "application/octet-stream", source, 1).await;
        let batch = finish_browser_batch(
            &[url],
            vec![navigation_failure(browser_cdp::CdpStage::Download)],
            OutputFormat::Markdown,
        )
        .await;
        assert_eq!(batch.provider(), "html");
        assert!(batch.fallback.is_none());
        let page = batch.results[0].as_ref().unwrap();
        assert_eq!(page.content, source);
        assert!(!page.http_error && !page.blank_content);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn browser_download_and_document_are_mixed_without_a_degradation() {
        let (url, server) = retry_fixture("200 OK", "text/plain", "source", 1).await;
        let urls = vec!["https://rendered.example/".to_string(), url];
        let batch = finish_browser_batch(
            &urls,
            vec![
                Ok(browser_page(
                    &urls[0],
                    "<html><body>Page</body></html>",
                    Some(200),
                )),
                navigation_failure(browser_cdp::CdpStage::Download),
            ],
            OutputFormat::Markdown,
        )
        .await;
        assert_eq!(batch.provider(), "mixed");
        assert!(batch.fallback.is_none());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn browser_batch_counts_real_failures_separately_from_downloads() {
        let (url, server) = retry_fixture("200 OK", "text/plain", "source", 2).await;
        let batch = finish_browser_batch(
            &[url.clone(), url],
            vec![
                navigation_failure(browser_cdp::CdpStage::Download),
                navigation_failure(browser_cdp::CdpStage::Read),
            ],
            OutputFormat::Markdown,
        )
        .await;
        assert_eq!(batch.provider(), "html");
        assert!(batch.fallback.as_ref().unwrap().reason.contains("1 of 2"));
        assert!(batch
            .fallback
            .as_ref()
            .unwrap()
            .reason
            .contains("browser read"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn browser_handoffs_preserve_http_error_challenge_and_blank_classification() {
        for (status, body, http_error, challenge, blank) in [
            (
                "404 Not Found",
                "<html><body>Not Found</body></html>",
                true,
                false,
                false,
            ),
            (
                "200 OK",
                "<html><body>百度安全验证</body></html>",
                false,
                true,
                false,
            ),
            ("200 OK", "<html><body></body></html>", false, false, true),
        ] {
            let (url, server) = retry_fixture(status, "text/html", body, 1).await;
            let batch = finish_browser_batch(
                &[url],
                vec![navigation_failure(browser_cdp::CdpStage::Navigate)],
                OutputFormat::Markdown,
            )
            .await;
            let page = batch.results[0].as_ref().unwrap();
            assert_eq!(page.http_error, http_error);
            assert_eq!(page.challenge.is_some(), challenge);
            assert_eq!(page.blank_content, blank);
            assert!(batch.fallback.is_some());
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn browser_and_http_failure_keeps_the_original_fallback_reason() {
        // A closed port can produce a proxy's HTTP error page on some machines.
        // Invalid advertised compression deterministically fails body decoding.
        let (url, server) = retry_fixture(
            "200 OK",
            "text/plain\r\nContent-Encoding: gzip",
            "invalid gzip",
            1,
        )
        .await;
        let batch = finish_browser_batch(
            &[url],
            vec![navigation_failure(browser_cdp::CdpStage::Read)],
            OutputFormat::Markdown,
        )
        .await;
        assert_eq!(batch.provider(), "html");
        assert!(batch.results[0].is_err());
        assert!(batch.fallback.unwrap().reason.contains("browser read"));
        server.await.unwrap();
    }

    /// Exercises the complete browser → download → HTTP path with a real
    /// Chromium, including continued use of the same page after a download.
    #[tokio::test]
    #[ignore = "live: starts a local headless browser against loopback fixtures"]
    async fn browser_download_handoff_live_local_server() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut buf = [0; 4096];
                    let n = stream.read(&mut buf).await.unwrap();
                    let request = String::from_utf8_lossy(&buf[..n]);
                    let (mime, body) = if request.starts_with("GET /source ") {
                        (
                            "application/octet-stream",
                            "package billingexpr\n\nfunc Compile() {}",
                        )
                    } else {
                        (
                            "text/html",
                            "<html><body><h1>Rendered page after download</h1></body></html>",
                        )
                    };
                    let response = format!("HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nX-Content-Type-Options: nosniff\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        let urls = vec![format!("{base}/source"), format!("{base}/page")];
        let batch = fetch_all_browser(&urls, OutputFormat::Markdown).await;
        server.abort();
        assert_eq!(batch.provider(), "mixed");
        assert!(batch.fallback.is_none(), "{:?}", batch.fallback);
        let source = batch.results[0].as_ref().expect("read download over HTTP");
        let page = batch.results[1].as_ref().expect("continue browser session");
        assert_eq!(source.content, "package billingexpr\n\nfunc Compile() {}");
        assert_eq!(source.provider, "html");
        assert_eq!(source.status, Some(200));
        assert_eq!(page.provider, "browser");
        assert!(page.content.contains("Rendered page after download"));
        assert_eq!(page.final_url, urls[1]);
    }

    /// A fully blank page must be reported as a failure, not as a successful
    /// fetch of zero bytes.
    #[test]
    fn blank_pages_make_the_result_a_failure() {
        let page = browser_page_to_fetched(
            browser_page(
                "https://example.com/",
                "<html><head><style>p{color:red}</style></head><body></body></html>",
                Some(200),
            ),
            OutputFormat::Markdown,
        );
        assert!(page.blank_content, "nothing readable was produced");
        // The decision inputs the tool keys off, asserted directly.
        let all_blank = [page].iter().all(|p| p.blank_content);
        assert!(all_blank);
    }

    /// A block page must never count as a success.
    #[test]
    fn an_intercepted_page_is_not_counted_as_success() {
        let intercepted = browser_page_to_fetched(
            browser_page(
                "https://baike.baidu.com/item/绝区零",
                "<html><head><title>百度安全验证</title></head><body></body></html>",
                None,
            ),
            OutputFormat::Markdown,
        );
        let is_failure = intercepted.http_error || intercepted.challenge.is_some();
        assert!(is_failure, "a verification page is a failed fetch");
    }
}
