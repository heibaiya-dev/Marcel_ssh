//! WebDAV 存储适配器（坚果云 / Nextcloud / Synology / 各种自建 DAV 服务）。
//!
//! 只用最基础的五个动作：`PROPFIND`（Depth: 1 列目录）、`GET`、`PUT`、`DELETE`、`MKCOL`，
//! 不依赖 ETag / 条件请求 / LOCK 这类各家实现不一的能力——同步协议本来就不需要它们（每台设备
//! 只写自己的文件）。认证固定 Basic（坚果云 / Nextcloud 的「应用密码」都走它），密码由调用方
//! 传入，只在内存里（`Zeroizing`），不落盘、不进日志、不进错误文案。
//!
//! ## 请求要省着用
//!
//! 坚果云免费版有请求频率限制（短时间内请求太多会被拒），所以：
//! - `put` **先直接 PUT**，只有服务器说「上级目录不存在」（409 / 404）时才逐级 `MKCOL` 再重试
//!   一次——稳态下每次写入只有 1 个请求，而不是「先 MKCOL 两级目录再 PUT」的 3 个；
//! - 建过 / 确认存在的目录记在实例里，不会重复 `MKCOL`；
//! - 不做多余的 `HEAD` / 存在性探测。
//!
//! ## 服务器之间的差异（解析 `PROPFIND` 时都兜住了，样本见测试）
//!
//! - XML 前缀五花八门（`D:` / `d:` / 默认命名空间），只认本地名；
//! - `href` 可能是绝对路径、完整 URL、百分号编码的、也可能直接是未编码的中文；名字一律解码还原；
//! - 列表里的「自己」那一条（Depth: 1 会带上）要去掉，认路径而不是认顺序；
//! - 有的服务器忽略 Depth 把子孙全列出来 / 返回的路径带不同的挂载前缀，按「直接子项」收口；
//! - 目录的 `getcontentlength` 经常缺失或躺在 404 的 `propstat` 里，当 0；
//! - 地址不是 WebDAV（返回了普通网页）要给出看得懂的提示，而不是「XML 解析失败」。

use std::collections::HashSet;
use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use percent_encoding::percent_decode_str;
use quick_xml::events::Event;
use quick_xml::Reader;
use reqwest::{Method, RequestBuilder, Response};
use url::Url;
use zeroize::Zeroizing;

use super::{is_hidden_name, rel_segments, RemoteEntry, StorageError, SyncStorage};

/// 建连超时。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// 元数据请求（列目录 / 建目录 / 删除）的总超时。
const META_TIMEOUT: Duration = Duration::from_secs(30);
/// 带正文的请求（GET / PUT）的总超时：快照可能有几百 KB 到几 MB，慢上行下 30 秒不够，给宽一点，
/// 免得一份大快照永远传不上去、每轮同步都超时重来。
const BODY_TIMEOUT: Duration = Duration::from_secs(120);

const PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/><d:getcontentlength/></d:prop></d:propfind>"#;

/// 见模块文档。
pub struct WebDavStorage {
    /// 用户填的 WebDAV 地址，路径恒以 `/` 结尾，不含账号 / 查询串。
    endpoint: Url,
    /// `root` 拆出来的各段（未编码）。
    root: Vec<String>,
    username: String,
    password: Zeroizing<String>,
    client: reqwest::Client,
    /// 给错误文案用的 `host` 或 `host:port`（不含账号）。
    host: String,
    /// 已确认存在的目录（`root + 相对路径` 用 `/` 连起来）。服务器端被人删了会让缓存过期，
    /// 所以只当「少发请求」的提示：PUT 报缺目录时无视缓存重新逐级建。
    known_dirs: Mutex<HashSet<String>>,
}

impl fmt::Debug for WebDavStorage {
    /// 手写：不输出密码，也不输出完整地址里可能带的东西。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebDavStorage")
            .field("host", &self.host)
            .field("username", &self.username)
            .field("root", &self.root.join("/"))
            .finish_non_exhaustive()
    }
}

/// 项目惯例的 HTTP 客户端（沿用 `commands/market`、`mcp/client` 的建法）：默认走系统代理（桌面
/// 的 `system-proxy` feature；Android 没有）、TLS 后端按平台由 Cargo 特性决定，这里不另起一套。
/// 与它们不同的只有一点：**不跟随重定向**——WebDAV 的 PROPFIND / MKCOL 被 301 改成 GET 就静悄悄
/// 错了，不如把「地址被重定向到哪」明明白白告诉用户。
fn default_client() -> Result<reqwest::Client, StorageError> {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(META_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("MarcelSSH-Sync/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| StorageError::Other(format!("HTTP 客户端初始化失败：{e}")))
}

impl WebDavStorage {
    /// `url` 是 WebDAV 端点（坚果云：`https://dav.jianguoyun.com/dav/`），`root` 是端点之下的同步
    /// 目录（可以多级，默认 `marcel-sync`；空 = 直接用端点）。`password` 由调用方从密钥链取来。
    pub fn new(
        url: &str,
        username: &str,
        password: &str,
        root: &str,
    ) -> Result<Self, StorageError> {
        Self::with_client(url, username, password, root, default_client()?)
    }

    /// 与 [`Self::new`] 相同，但用调用方给的客户端（测试里要绕开系统代理连回环地址）。
    pub(crate) fn with_client(
        url: &str,
        username: &str,
        password: &str,
        root: &str,
        client: reqwest::Client,
    ) -> Result<Self, StorageError> {
        let mut endpoint = Url::parse(url.trim()).map_err(|_| {
            StorageError::Other("WebDAV 地址格式不对，应类似 https://dav.example.com/dav/".into())
        })?;
        if !matches!(endpoint.scheme(), "http" | "https") || endpoint.host_str().is_none() {
            return Err(StorageError::Other(
                "WebDAV 地址必须以 http:// 或 https:// 开头".into(),
            ));
        }
        // 地址会被存进同步配置、还会写进同步码：里面带了账号密码就等于把密码明文落盘，直接拒绝。
        if !endpoint.username().is_empty() || endpoint.password().is_some() {
            return Err(StorageError::Other(
                "WebDAV 地址里不要包含用户名和密码，请分别填在账号、密码栏里".into(),
            ));
        }
        endpoint.set_query(None);
        endpoint.set_fragment(None);
        if !endpoint.path().ends_with('/') {
            let with_slash = format!("{}/", endpoint.path());
            endpoint.set_path(&with_slash);
        }
        let root = rel_segments(root)?.into_iter().map(String::from).collect();
        let host = match endpoint.port() {
            Some(p) => format!("{}:{}", endpoint.host_str().unwrap_or_default(), p),
            None => endpoint.host_str().unwrap_or_default().to_string(),
        };
        Ok(Self {
            endpoint,
            root,
            username: username.to_string(),
            password: Zeroizing::new(password.to_string()),
            client,
            host,
            known_dirs: Mutex::new(HashSet::new()),
        })
    }

    /// `root + rel` 对应的完整 URL。每一段单独做百分号编码（中文 / 空格 / `%` 都安全）；
    /// `dir` 为真时补尾斜杠——集合（目录）一律带它，免得服务器为补斜杠回一个重定向。
    fn url_for(&self, rel: &[&str], dir: bool) -> Url {
        let mut url = self.endpoint.clone();
        {
            let mut segs = url
                .path_segments_mut()
                .expect("http(s) 地址一定可以作为 base");
            segs.pop_if_empty();
            for s in &self.root {
                segs.push(s);
            }
            for s in rel {
                segs.push(s);
            }
            if dir {
                segs.push("");
            }
        }
        url
    }

    fn request(&self, method: &str, url: Url, timeout: Duration) -> RequestBuilder {
        let method = Method::from_bytes(method.as_bytes()).expect("合法的 HTTP 方法名");
        self.client
            .request(method, url)
            .basic_auth(&self.username, Some(self.password.as_str()))
            .timeout(timeout)
    }

    async fn exec(&self, rb: RequestBuilder) -> Result<Response, StorageError> {
        rb.send().await.map_err(|e| self.net_error(e))
    }

    fn is_jianguoyun(&self) -> bool {
        self.host.ends_with("jianguoyun.com")
    }

    /// 传输层错误 → 给人看的中文。`without_url`：reqwest 的错误会带上完整 URL，保险起见剥掉。
    fn net_error(&self, e: reqwest::Error) -> StorageError {
        let e = e.without_url();
        let detail = error_chain(&e);
        let lower = detail.to_lowercase();
        if e.is_timeout() {
            StorageError::Network(format!("连接 {} 超时，请检查网络后重试。", self.host))
        } else if lower.contains("certificate") || lower.contains("cert verif") {
            StorageError::Network(format!(
                "无法验证 {} 的 HTTPS 证书（自签名、已过期或域名不匹配）。请检查地址是否正确；自建服务请配置受信任的证书。",
                self.host
            ))
        } else if e.is_connect() {
            StorageError::Network(format!(
                "无法连接到 {}，请检查地址和网络（如果开着代理软件，也请确认代理可用）。（{detail}）",
                self.host
            ))
        } else {
            StorageError::Network(format!("与 {} 通信失败：{detail}", self.host))
        }
    }

    /// 非预期状态码 → 错误。`action` 是「读取文件」「创建文件夹」这样的动宾短语。
    fn status_error(&self, resp: &Response, action: &str) -> StorageError {
        let status = resp.status();
        let code = status.as_u16();
        match code {
            401 => StorageError::Auth(if self.is_jianguoyun() {
                "坚果云认证失败：密码请填「账户信息 → 安全选项 → 第三方应用管理」里生成的应用密码，不是登录密码。".into()
            } else {
                format!("WebDAV 认证失败（{}）：用户名或密码不正确。Nextcloud 等请使用「应用密码」。", self.host)
            }),
            403 => StorageError::Permission(format!(
                "服务器拒绝了{action}（HTTP 403）：账号在这个位置没有写入权限，或服务器不允许在此操作。{}",
                if self.is_jianguoyun() {
                    "坚果云不允许在 /dav/ 根目录下直接建文件夹，请把同步目录设在已有的同步文件夹里，例如「我的坚果云/marcel-sync」。"
                } else {
                    ""
                }
            )),
            404 => StorageError::Other(format!("{action}失败：服务器上找不到这个位置（HTTP 404），请检查 WebDAV 地址。")),
            405 => StorageError::Other(format!(
                "{action}失败：服务器不支持这个操作（HTTP 405），请确认地址指向的是 WebDAV 服务而不是普通网页。"
            )),
            409 => StorageError::Other(format!(
                "{action}失败：服务器上找不到 WebDAV 地址的上级目录（HTTP 409），请检查地址和同步目录是否正确。"
            )),
            413 => StorageError::Other("文件太大，被服务器拒绝（HTTP 413）。".into()),
            423 => StorageError::Other(format!("{action}失败：目标被服务器锁定（HTTP 423），稍后再试。")),
            429 | 503 => StorageError::Network(format!(
                "服务器暂时拒绝了请求（HTTP {code}）：请求过于频繁或服务繁忙，稍后会自动重试。"
            )),
            507 => StorageError::Other("服务器存储空间不足（HTTP 507）。".into()),
            300..=399 => {
                let target = resp
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .map(|loc| redirect_display(&self.endpoint, loc))
                    .unwrap_or_else(|| "另一个地址".into());
                StorageError::Other(format!(
                    "服务器把请求重定向到了 {target}（HTTP {code}）。请把 WebDAV 地址改成重定向后的地址（常见原因：http 要改成 https）。"
                ))
            }
            500..=599 => StorageError::Network(format!("服务器出错了（HTTP {code}）：{action}失败，稍后会自动重试。")),
            _ => StorageError::Other(format!("{action}失败：服务器返回了意外的状态（HTTP {code}）。")),
        }
    }

    /// 逐级 `MKCOL` 出 `root + rel_dirs`。`known_dirs` 里有的跳过；`force` 时无视缓存（缓存可能
    /// 已经过期：目录被人在服务器上删了）。
    async fn ensure_dirs(&self, rel_dirs: &[&str], force: bool) -> Result<(), StorageError> {
        let chain: Vec<&str> = self
            .root
            .iter()
            .map(String::as_str)
            .chain(rel_dirs.iter().copied())
            .collect();
        for depth in 1..=chain.len() {
            let key = chain[..depth].join("/");
            if !force && self.known_dirs.lock().contains(&key) {
                continue;
            }
            // chain 已经含 root，这里构造 URL 时不能再叠一次 root：直接在端点上拼。
            let mut url = self.endpoint.clone();
            {
                let mut segs = url
                    .path_segments_mut()
                    .expect("http(s) 地址一定可以作为 base");
                segs.pop_if_empty();
                for s in &chain[..depth] {
                    segs.push(s);
                }
                segs.push("");
            }
            let resp = self.exec(self.request("MKCOL", url, META_TIMEOUT)).await?;
            match resp.status().as_u16() {
                // 201 新建；405 = 已存在（规范里 MKCOL 对已有资源就是回 405）。
                200..=299 | 405 => {
                    self.known_dirs.lock().insert(key);
                }
                // 自上而下逐级建，仍然 409 只可能是端点本身 / 它的上级不存在。
                409 => {
                    return Err(StorageError::Other(
                        "服务器上找不到 WebDAV 地址的上级目录（HTTP 409），请检查地址是否正确。"
                            .into(),
                    ))
                }
                _ => return Err(self.status_error(&resp, "创建文件夹")),
            }
        }
        Ok(())
    }

    fn forget_dirs_under(&self, rel: &[&str]) {
        let key = self
            .root
            .iter()
            .map(String::as_str)
            .chain(rel.iter().copied())
            .collect::<Vec<_>>()
            .join("/");
        let prefix = format!("{key}/");
        self.known_dirs
            .lock()
            .retain(|d| *d != key && !d.starts_with(&prefix));
    }

    fn mark_known(&self, rel_dirs: &[&str]) {
        let chain: Vec<&str> = self
            .root
            .iter()
            .map(String::as_str)
            .chain(rel_dirs.iter().copied())
            .collect();
        let mut known = self.known_dirs.lock();
        for depth in 1..=chain.len() {
            known.insert(chain[..depth].join("/"));
        }
    }

    /// 一次 PUT。`Ok(false)` = 服务器说上级目录不存在（409 / 404），其余失败走 `Err`。
    async fn put_once(&self, url: Url, bytes: &[u8]) -> Result<bool, StorageError> {
        let rb = self
            .request("PUT", url, BODY_TIMEOUT)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(bytes.to_vec());
        let resp = self.exec(rb).await?;
        match resp.status().as_u16() {
            200..=299 => Ok(true),
            404 | 409 => Ok(false),
            _ => Err(self.status_error(&resp, "写入文件")),
        }
    }
}

#[async_trait]
impl SyncStorage for WebDavStorage {
    async fn list(&self, dir: &str) -> Result<Vec<RemoteEntry>, StorageError> {
        let rel = rel_segments(dir)?;
        let url = self.url_for(&rel, true);
        let request_path = url_path_segments(url.path());
        let rb = self
            .request("PROPFIND", url, META_TIMEOUT)
            .header("Depth", "1")
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/xml; charset=utf-8",
            )
            .body(PROPFIND_BODY);
        let resp = self.exec(rb).await?;
        match resp.status().as_u16() {
            // 目录不存在 → 空（规格约定）。
            404 | 410 => Ok(Vec::new()),
            200..=299 => {
                let body = resp.bytes().await.map_err(|e| self.net_error(e))?;
                parse_multistatus(&String::from_utf8_lossy(&body), &request_path)
            }
            _ => Err(self.status_error(&resp, "列出目录")),
        }
    }

    async fn get(&self, path: &str) -> Result<Option<Vec<u8>>, StorageError> {
        let rel = rel_segments(path)?;
        if rel.is_empty() {
            return Err(StorageError::Other("同步路径不能为空".into()));
        }
        let resp = self
            .exec(self.request("GET", self.url_for(&rel, false), BODY_TIMEOUT))
            .await?;
        match resp.status().as_u16() {
            200..=299 => {
                let bytes = resp.bytes().await.map_err(|e| self.net_error(e))?;
                Ok(Some(bytes.to_vec()))
            }
            404 | 410 => Ok(None),
            _ => Err(self.status_error(&resp, "读取文件")),
        }
    }

    async fn put(&self, path: &str, bytes: &[u8]) -> Result<(), StorageError> {
        let rel = rel_segments(path)?;
        let Some((_, parent)) = rel.split_last() else {
            return Err(StorageError::Other("同步路径不能为空".into()));
        };
        let url = self.url_for(&rel, false);
        if self.put_once(url.clone(), bytes).await? {
            self.mark_known(parent);
            return Ok(());
        }
        // 上级目录不存在：无视缓存重新逐级建，再写一次。
        self.ensure_dirs(parent, true).await?;
        if self.put_once(url, bytes).await? {
            Ok(())
        } else {
            Err(StorageError::Other(
                "写入失败：服务器说上级目录不存在，但刚刚已经建过它。请检查 WebDAV 地址是否正确。"
                    .into(),
            ))
        }
    }

    async fn delete(&self, path: &str) -> Result<(), StorageError> {
        let rel = rel_segments(path)?;
        if rel.is_empty() {
            return Err(StorageError::Other("不能删除同步根目录本身".into()));
        }
        // 先按文件形式（不带尾斜杠）删：对文件和绝大多数服务器的目录都有效；个别服务器要求
        // 集合 URL 带尾斜杠、会回一个重定向，那就补上斜杠再来一次。
        let mut resp = self
            .exec(self.request("DELETE", self.url_for(&rel, false), META_TIMEOUT))
            .await?;
        if resp.status().is_redirection() {
            resp = self
                .exec(self.request("DELETE", self.url_for(&rel, true), META_TIMEOUT))
                .await?;
        }
        match resp.status().as_u16() {
            200..=299 | 404 | 410 => {
                self.forget_dirs_under(&rel);
                Ok(())
            }
            _ => Err(self.status_error(&resp, "删除")),
        }
    }

    async fn probe(&self) -> Result<(), StorageError> {
        // 1) 确保 root 在（不存在就建）。
        self.ensure_dirs(&[], false).await?;
        // 2) 读：列一次 root（顺便确认 PROPFIND 可用、地址确实是 WebDAV）。
        self.list("").await?;
        // 3) 写 / 读回 / 删一个探针文件。
        super::write_read_delete_probe(self).await
    }
}

// ── 纯函数：错误文案 / 路径 / multistatus 解析 ────────────────────────────

/// 把错误链拼成一行 `a: b: c`（reqwest 的 `Display` 只有最外层那句「error sending request」，
/// 真正的原因在 source 里）。
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut parts = vec![e.to_string()];
    let mut cur = e.source();
    while let Some(s) = cur {
        parts.push(s.to_string());
        cur = s.source();
    }
    parts.join(": ")
}

/// 重定向目标的展示形式：只保留 `scheme://host/path`，去掉账号 / 查询串，相对地址按端点补全。
fn redirect_display(base: &Url, location: &str) -> String {
    match base.join(location) {
        Ok(mut u) => {
            let _ = u.set_username("");
            let _ = u.set_password(None);
            u.set_query(None);
            u.set_fragment(None);
            u.to_string()
        }
        Err(_) => "另一个地址".into(),
    }
}

/// 把 URL 路径（或 `href` 里的路径）拆成**解码后**的段。空段丢掉，所以尾斜杠、重复斜杠无所谓。
fn url_path_segments(path: &str) -> Vec<String> {
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(|s| percent_decode_str(s).decode_utf8_lossy().into_owned())
        .collect()
}

/// `href` 可能是 `/dav/a/`、`https://host/dav/a/`、也可能是没编码的中文路径：统一取出路径段。
fn href_segments(href: &str) -> Vec<String> {
    let href = href.trim();
    if href.contains("://") {
        if let Ok(u) = Url::parse(href) {
            return url_path_segments(u.path());
        }
        // 解析不了的完整 URL（极少见）：手工去掉 scheme://authority。
        let after = href.split_once("://").map(|(_, r)| r).unwrap_or(href);
        let path = after.find('/').map(|i| &after[i..]).unwrap_or("/");
        return url_path_segments(path);
    }
    url_path_segments(href)
}

#[derive(Default)]
struct RawResponse {
    href: String,
    status: String,
    is_collection: bool,
    length: Option<u64>,
}

enum Capture {
    None,
    Href,
    Length,
    Status,
}

struct Multistatus {
    saw_root: bool,
    current: Option<RawResponse>,
    capture: Capture,
    buf: String,
    /// 当前在几层 `propstat` 里：`response` 直接子级的 `status`（整条资源的状态）和 `propstat`
    /// 里的 `status`（某组属性的状态）要分开——后者经常是 404（目录没有 getcontentlength）。
    propstat_depth: u32,
    done: Vec<RawResponse>,
}

impl Multistatus {
    fn start(&mut self, local: &[u8]) {
        match local {
            b"multistatus" => self.saw_root = true,
            b"response" => {
                self.current = Some(RawResponse::default());
                self.propstat_depth = 0;
            }
            b"propstat" => self.propstat_depth += 1,
            b"href" if self.current.is_some() => self.begin(Capture::Href),
            b"getcontentlength" if self.current.is_some() => self.begin(Capture::Length),
            b"status" if self.current.is_some() => self.begin(Capture::Status),
            b"collection" => {
                if let Some(r) = self.current.as_mut() {
                    r.is_collection = true;
                }
            }
            _ => {}
        }
    }

    fn begin(&mut self, c: Capture) {
        self.capture = c;
        self.buf.clear();
    }

    fn end(&mut self, local: &[u8]) {
        match local {
            b"href" => {
                if matches!(self.capture, Capture::Href) {
                    if let Some(r) = self.current.as_mut() {
                        r.href = self.buf.trim().to_string();
                    }
                    self.capture = Capture::None;
                }
            }
            b"getcontentlength" => {
                if matches!(self.capture, Capture::Length) {
                    if let (Some(r), Ok(n)) =
                        (self.current.as_mut(), self.buf.trim().parse::<u64>())
                    {
                        // 同一条资源可能出现两个 getcontentlength（200 与 404 的 propstat 各一个，
                        // 后者是空的）：取第一个能解析成数字的。
                        r.length.get_or_insert(n);
                    }
                    self.capture = Capture::None;
                }
            }
            b"status" => {
                if matches!(self.capture, Capture::Status) {
                    if self.propstat_depth == 0 {
                        if let Some(r) = self.current.as_mut() {
                            r.status = self.buf.trim().to_string();
                        }
                    }
                    self.capture = Capture::None;
                }
            }
            b"propstat" => self.propstat_depth = self.propstat_depth.saturating_sub(1),
            b"response" => {
                if let Some(r) = self.current.take() {
                    self.done.push(r);
                }
            }
            _ => {}
        }
    }

    fn text(&mut self, t: &str) {
        if !matches!(self.capture, Capture::None) {
            self.buf.push_str(t);
        }
    }
}

/// 解析 `PROPFIND Depth:1` 的 multistatus，得到 `request_path`（请求目录自身的解码路径段）的
/// 直接子项。见模块文档里列的服务器差异。
pub(crate) fn parse_multistatus(
    xml: &str,
    request_path: &[String],
) -> Result<Vec<RemoteEntry>, StorageError> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut ms = Multistatus {
        saw_root: false,
        current: None,
        capture: Capture::None,
        buf: String::new(),
        propstat_depth: 0,
        done: Vec::new(),
    };
    loop {
        let ev = reader.read_event().map_err(|e| {
            StorageError::Other(format!(
                "无法解析服务器返回的目录列表（XML 格式有误：{e}）。"
            ))
        })?;
        match ev {
            Event::Start(e) => ms.start(e.local_name().as_ref()),
            Event::Empty(e) => {
                let name = e.local_name();
                ms.start(name.as_ref());
                ms.end(name.as_ref());
            }
            Event::End(e) => ms.end(e.local_name().as_ref()),
            Event::Text(t) => match t.unescape() {
                Ok(s) => ms.text(&s),
                Err(_) => ms.text(&String::from_utf8_lossy(&t)),
            },
            Event::CData(c) => ms.text(&String::from_utf8_lossy(&c)),
            Event::Eof => break,
            _ => {}
        }
    }
    if !ms.saw_root {
        return Err(StorageError::Other(
            "服务器返回的内容不是 WebDAV 目录列表，请确认地址指向的是 WebDAV 服务（而不是网盘的网页）。".into(),
        ));
    }

    // 过滤、分出「自己」与候选子项。
    let mut others: Vec<(Vec<String>, RawResponse)> = Vec::new();
    for r in ms.done {
        if r.status.contains(" 404") {
            continue;
        }
        let segs = href_segments(&r.href);
        let is_self = segs == request_path
            // 个别服务器返回的路径少了挂载前缀（`/dir/` 对应请求的 `/webdav/dir/`）
            || (segs.len() < request_path.len() && request_path.ends_with(&segs));
        if is_self || segs.is_empty() {
            continue;
        }
        others.push((segs, r));
    }
    let n = request_path.len();
    let strict = |segs: &Vec<String>| segs.len() == n + 1 && segs[..n] == *request_path;
    let strict_count = others.iter().filter(|(s, _)| strict(s)).count();
    // 正常情况：全是请求目录的直接子项。忽略 Depth 的服务器会把子孙也列出来 → 只留直接子项；
    // 路径前缀对不上（一个严格匹配的都没有）→ 退而求其次，按最后一段当名字。
    let use_strict = strict_count > 0;

    let mut out = Vec::new();
    for (segs, r) in others {
        if use_strict && !strict(&segs) {
            continue;
        }
        let name = segs.last().cloned().unwrap_or_default();
        if name.is_empty() || is_hidden_name(&name) {
            continue;
        }
        let is_dir = r.is_collection || r.href.trim_end().ends_with('/');
        out.push(RemoteEntry {
            name,
            is_dir,
            size: if is_dir { 0 } else { r.length.unwrap_or(0) },
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
