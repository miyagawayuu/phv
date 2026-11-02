//! Development/production web server: routes `public/` by file name and runs
//! each request in a fresh VM (nothing is shared between requests except the
//! compiled, immutable bytecode).

mod body;
mod multipart;
mod session;

use body::{RespBody, StallGuard};
use bytes::Bytes;
use http_body_util::{BodyExt, Limited};
use hyper::body::Incoming;
use hyper::header::{HeaderName, HeaderValue, CONTENT_TYPE};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use phv_driver::{compile_cached, load_config, run, Compiled, Config, RunOptions};
use phv_vm::value::html_escape;
use phv_vm::{HttpCtx, Limits};
use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;

/// Request bodies larger than this are rejected with 413.
const MAX_BODY: usize = 8 * 1024 * 1024;
/// Static files larger than this are not served (use a CDN or reverse proxy).
const MAX_STATIC: u64 = 64 * 1024 * 1024;
/// Connections handled at the same time; further clients wait in the backlog.
const MAX_CONNECTIONS: usize = 1024;
/// Time to receive the request headers (also the keep-alive idle timeout).
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
/// Time to receive the whole request body.
const BODY_TIMEOUT: Duration = Duration::from_secs(30);
/// Query and form fields accepted per request.
const MAX_FIELDS: usize = 1000;
/// A connection is closed when the client accepts no data for this long.
const WRITE_STALL: Duration = Duration::from_secs(30);

pub struct ServeOptions {
    /// Site directory; pages live in `<root>/public`.
    pub root: PathBuf,
    pub host: String,
    pub port: u16,
    /// Show error details in the browser.
    pub dev: bool,
    pub time_limit: Duration,
}

struct State {
    root: PathBuf,
    public: PathBuf,
    dev: bool,
    time_limit: Duration,
    config: Config,
    sessions: session::Store,
    /// Key for CSRF tokens; new on every start (open forms then need a reload).
    csrf_secret: Vec<u8>,
    csrf_cookie: &'static str,
    cache: Mutex<HashMap<PathBuf, Arc<Compiled>>>,
}

impl State {
    fn new(root: PathBuf, dev: bool, time_limit: Duration, config: Config) -> State {
        let mut secret = vec![0u8; 32];
        getrandom::fill(&mut secret).expect("operating system random number generator failed");
        State {
            public: root.join("public"),
            sessions: session::Store::new(
                Duration::from_secs(config.session_ttl),
                config.secure_cookies,
                config.session_dir.clone(),
            ),
            csrf_cookie: if config.secure_cookies { "__Host-PHVCSRF" } else { "PHVCSRF" },
            csrf_secret: secret,
            config,
            root,
            dev,
            time_limit,
            cache: Mutex::new(HashMap::new()),
        }
    }
}

pub fn serve(opts: ServeOptions) -> Result<(), String> {
    let addr: SocketAddr = format!("{}:{}", opts.host, opts.port)
        .parse()
        .map_err(|e| format!("invalid address {}:{}: {e}", opts.host, opts.port))?;
    let listener = std::net::TcpListener::bind(addr).map_err(|e| format!("cannot listen on {addr}: {e}"))?;
    serve_on(listener, opts)
}

/// Serves on an already bound listener (tests bind port 0).
pub fn serve_on(listener: std::net::TcpListener, opts: ServeOptions) -> Result<(), String> {
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let root = opts.root.canonicalize().map_err(|e| format!("cannot open {}: {e}", opts.root.display()))?;
    if !root.join("public").is_dir() {
        return Err(format!("{} has no `public/` directory", root.display()));
    }
    let config = load_config(&root)?;
    let state = Arc::new(State::new(root, opts.dev, opts.time_limit, config));
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        // Scripts run on blocking threads; give them room for deep recursion.
        .thread_stack_size(16 * 1024 * 1024)
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(async move {
        let listener = TcpListener::from_std(listener).map_err(|e| format!("cannot listen on {addr}: {e}"))?;
        let mode = if state.dev { "development" } else { "production" };
        eprintln!("PHV server ({mode} mode) listening on http://{addr}");
        if state.dev && !addr.ip().is_loopback() {
            eprintln!("warning: development mode shows error details to every visitor; use --prod");
        }
        eprintln!("Serving {}", state.public.display());
        let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
        loop {
            let permit = slots.clone().acquire_owned().await.expect("semaphore is never closed");
            let (stream, peer) = match listener.accept().await {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("accept failed: {e}");
                    continue;
                }
            };
            let state = state.clone();
            tokio::spawn(async move {
                let service = service_fn(move |req| handle(state.clone(), peer, req));
                let _ = http1::Builder::new()
                    .timer(TokioTimer::new())
                    .header_read_timeout(HEADER_TIMEOUT)
                    .serve_connection(TokioIo::new(StallGuard::new(stream, WRITE_STALL)), service)
                    .await;
                drop(permit);
            });
        }
    })
}

type Resp = Response<RespBody>;

async fn handle(state: Arc<State>, peer: SocketAddr, req: Request<Incoming>) -> Result<Resp, Infallible> {
    let start = Instant::now();
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let ip = peer.ip().to_canonical().to_string();
    let limit = state.config.requests_per_minute;
    let client = format!("ip:{}", phv_std::ratelimit::client_key(peer.ip()));
    let minute = Duration::from_secs(60);
    let mut resp = if limit > 0 && !phv_std::ratelimit::hit(&client, limit, minute) {
        let mut r = simple(StatusCode::TOO_MANY_REQUESTS, "text/plain", "too many requests".into());
        let wait = phv_std::ratelimit::retry_after(&client, minute);
        r.headers_mut().insert("retry-after", HeaderValue::from(wait));
        r
    } else {
        route(&state, ip, req).await
    };
    add_security_headers(&state, &mut resp);
    eprintln!("{method} {path} {} {:.1}ms", resp.status().as_u16(), start.elapsed().as_secs_f64() * 1000.0);
    Ok(resp)
}

fn simple(status: StatusCode, content_type: &str, body: String) -> Resp {
    let mut r = Response::new(RespBody::full(body));
    *r.status_mut() = status;
    r.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_str(content_type).unwrap());
    r
}

fn page(status: StatusCode, title: &str, body_html: &str) -> Resp {
    let html = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{t}</title><style>\
         body{{font:15px/1.5 system-ui,sans-serif;margin:2rem auto;max-width:60rem;padding:0 1rem;color:#222}}\
         h1{{font-size:1.3rem;color:#b3261e}}pre{{background:#1d1f21;color:#e6e6e6;padding:1rem;border-radius:6px;overflow:auto;white-space:pre-wrap}}\
         .hint{{color:#666;font-size:.9rem}}</style></head><body><h1>{t}</h1>{body_html}</body></html>",
        t = html_escape(title)
    );
    simple(status, "text/html; charset=utf-8", html)
}

fn not_found() -> Resp {
    page(StatusCode::NOT_FOUND, "404 Not Found", "<p>The requested page does not exist.</p>")
}

fn add_security_headers(state: &State, resp: &mut Resp) {
    let h = resp.headers_mut();
    let mut set = |name: &'static str, value: &str| {
        if !h.contains_key(name) {
            if let Ok(v) = HeaderValue::from_str(value) {
                h.insert(name, v);
            }
        }
    };
    set("x-content-type-options", "nosniff");
    set("x-frame-options", "SAMEORIGIN");
    set("referrer-policy", "strict-origin-when-cross-origin");
    if state.config.secure_cookies {
        set("strict-transport-security", "max-age=31536000");
    }
    // Pages set their own policy with a nonce; everything else gets one without.
    if let Some(csp) = state.config.csp.header(None) {
        set("content-security-policy", &csp);
    }
}

enum Target {
    Script(PathBuf),
    Static(PathBuf),
}

/// Maps a URL path to a file under `public/`, refusing anything that could
/// escape it or reveal source: `..`, hidden (`.x`) and private (`_x`) files,
/// templates, symlinks pointing outside, and any spelling that is not the
/// file's exact name. The last rule matters on Windows and macOS, where
/// `INDEX.PHV`, 8.3 short names (`INDEX~1.PHV`) and `index.phv::$DATA` all
/// open `index.phv`.
fn resolve(state: &State, url_path: &str) -> Option<Target> {
    let decoded = percent_encoding::percent_decode_str(url_path).decode_utf8().ok()?;
    if decoded.contains(['\0', '\\', ':']) {
        return None;
    }
    let segments: Vec<&str> = decoded.split('/').filter(|s| !s.is_empty()).collect();
    if segments.iter().any(|s| s.starts_with('.') || s.starts_with('_')) {
        return None;
    }
    let rel = segments.join("/");
    let mut candidates = Vec::new();
    if rel.is_empty() {
        candidates.push("index.phv".to_string());
    } else if rel.ends_with(".phv") {
        candidates.push(rel.clone());
    } else {
        candidates.push(format!("{rel}.phv"));
        candidates.push(format!("{rel}/index.phv"));
    }
    // The file `rel` names, only if its canonical path inside public/ is spelled exactly `rel`.
    let exact = |rel: &str| -> Option<PathBuf> {
        let c = state.public.join(rel).canonicalize().ok()?;
        let inner = c.strip_prefix(&state.public).ok()?;
        let same = inner.components().map(|x| x.as_os_str().to_str()).eq(rel.split('/').map(Some));
        (same && c.is_file()).then_some(c)
    };
    let name_of = |p: &Path| p.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    for c in candidates {
        if let Some(p) = exact(&c) {
            if !name_of(&p).ends_with(".phv.html") {
                return Some(Target::Script(p));
            }
        }
    }
    if !rel.is_empty() {
        if let Some(p) = exact(&rel) {
            let n = name_of(&p);
            if !n.ends_with(".phv") && !n.ends_with(".phv.html") && content_type(&p).is_some() {
                return Some(Target::Static(p));
            }
        }
    }
    None
}

/// Static files are served only for these extensions, so stray source or
/// secret files (`.php`, `.bak`, `.toml`, `.sql`, ...) in public/ are not exposed.
fn content_type(path: &Path) -> Option<&'static str> {
    Some(match path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase().as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "map" | "json" => "application/json",
        "txt" => "text/plain; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "pdf" => "application/pdf",
        "mp3" => "audio/mpeg",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "wasm" => "application/wasm",
        _ => return None,
    })
}

async fn route(state: &Arc<State>, ip: String, req: Request<Incoming>) -> Resp {
    let target = resolve(state, req.uri().path());
    match target {
        None => not_found(),
        Some(Target::Static(p)) => {
            if req.method() != Method::GET && req.method() != Method::HEAD {
                return simple(StatusCode::METHOD_NOT_ALLOWED, "text/plain", "method not allowed".into());
            }
            let Ok(file) = tokio::fs::File::open(&p).await else { return not_found() };
            let len = match file.metadata().await {
                Ok(m) if m.is_file() => m.len(),
                _ => return not_found(),
            };
            if len > MAX_STATIC {
                return simple(StatusCode::PAYLOAD_TOO_LARGE, "text/plain", "file too large to serve".into());
            }
            // Streamed from the open file: memory does not grow with the file or the number of downloads.
            let mut r = Response::new(RespBody::file(file, len));
            let ct = content_type(&p).unwrap_or("application/octet-stream");
            r.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static(ct));
            r
        }
        Some(Target::Script(p)) => run_script(state, p, ip, req).await,
    }
}

impl State {
    /// Compiles a page, reusing the cached bytecode until a source file changes.
    fn compiled(&self, script: &Path) -> Result<Arc<Compiled>, String> {
        if let Some(c) = self.cache.lock().unwrap().get(script) {
            if !c.is_stale() {
                return Ok(c.clone());
            }
        }
        let mut c = compile_cached(&self.root, script).map_err(|e| e.render(false))?;
        c.enable_jit();
        let c = Arc::new(c);
        if !c.warnings.is_empty() {
            eprint!("{}", c.warnings);
        }
        self.cache.lock().unwrap().insert(script.to_path_buf(), c.clone());
        Ok(c)
    }

    fn error_page(&self, title: &str, detail: &str) -> Resp {
        if self.dev {
            let body = format!(
                "<pre>{}</pre><p class=\"hint\">Shown because the server runs in development mode.</p>",
                html_escape(detail)
            );
            page(StatusCode::INTERNAL_SERVER_ERROR, title, &body)
        } else {
            page(StatusCode::INTERNAL_SERVER_ERROR, "500 Internal Server Error", "<p>Something went wrong.</p>")
        }
    }

    /// Parses the request, checks CSRF, runs the page and builds the response.
    /// Runs on a blocking thread: everything here may take CPU time.
    fn execute(&self, script: &Path, ip: String, parts: hyper::http::request::Parts, body: Bytes) -> Resp {
        let headers: Vec<(String, String)> = parts
            .headers
            .iter()
            .map(|(k, v)| (k.as_str().to_ascii_lowercase(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
            .collect();
        let query: Vec<(String, String)> =
            form_urlencoded::parse(parts.uri.query().unwrap_or("").as_bytes()).into_owned().collect();
        let raw_content_type = parts.headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
        let content_type = raw_content_type.to_ascii_lowercase();
        let mut form = Vec::new();
        let mut files = Vec::new();
        let mut raw_body = true;
        if content_type.starts_with("application/x-www-form-urlencoded") {
            form = form_urlencoded::parse(&body).into_owned().collect();
        } else if let Some(b) = multipart::boundary(raw_content_type) {
            raw_body = false;
            match multipart::parse(&body, &b) {
                Ok((f, up)) => {
                    form = f;
                    files = up;
                }
                Err(e) => return simple(StatusCode::BAD_REQUEST, "text/plain", format!("bad request: {e}")),
            }
        }
        if query.len() > MAX_FIELDS || form.len() > MAX_FIELDS {
            return simple(StatusCode::BAD_REQUEST, "text/plain", "bad request: too many fields".into());
        }
        let cookie_header = parts.headers.get(hyper::header::COOKIE).and_then(|v| v.to_str().ok());
        let session = self.sessions.load(cookie_header);
        let csrf_cookie = session::cookie_value(cookie_header, self.csrf_cookie)
            .filter(|v| v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit()))
            .map(str::to_string);

        // CSRF: form-style requests that change state must carry the token from
        // csrf_field(). Browsers can send these cross-site without asking, unlike
        // JSON requests, which need a CORS preflight.
        let unsafe_method = matches!(parts.method, Method::POST | Method::PUT | Method::PATCH | Method::DELETE);
        let simple_body = content_type.is_empty()
            || content_type.starts_with("application/x-www-form-urlencoded")
            || content_type.starts_with("multipart/form-data")
            || content_type.starts_with("text/plain");
        if self.config.csrf && unsafe_method && simple_body {
            let expected = csrf_cookie.as_ref().map(|c| phv_std::csrf_expected(&self.csrf_secret, c));
            let submitted = form
                .iter()
                .find(|(k, _)| k == "_csrf")
                .map(|(_, v)| v.as_str())
                .or_else(|| parts.headers.get("x-csrf-token").and_then(|v| v.to_str().ok()));
            let ok = matches!((&expected, submitted), (Some(e), Some(s)) if phv_std::constant_time_eq(e.as_bytes(), s.as_bytes()));
            if !ok {
                let hint = if self.dev {
                    "<p class=\"hint\">Add <code>&lt;?= csrf_field() ?&gt;</code> inside the form, or send the token from \
                     <code>csrf_token()</code> in an <code>X-CSRF-Token</code> header. \
                     (It can be turned off with <code>[security] csrf = false</code> in phv.toml.)</p>"
                } else {
                    ""
                };
                let body = format!(
                    "<p>The form has expired or did not come from this site. Reload the page and try again.</p>{hint}"
                );
                return page(StatusCode::FORBIDDEN, "403 Forbidden", &body);
            }
        }

        let had_session = session.id.is_some();
        let ctx = HttpCtx {
            method: parts.method.as_str().to_string(),
            path: percent_encoding::percent_decode_str(parts.uri.path()).decode_utf8_lossy().into_owned(),
            query,
            form,
            headers,
            files,
            session,
            csrf_secret: self.csrf_secret.clone(),
            csrf_cookie,
            remote_ip: ip,
            csp_nonce: self.config.csp.uses_nonce().then(|| phv_std::random_hex(16)),
            body: if raw_body { String::from_utf8_lossy(&body).into_owned() } else { String::new() },
            ..HttpCtx::default()
        };
        drop(body);

        let rel = script.strip_prefix(&self.root).unwrap_or(script).to_string_lossy().replace('\\', "/");
        let compiled = match self.compiled(script) {
            Ok(c) => c,
            Err(diagnostics) => {
                eprintln!("compile error in {rel}:\n{diagnostics}");
                return self.error_page("Compile error", &format!("{rel} could not be compiled.\n\n{diagnostics}"));
            }
        };
        let limits = Limits {
            max_time: Some(self.time_limit),
            max_memory: Some(self.config.memory_limit_mb as usize * 1024 * 1024),
            ..Limits::default()
        };
        let host = Some(self.config.host.clone());
        let outcome = run(&compiled, RunOptions { html: true, limits, http: Some(ctx), host });
        if let Some(err) = &outcome.error {
            let text = err.render();
            eprintln!("runtime error in {rel}:\n{text}");
            return self.error_page("Runtime error", &text);
        }

        let mut http = outcome.http;
        let csp = self.config.csp.header(http.csp_nonce.as_deref());
        let flags = if self.config.secure_cookies { "; Secure" } else { "" };
        let session = std::mem::take(&mut http.session);
        let touched_session = had_session || session.changed;
        if let Some(cookie) = self.sessions.save(session) {
            http.resp_headers.push(("Set-Cookie".into(), cookie));
        }
        if let Some(c) = &http.new_csrf_cookie {
            http.resp_headers.push((
                "Set-Cookie".into(),
                format!("{}={c}; Path=/; HttpOnly; SameSite=Lax{flags}", self.csrf_cookie),
            ));
        }
        let status = StatusCode::from_u16(http.status).unwrap_or(StatusCode::OK);
        let body = if matches!(status, StatusCode::NO_CONTENT | StatusCode::NOT_MODIFIED) {
            RespBody::full(Bytes::new())
        } else if let Some(file) = http.body_file.take() {
            // The file the script opened (and the checks passed for) is streamed as it is sent.
            match std::sync::Arc::try_unwrap(file).map_err(|f| f.try_clone()) {
                Ok(f) | Err(Ok(f)) => {
                    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
                    RespBody::file(tokio::fs::File::from_std(f), len)
                }
                Err(Err(_)) => RespBody::full(Bytes::new()),
            }
        } else {
            RespBody::full(outcome.output)
        };
        let mut r = Response::new(body);
        *r.status_mut() = status;
        let ct = http.content_type.unwrap_or_else(|| "text/html; charset=utf-8".into());
        let h = r.headers_mut();
        if let Ok(v) = HeaderValue::from_str(&ct) {
            h.insert(CONTENT_TYPE, v);
        }
        let mut sets_cookie = false;
        for (name, value) in http.resp_headers {
            let lower = name.to_ascii_lowercase();
            // Framing headers belong to the server (checked in the builtin too).
            if ["content-length", "transfer-encoding", "connection", "keep-alive", "upgrade", "te", "trailer"]
                .contains(&lower.as_str())
            {
                continue;
            }
            sets_cookie |= lower == "set-cookie";
            if let (Ok(n), Ok(v)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(&value)) {
                h.append(n, v);
            }
        }
        if let Some(v) = csp.and_then(|c| HeaderValue::from_str(&c).ok()) {
            if !h.contains_key("content-security-policy") {
                h.insert("content-security-policy", v);
            }
        }
        // Personalized responses must not be stored by shared caches: those that use the
        // session or cookies, or carry a CSRF token (it is tied to the visitor's cookie).
        if (touched_session || sets_cookie || http.personalized) && !h.contains_key("cache-control") {
            h.insert("cache-control", HeaderValue::from_static("private, no-store"));
        }
        r
    }
}

async fn run_script(state: &Arc<State>, script: PathBuf, ip: String, req: Request<Incoming>) -> Resp {
    let (parts, body) = req.into_parts();
    let body = match tokio::time::timeout(BODY_TIMEOUT, Limited::new(body, MAX_BODY).collect()).await {
        Ok(Ok(b)) => b.to_bytes(),
        Ok(Err(_)) => return simple(StatusCode::PAYLOAD_TOO_LARGE, "text/plain", "request body too large".into()),
        Err(_) => return simple(StatusCode::REQUEST_TIMEOUT, "text/plain", "request body timed out".into()),
    };
    let st = state.clone();
    match tokio::task::spawn_blocking(move || st.execute(&script, ip, parts, body)).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("internal error: {e}");
            page(StatusCode::INTERNAL_SERVER_ERROR, "500 Internal Server Error", "<p>Something went wrong.</p>")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_stay_inside_public() {
        let dir = std::env::temp_dir().join(format!("phv-route-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("public/blog")).unwrap();
        std::fs::create_dir_all(dir.join("public/.git")).unwrap();
        std::fs::write(dir.join("public/index.phv"), "echo 1;").unwrap();
        std::fs::write(dir.join("public/dashboardpage.phv"), "echo 1;").unwrap();
        std::fs::write(dir.join("public/blog/post.phv"), "echo 1;").unwrap();
        std::fs::write(dir.join("public/_partial.phv"), "echo 1;").unwrap();
        std::fs::write(dir.join("public/layout.phv.html"), "x").unwrap();
        std::fs::write(dir.join("public/style.css"), "body{}").unwrap();
        std::fs::write(dir.join("public/config.php"), "<?php $pw = 1;").unwrap();
        std::fs::write(dir.join("public/backup.sql"), "x").unwrap();
        std::fs::write(dir.join("public/.env"), "SECRET=1").unwrap();
        std::fs::write(dir.join("public/.git/config"), "x").unwrap();
        std::fs::write(dir.join("secret.phv"), "echo 1;").unwrap();
        let root = dir.canonicalize().unwrap();
        let st = State::new(root.clone(), true, Duration::from_secs(5), Config::default_for(&root));
        assert!(matches!(resolve(&st, "/"), Some(Target::Script(_))));
        assert!(matches!(resolve(&st, "/blog/post"), Some(Target::Script(_))));
        assert!(matches!(resolve(&st, "/blog/post.phv"), Some(Target::Script(_))));
        assert!(matches!(resolve(&st, "/style.css"), Some(Target::Static(_))));
        for bad in [
            "/.env",
            "/../secret",
            "/%2e%2e/secret",
            "/blog/..%2f..%2fsecret",
            "/missing",
            "/_partial",
            "/layout.phv.html",
            // Windows aliases of index.phv / templates / dotfiles must not be served as files.
            "/INDEX.PHV",
            "/index.Phv",
            "/index.phv::$DATA",
            "/index.phv%3A%3A%24DATA",
            "/DASHBO~1.PHV",
            "/LAYOUT.PHV.HTML",
            "/ENV~1",
            "/GIT~1/config",
            "/Style.CSS",
            "/config.php",
            "/backup.sql",
        ] {
            assert!(resolve(&st, bad).is_none(), "{bad} must not resolve");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
