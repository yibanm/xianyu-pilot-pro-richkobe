//! 本机统一网关：替代原架构里两个 nginx 容器。
//!
//! 复刻 apps/user-web/nginx.conf 与 apps/admin-web/nginx.conf 的语义，
//! 监听两个端口（user-web / admin-web）—— 因为两个前端的 Vite base 都是 `/`，
//! assets 引用为 `/assets/*`，无法共用一个端口的子路径。

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use bytes::Bytes;
use futures_util::StreamExt;
use tokio::sync::Mutex;

/// 一个前端站点的网关配置。
#[derive(Clone)]
pub struct SiteConfig {
    /// 静态资源根目录（Vite dist）
    pub root: PathBuf,
    /// 反代规则：路径前缀 → 后端 origin，按前缀长度降序匹配（nginx 的 ^~ 语义）
    pub proxies: Vec<ProxyRule>,
    /// 站点标识，仅用于日志
    pub name: &'static str,
}

#[derive(Clone)]
pub struct ProxyRule {
    /// 请求路径前缀，例如 "/api/"、"/uploads/"
    pub prefix: &'static str,
    /// 转发时是否剥掉该前缀。nginx `proxy_pass http://x/admin-api/;` 会剥，
    /// `proxy_pass http://x;` 不剥 —— 两种都要支持。
    pub strip_prefix: bool,
    /// 后端 origin，例如 "http://127.0.0.1:54321"
    pub target: Arc<Mutex<String>>,
    /// 读超时：SSE 需要 3600s，普通 API 300s，admin-api 60s（均为 compose/nginx 原值）
    pub read_timeout: Duration,
    /// 路径重写：/uploads/images/carousel_x → /uploads/images/tenant-1/carousel_x
    pub carousel_rewrite: bool,
}

impl SiteConfig {
    /// 按 nginx 的最长前缀优先规则找匹配项。
    fn match_proxy(&self, path: &str) -> Option<&ProxyRule> {
        self.proxies
            .iter()
            .filter(|r| path.starts_with(r.prefix))
            .max_by_key(|r| r.prefix.len())
    }
}

/// 安全响应头，逐条对齐两份 nginx.conf。
///
/// 与原配置的**唯一差异**：去掉了 `upgrade-insecure-requests`（CSP）与
/// `Strict-Transport-Security`。原因：网关是本机 HTTP，保留这两项会让 WebView2
/// 把请求升级到 HTTPS 从而全部失败。其余头原样保留。
fn security_headers(is_uploads: bool) -> [(HeaderName, HeaderValue); 5] {
    // uploads 路径的 CSP 收紧为 default-src 'none'（nginx 原配置如此，
    // 防止用户上传的 HTML/SVG 在本机源下执行脚本）
    let csp = if is_uploads {
        "default-src 'none'; frame-ancestors 'none'; sandbox"
    } else {
        "default-src 'self'; base-uri 'self'; object-src 'none'; frame-ancestors 'none'; \
         form-action 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
         img-src 'self' data: blob: https:; font-src 'self' data:; connect-src 'self' https: wss:"
    };
    [
        (
            header::HeaderName::from_static("x-content-type-options"),
            HeaderValue::from_static("nosniff"),
        ),
        (
            header::HeaderName::from_static("x-frame-options"),
            HeaderValue::from_static("DENY"),
        ),
        (
            header::HeaderName::from_static("referrer-policy"),
            HeaderValue::from_static("strict-origin-when-cross-origin"),
        ),
        (
            header::HeaderName::from_static("permissions-policy"),
            HeaderValue::from_static("camera=(), microphone=(), geolocation=(), payment=()"),
        ),
        (
            header::HeaderName::from_static("content-security-policy"),
            HeaderValue::from_static(csp),
        ),
    ]
}

/// 请求体上限 20MB（nginx `client_max_body_size 20m`）。
const MAX_BODY_BYTES: usize = 20 * 1024 * 1024;

async fn handle(
    State(cfg): State<SiteConfig>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let path = uri.path().to_string();

    // 1) 反代优先（对应 nginx 的 location ^~ /api/ 等）
    if let Some(rule) = cfg.match_proxy(&path) {
        return proxy(rule, method, uri, headers, body).await;
    }

    // 2) 静态资源
    serve_static(&cfg, &path, &headers).await
}

/// 静态文件服务，复刻 nginx 的 try_files / expires / gzip_static 语义。
async fn serve_static(cfg: &SiteConfig, path: &str, headers: &HeaderMap) -> Response {
    let is_uploads = false;

    // 防目录穿越：规范化后必须仍在 root 内。
    // nginx 由 try_files 天然免疫，手写实现必须显式校验。
    let relative = path.trim_start_matches('/');
    let candidate = cfg.root.join(if relative.is_empty() { "." } else { relative });
    let resolved = match normalize_within_root(&candidate, &cfg.root) {
        Some(p) => p,
        None => {
                return (
                    StatusCode::FORBIDDEN,
                    [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                    "403 Forbidden",
                )
                    .into_response()
            }
    };

    // 目录 → index.html
    let mut file_path = resolved;
    if file_path.is_dir() {
        file_path = file_path.join("index.html");
    }

    // SPA fallback：文件不存在则回 index.html（nginx `try_files $uri $uri/ /index.html`）。
    // 但 /assets/* 与带静态扩展名的路径不 fallback（nginx 里它们是 `try_files $uri =404`），
    // 否则前端资源 404 会被静默替换成 HTML，表现为「白屏且无报错」，极难排查。
    let no_fallback = path.starts_with("/assets/") || has_static_ext(path);
    if !file_path.is_file() {
        if no_fallback {
            return not_found();
        }
        file_path = cfg.root.join("index.html");
        if !file_path.is_file() {
            return not_found();
        }
    }

    // MIME 必须在 file_path 被移动进下面的 gzip 分支之前算好，
    // 且要按原文件名推断——用 .gz 会推成 application/gzip，浏览器拒绝执行。
    let mime = mime_guess::from_path(&file_path)
        .first_or_octet_stream()
        .to_string();

    // gzip_static on：优先返回构建期预压缩的 .gz（vite-plugin-compression 产物）
    let accepts_gzip = headers
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("gzip"))
        .unwrap_or(false);

    let (send_path, gzipped) = if accepts_gzip {
        let gz = {
            let mut s = file_path.clone().into_os_string();
            s.push(".gz");
            PathBuf::from(s)
        };
        if gz.is_file() { (gz, true) } else { (file_path, false) }
    } else {
        (file_path, false)
    };

    let bytes = match tokio::fs::read(&send_path).await {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("读取文件失败: {e}"),
            )
                .into_response()
        }
    };

    // 缓存策略对齐 nginx：/assets/* 一年 immutable；其他静态扩展名 1 小时；
    // index.html 不缓存（expires -1）
    let cache = if path.starts_with("/assets/") {
        "public, max-age=31536000, immutable"
    } else if has_static_ext(path) {
        "public, max-age=3600, must-revalidate"
    } else {
        "no-store"
    };

    let mut resp_headers = HeaderMap::new();
    resp_headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&mime).unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    resp_headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache),
    );
    if gzipped {
        resp_headers.insert(
            header::CONTENT_ENCODING,
            HeaderValue::from_static("gzip"),
        );
        // 预压缩文件的长度与原文不一致，必须让 HTTP 层重新计算或省略
        resp_headers.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    }
    for (k, v) in security_headers(is_uploads) {
        resp_headers.insert(k, v);
    }

    (StatusCode::OK, resp_headers, bytes).into_response()
}

/// 反代到后端服务。流式转发，不缓冲 —— SSE 依赖这一点。
async fn proxy(
    rule: &ProxyRule,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let target = rule.target.lock().await.clone();
    if target.is_empty() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "后端服务尚未就绪",
        )
            .into_response();
    }

    // 组装转发路径
    let orig_path = uri.path();
    let mut forward_path = if rule.carousel_rewrite && orig_path.starts_with("/uploads/images/carousel_") {
        // nginx: rewrite ^/uploads/images/(carousel_.+)$ /uploads/images/tenant-1/$1 break;
        let tail = &orig_path["/uploads/images/".len()..];
        format!("/uploads/images/tenant-1/{tail}")
    } else if rule.strip_prefix && orig_path.starts_with(rule.prefix) {
        let rest = &orig_path[rule.prefix.trim_end_matches('/').len()..];
        if rest.is_empty() || rest.starts_with('/') {
            rest.to_string()
        } else {
            format!("/{rest}")
        }
    } else {
        orig_path.to_string()
    };
    if forward_path.is_empty() {
        forward_path = "/".to_string();
    }
    if let Some(q) = uri.query() {
        forward_path.push('?');
        forward_path.push_str(q);
    }

    let url = format!("{target}{forward_path}");

    // 过滤转发头：剔除 hop-by-hop 与 Host（reqwest 会按目标 URL 重设 Host）
    let mut fwd_headers = HeaderMap::new();
    for (k, v) in headers.iter() {
        let name = k.as_str();
        if matches!(
            name,
            "host" | "connection" | "keep-alive" | "proxy-authenticate" | "proxy-authorization"
                | "te" | "trailer" | "transfer-encoding" | "upgrade" | "content-length"
        ) {
            continue;
        }
        fwd_headers.append(k.clone(), v.clone());
    }
    // 复刻 nginx 的 X-Forwarded-* 与 X-Real-IP
    fwd_headers.insert(
        HeaderName::from_static("x-real-ip"),
        HeaderValue::from_static("127.0.0.1"),
    );
    fwd_headers.insert(
        HeaderName::from_static("x-forwarded-for"),
        HeaderValue::from_static("127.0.0.1"),
    );
    fwd_headers.insert(
        HeaderName::from_static("x-forwarded-proto"),
        HeaderValue::from_static("http"),
    );
    fwd_headers.insert(
        HeaderName::from_static("x-forwarded-host"),
        HeaderValue::from_static("127.0.0.1"),
    );

    // 读取请求体（受 20MB 上限约束）
    let body_bytes = match collect_body(body).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };

    let client = match reqwest::Client::builder()
        // 不设总超时：SSE 是长连接，由 read_timeout 控制单次读等待
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(rule.read_timeout)
        // 上游是本机服务，无需重定向跟随
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    };

    // 用 reqwest 自己的 RequestBuilder（不是 http::Request）：
    // execute() 只接受 reqwest::Request，混用会类型不匹配。
    let upstream_resp = match client
        .request(method, &url)
        .headers(fwd_headers)
        .body(body_bytes)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("后端 {url} 不可达: {e}"),
            )
                .into_response()
        }
    };

    // 回传状态与响应头（同样剔除 hop-by-hop 与 Content-Encoding/Length，
    // 由 axum 按实际 body 重新计算）
    let status = StatusCode::from_u16(upstream_resp.status().as_u16())
        .unwrap_or(StatusCode::BAD_GATEWAY);
    let mut resp = Response::builder().status(status);
    let rh = resp.headers_mut().expect("response builder headers");
    for (k, v) in upstream_resp.headers().iter() {
        let name = k.as_str();
        if matches!(
            name,
            "connection" | "keep-alive" | "transfer-encoding" | "content-length"
                | "content-encoding"
        ) {
            continue;
        }
        rh.append(k.clone(), v.clone());
    }
    let is_uploads = uri.path().starts_with("/uploads/");
    for (k, v) in security_headers(is_uploads) {
        // 后端已给的同名头以网关为准（与 nginx add_header always 一致）
        rh.insert(k, v);
    }

    // 流式转发：SSE 场景下 reqwest 的 bytes_stream 不缓冲，事件到达即下发
    let stream = upstream_resp.bytes_stream().map(|chunk| match chunk {
        Ok(b) => Ok::<Bytes, std::io::Error>(b),
        Err(e) => Err(std::io::Error::new(std::io::ErrorKind::Other, e)),
    });

    match resp.body(Body::from_stream(stream)) {
        Ok(r) => r,
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    }
}

async fn collect_body(body: Body) -> std::result::Result<Bytes, Response> {
    let mut buf = Vec::new();
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| {
            (StatusCode::BAD_REQUEST, format!("读取请求体失败: {e}")).into_response()
        })?;
        if buf.len() + chunk.len() > MAX_BODY_BYTES {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("请求体超过 {MAX_BODY_BYTES} 字节上限"),
            )
                .into_response());
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(buf))
}

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "404 Not Found",
    )
        .into_response()
}

fn has_static_ext(path: &str) -> bool {
    const EXTS: &[&str] = &[
        ".js", ".css", ".woff", ".woff2", ".png", ".jpg", ".jpeg", ".gif", ".svg", ".webp",
        ".ico", ".map", ".br", ".gz",
    ];
    let lower = path.to_ascii_lowercase();
    let lower = lower.split('?').next().unwrap_or(&lower);
    EXTS.iter().any(|e| lower.ends_with(e))
}

/// 规范化路径并确认仍在 root 内，防目录穿越。
///
/// 返回 None 表示试图越界（如 `/../secret`）。
fn normalize_within_root(candidate: &Path, root: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for comp in candidate.components() {
        use std::path::Component;
        match comp {
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    // root 自身必须是 out 的前缀
    let root_str = root.to_string_lossy().replace('/', "\\").to_lowercase();
    let out_str = out.to_string_lossy().replace('/', "\\").to_lowercase();
    if out_str.starts_with(&root_str) {
        Some(out)
    } else {
        None
    }
}

/// 启动一个站点网关，返回实际监听地址。
pub async fn serve(cfg: SiteConfig, port: u16) -> Result<SocketAddr> {
    let app = Router::new()
        .route("/{*rest}", any(handle))
        .route("/", any(handle))
        .with_state(cfg.clone());

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("网关端口 {port} 绑定失败"))?;
    let local = listener.local_addr()?;

    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            eprintln!("[gateway:{}] 服务异常退出: {e}", cfg.name);
        }
    });

    Ok(local)
}
