//! Development/production web server: routes `public/` by file name and runs
//! each request in a fresh VM (nothing is shared between requests except the
//! compiled, immutable bytecode).

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::header::{HeaderName, HeaderValue, CONTENT_TYPE};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use phenix_driver::{compile, run, Compiled, RunOptions};
use phenix_vm::value::html_escape;
use phenix_vm::{HttpCtx, Limits};
use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;

/// Request bodies larger than this are rejected with 413.
const MAX_BODY: usize = 8 * 1024 * 1024;

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
    cache: Mutex<HashMap<PathBuf, Arc<Compiled>>>,
}

pub fn serve(opts: ServeOptions) -> Result<(), String> {
    let root = opts.root.canonicalize().map_err(|e| format!("cannot open {}: {e}", opts.root.display()))?;
    let public = root.join("public");
    if !public.is_dir() {
        return Err(format!("{} has no `public/` directory", root.display()));
    }
    let addr: SocketAddr = format!("{}:{}", opts.host, opts.port)
        .parse()
        .map_err(|e| format!("invalid address {}:{}: {e}", opts.host, opts.port))?;
    let state = Arc::new(State {
        root,
        public,
        dev: opts.dev,
        time_limit: opts.time_limit,
        cache: Mutex::new(HashMap::new()),
    });
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        // Scripts run on blocking threads; give them room for deep recursion.
        .thread_stack_size(16 * 1024 * 1024)
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(async move {
        let listener = TcpListener::bind(addr).await.map_err(|e| format!("cannot listen on {addr}: {e}"))?;
        let mode = if state.dev { "development" } else { "production" };
        eprintln!("Phenix server ({mode} mode) listening on http://{addr}");
        eprintln!("Serving {}", state.public.display());
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("accept failed: {e}");
                    continue;
                }
            };
            let state = state.clone();
            tokio::spawn(async move {
                let service = service_fn(move |req| handle(state.clone(), req));
                let _ = http1::Builder::new().serve_connection(TokioIo::new(stream), service).await;
            });
        }
    })
}

type Resp = Response<Full<Bytes>>;

async fn handle(state: Arc<State>, req: Request<Incoming>) -> Result<Resp, Infallible> {
    let start = Instant::now();
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let mut resp = route(&state, req).await;
    add_security_headers(&mut resp);
    eprintln!("{method} {path} {} {:.1}ms", resp.status().as_u16(), start.elapsed().as_secs_f64() * 1000.0);
    Ok(resp)
}

fn simple(status: StatusCode, content_type: &str, body: String) -> Resp {
    let mut r = Response::new(Full::new(Bytes::from(body)));
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

fn add_security_headers(resp: &mut Resp) {
    let h = resp.headers_mut();
    for (name, value) in [
        ("x-content-type-options", "nosniff"),
        ("x-frame-options", "SAMEORIGIN"),
        ("referrer-policy", "strict-origin-when-cross-origin"),
    ] {
        if !h.contains_key(name) {
            h.insert(name, HeaderValue::from_static(value));
        }
    }
}

enum Target {
    Script(PathBuf),
    Static(PathBuf),
}

/// Maps a URL path to a file under `public/`, refusing anything that could
/// escape it (`..`, hidden files, symlinks pointing outside).
fn resolve(state: &State, url_path: &str) -> Option<Target> {
    let decoded = percent_encoding::percent_decode_str(url_path).decode_utf8().ok()?;
    if decoded.contains('\0') || decoded.contains('\\') {
        return None;
    }
    let segments: Vec<&str> = decoded.split('/').filter(|s| !s.is_empty()).collect();
    if segments.iter().any(|s| s.starts_with('.')) {
        return None;
    }
    let rel = segments.join("/");
    if rel.ends_with(".phx.html") {
        return None;
    }
    let mut candidates = Vec::new();
    if rel.is_empty() {
        candidates.push("index.phx".to_string());
    } else if rel.ends_with(".phx") {
        candidates.push(rel.clone());
    } else {
        candidates.push(format!("{rel}.phx"));
        candidates.push(format!("{rel}/index.phx"));
    }
    let inside = |p: &Path| -> Option<PathBuf> {
        let c = p.canonicalize().ok()?;
        (c.starts_with(&state.public) && c.is_file()).then_some(c)
    };
    for c in candidates {
        if let Some(p) = inside(&state.public.join(&c)) {
            return Some(Target::Script(p));
        }
    }
    if !rel.is_empty() {
        if let Some(p) = inside(&state.public.join(&rel)) {
            return Some(Target::Static(p));
        }
    }
    None
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase().as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "txt" => "text/plain; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

async fn route(state: &Arc<State>, req: Request<Incoming>) -> Resp {
    let target = resolve(state, req.uri().path());
    match target {
        None => not_found(),
        Some(Target::Static(p)) => {
            if req.method() != Method::GET && req.method() != Method::HEAD {
                return simple(StatusCode::METHOD_NOT_ALLOWED, "text/plain", "method not allowed".into());
            }
            match tokio::fs::read(&p).await {
                Ok(bytes) => {
                    let mut r = Response::new(Full::new(Bytes::from(bytes)));
                    r.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static(content_type(&p)));
                    r
                }
                Err(_) => not_found(),
            }
        }
        Some(Target::Script(p)) => run_script(state, p, req).await,
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
        let c = Arc::new(compile(&self.root, script).map_err(|e| e.render(false))?);
        if !c.warnings.is_empty() {
            eprint!("{}", c.warnings);
        }
        self.cache.lock().unwrap().insert(script.to_path_buf(), c.clone());
        Ok(c)
    }
}

async fn run_script(state: &Arc<State>, script: PathBuf, req: Request<Incoming>) -> Resp {
    let (parts, body) = req.into_parts();
    let body = match Limited::new(body, MAX_BODY).collect().await {
        Ok(b) => b.to_bytes(),
        Err(_) => return simple(StatusCode::PAYLOAD_TOO_LARGE, "text/plain", "request body too large".into()),
    };
    let headers: Vec<(String, String)> = parts
        .headers
        .iter()
        .map(|(k, v)| (k.as_str().to_ascii_lowercase(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
        .collect();
    let query = form_urlencoded::parse(parts.uri.query().unwrap_or("").as_bytes()).into_owned().collect();
    let is_form = parts
        .headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.to_ascii_lowercase().starts_with("application/x-www-form-urlencoded"));
    let form = if is_form { form_urlencoded::parse(&body).into_owned().collect() } else { Vec::new() };
    let ctx = HttpCtx {
        method: parts.method.as_str().to_string(),
        path: percent_encoding::percent_decode_str(parts.uri.path()).decode_utf8_lossy().into_owned(),
        query,
        form,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
        ..HttpCtx::default()
    };

    let st = state.clone();
    let rel = script.strip_prefix(&state.root).unwrap_or(&script).to_string_lossy().replace('\\', "/");
    let result = tokio::task::spawn_blocking(move || {
        let compiled = st.compiled(&script)?;
        let limits = Limits { max_time: Some(st.time_limit), ..Limits::default() };
        Ok::<_, String>(run(&compiled, RunOptions { html: true, limits, http: Some(ctx) }))
    })
    .await;

    let outcome = match result {
        Ok(Ok(o)) => o,
        Ok(Err(diagnostics)) => {
            eprintln!("compile error in {rel}:\n{diagnostics}");
            return if state.dev {
                let body = format!(
                    "<p>{} could not be compiled.</p><pre>{}</pre><p class=\"hint\">Shown because the server runs in development mode.</p>",
                    html_escape(&rel),
                    html_escape(&diagnostics)
                );
                page(StatusCode::INTERNAL_SERVER_ERROR, "Compile error", &body)
            } else {
                page(StatusCode::INTERNAL_SERVER_ERROR, "500 Internal Server Error", "<p>Something went wrong.</p>")
            };
        }
        Err(e) => {
            eprintln!("internal error in {rel}: {e}");
            return page(StatusCode::INTERNAL_SERVER_ERROR, "500 Internal Server Error", "<p>Something went wrong.</p>");
        }
    };

    if let Some(err) = &outcome.error {
        let text = err.render();
        eprintln!("runtime error in {rel}:\n{text}");
        return if state.dev {
            let body = format!(
                "<pre>{}</pre><p class=\"hint\">Shown because the server runs in development mode.</p>",
                html_escape(&text)
            );
            page(StatusCode::INTERNAL_SERVER_ERROR, "Runtime error", &body)
        } else {
            page(StatusCode::INTERNAL_SERVER_ERROR, "500 Internal Server Error", "<p>Something went wrong.</p>")
        };
    }

    let http = outcome.http;
    let mut r = Response::new(Full::new(Bytes::from(outcome.output)));
    *r.status_mut() = StatusCode::from_u16(http.status).unwrap_or(StatusCode::OK);
    let ct = http.content_type.unwrap_or_else(|| "text/html; charset=utf-8".into());
    let h = r.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&ct) {
        h.insert(CONTENT_TYPE, v);
    }
    for (name, value) in http.resp_headers {
        if let (Ok(n), Ok(v)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(&value)) {
            h.append(n, v);
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(dir: &Path) -> State {
        let root = dir.canonicalize().unwrap();
        State {
            public: root.join("public"),
            root,
            dev: true,
            time_limit: Duration::from_secs(5),
            cache: Mutex::new(HashMap::new()),
        }
    }

    #[test]
    fn routes_stay_inside_public() {
        let dir = std::env::temp_dir().join(format!("phenix-route-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("public/blog")).unwrap();
        std::fs::write(dir.join("public/index.phx"), "echo 1;").unwrap();
        std::fs::write(dir.join("public/blog/post.phx"), "echo 1;").unwrap();
        std::fs::write(dir.join("public/style.css"), "body{}").unwrap();
        std::fs::write(dir.join("public/.env"), "SECRET=1").unwrap();
        std::fs::write(dir.join("secret.phx"), "echo 1;").unwrap();
        let st = state(&dir);
        assert!(matches!(resolve(&st, "/"), Some(Target::Script(_))));
        assert!(matches!(resolve(&st, "/blog/post"), Some(Target::Script(_))));
        assert!(matches!(resolve(&st, "/blog/post.phx"), Some(Target::Script(_))));
        assert!(matches!(resolve(&st, "/style.css"), Some(Target::Static(_))));
        assert!(resolve(&st, "/.env").is_none());
        assert!(resolve(&st, "/../secret").is_none());
        assert!(resolve(&st, "/%2e%2e/secret").is_none());
        assert!(resolve(&st, "/blog/..%2f..%2fsecret").is_none());
        assert!(resolve(&st, "/missing").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
