//! Development/production web server: routes `public/` by file name and runs
//! each request in a fresh VM (nothing is shared between requests except the
//! compiled, immutable bytecode).

mod body;
mod multipart;
mod routes;
mod session;
mod static_files;
mod tls;

use body::{RespBody, StallGuard};
use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::header::{HeaderName, HeaderValue, CONTENT_TYPE};
use hyper::server::conn::{http1, http2};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use phv_driver::config::LogFormat;
use phv_driver::{compile_cached, load_config, run, Compiled, Config, RunOptions};
use phv_vm::value::html_escape;
use phv_vm::{HttpCtx, Limits};
use static_files::Validators;
use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::OwnedSemaphorePermit;

/// Request bodies larger than this are rejected with 413.
const MAX_BODY: usize = 8 * 1024 * 1024;
/// The first bytes of a body that are read without a share of the body budget.
const SMALL_BODY: usize = 64 * 1024;
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
/// Time a request waits for a free worker before it gets 503.
const QUEUE_WAIT: Duration = Duration::from_secs(10);
/// Static files larger than this are sent uncompressed.
const MAX_STATIC_COMPRESS: u64 = 1 << 20;
/// Bytes of compressed static files kept, so each version is compressed once.
const GZ_CACHE: usize = 32 << 20;
/// A connection's read and write buffers (hyper's default is about 400 KB):
/// a client that stops reading holds at most this much (and a file's 64 KB).
const CONN_BUFFER: usize = 64 * 1024;
/// Requests one HTTP/2 connection may have open at once.
const H2_STREAMS: u32 = 64;
/// HTTP/2 flow control: what a client may send before the server reads it,
/// on each stream and on the whole connection (as `CONN_BUFFER` for HTTP/1).
const H2_WINDOW: u32 = 64 * 1024;
/// An HTTP/2 connection without a request for this long is closed (as an
/// idle HTTP/1 connection is by `HEADER_TIMEOUT`).
const H2_IDLE: Duration = Duration::from_secs(30);

/// A site built with `phv build`: its pages and static files, by their paths
/// under `public/`, and its phv.toml. Directories the project reads and writes
/// stay on disk, under the root given to the server.
pub struct Embedded {
    pub pages: HashMap<String, Arc<Compiled>>,
    pub statics: HashMap<String, Bytes>,
    pub config: Option<String>,
}

pub struct ServeOptions {
    /// Site directory; pages live in `<root>/public`.
    pub root: PathBuf,
    pub host: String,
    pub port: u16,
    /// Show error details in the browser.
    pub dev: bool,
    pub time_limit: Duration,
    /// Reload pages in the browser when a file of the site changes
    /// (`phv serve --watch`; development only).
    pub watch: bool,
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
    embedded: Option<Embedded>,
    /// Scripts running at once (`[server] workers`).
    workers: Arc<tokio::sync::Semaphore>,
    /// Bytes of request bodies received or held, in KiB: as much as the workers
    /// can use at once. Past its first `SMALL_BODY` bytes, a body takes its share
    /// as the bytes arrive (never for bytes not yet sent) and keeps it until its
    /// script ends.
    body_budget: Arc<tokio::sync::Semaphore>,
    /// Bytes of script output waiting to be sent, over all connections.
    buffered: Arc<AtomicUsize>,
    /// No new script runs while `buffered` is above this.
    buffered_cap: usize,
    /// Pages with parameters, and when `public/` was last scanned for them
    /// (again after a second in development mode, where files change).
    routes: Mutex<(Instant, Arc<routes::Table>)>,
    /// `[server] access_log`: the open file.
    access_log: Option<Mutex<std::fs::File>>,
    /// Tags of the static files of a built site, by path.
    embedded_tags: HashMap<String, String>,
    /// Compressed static files by `ETag`, and their total size.
    gz_cache: Mutex<(HashMap<String, Bytes>, usize)>,
    /// `phv serve --watch`: pages get a script that reloads them on changes.
    watch: bool,
    /// The last change stamp and when it was taken (every open tab asks each second).
    watch_stamp: tokio::sync::Mutex<Option<(Instant, String)>>,
}

impl State {
    fn new(root: PathBuf, dev: bool, time_limit: Duration, config: Config) -> State {
        let mut secret = vec![0u8; 32];
        getrandom::fill(&mut secret).expect("operating system random number generator failed");
        State {
            public: root.join("public"),
            sessions: match config.session_db.as_ref().and_then(|n| config.host.databases.get(n)) {
                Some(spec) => session::Store::with_database(
                    Duration::from_secs(config.session_ttl),
                    config.secure_cookies,
                    spec.clone(),
                ),
                None => session::Store::new(
                    Duration::from_secs(config.session_ttl),
                    config.secure_cookies,
                    config.session_dir.clone(),
                ),
            },
            csrf_cookie: if config.secure_cookies { "__Host-PHVCSRF" } else { "PHVCSRF" },
            csrf_secret: secret,
            workers: Arc::new(tokio::sync::Semaphore::new(config.workers)),
            body_budget: Arc::new(tokio::sync::Semaphore::new(config.workers.max(1) * (MAX_BODY / 1024))),
            buffered: Arc::new(AtomicUsize::new(0)),
            // As much output as the workers could make at once.
            buffered_cap: config.workers * (config.memory_limit_mb as usize) * 1024 * 1024,
            config,
            root,
            dev,
            time_limit,
            cache: Mutex::new(HashMap::new()),
            embedded: None,
            routes: Mutex::new((Instant::now(), Arc::new(routes::Table::default()))),
            access_log: None,
            embedded_tags: HashMap::new(),
            gz_cache: Mutex::new((HashMap::new(), 0)),
            watch: false,
            watch_stamp: tokio::sync::Mutex::new(None),
        }
    }

    fn scan_routes(&self) -> Result<routes::Table, String> {
        match &self.embedded {
            Some(e) => routes::Table::from_pages(e.pages.keys().map(String::as_str)),
            None => routes::Table::scan(&self.public),
        }
    }

    /// The table of pages with parameters, scanned again in development mode
    /// when the last scan is more than a second old.
    fn routes(&self) -> Result<Arc<routes::Table>, String> {
        let mut r = self.routes.lock().unwrap();
        if self.dev && self.embedded.is_none() && r.0.elapsed() > Duration::from_secs(1) {
            *r = (Instant::now(), Arc::new(self.scan_routes()?));
        }
        Ok(r.1.clone())
    }

    /// A special page under `public/` (`_404.phv`, `_middleware.phv`, ...), if the site has it.
    fn special(&self, rel: &str) -> Option<PathBuf> {
        if let Some(e) = &self.embedded {
            return e.pages.contains_key(rel).then(|| self.public.join(rel));
        }
        let c = self.public.join(rel).canonicalize().ok()?;
        let inner = c.strip_prefix(&self.public).ok()?;
        let same = inner.components().map(|x| x.as_os_str().to_str()).eq(rel.split('/').map(Some));
        (same && c.is_file()).then_some(c)
    }

    /// The `_middleware.phv` files that apply to a page, outermost first.
    fn middleware_for(&self, script: &Path) -> Vec<PathBuf> {
        let rel = script.strip_prefix(&self.public).map(|r| r.to_string_lossy().replace('\\', "/")).unwrap_or_default();
        let mut dirs: Vec<&str> = rel.split('/').collect();
        dirs.pop();
        let mut out = Vec::new();
        for i in 0..=dirs.len() {
            let prefix = dirs[..i].join("/");
            let file =
                if prefix.is_empty() { "_middleware.phv".to_string() } else { format!("{prefix}/_middleware.phv") };
            if let Some(p) = self.special(&file) {
                out.push(p);
            }
        }
        out
    }
}

pub fn serve(opts: ServeOptions) -> Result<(), String> {
    let addr: SocketAddr = format!("{}:{}", opts.host, opts.port)
        .parse()
        .map_err(|e| format!("invalid address {}:{}: {e}", opts.host, opts.port))?;
    let listener = std::net::TcpListener::bind(addr).map_err(|e| format!("cannot listen on {addr}: {e}"))?;
    serve_on(listener, opts)
}

/// Serves on an already bound listener (tests bind port 0) until Ctrl-C or SIGTERM.
pub fn serve_on(listener: std::net::TcpListener, opts: ServeOptions) -> Result<(), String> {
    serve_with(listener, opts, None, None)
}

/// Serves until `stop` completes, then lets running requests finish.
pub fn serve_until(
    listener: std::net::TcpListener,
    opts: ServeOptions,
    stop: impl Future<Output = ()> + Send + 'static,
) -> Result<(), String> {
    serve_with(listener, opts, None, Some(Box::pin(stop)))
}

/// Ctrl-C, or SIGTERM (what `docker stop` and process managers send).
async fn os_shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = term => {}
    }
}

/// Serves a site built with `phv build` (see `Embedded`).
pub fn serve_embedded(opts: ServeOptions, embedded: Embedded) -> Result<(), String> {
    let addr: SocketAddr = format!("{}:{}", opts.host, opts.port)
        .parse()
        .map_err(|e| format!("invalid address {}:{}: {e}", opts.host, opts.port))?;
    let listener = std::net::TcpListener::bind(addr).map_err(|e| format!("cannot listen on {addr}: {e}"))?;
    serve_with(listener, opts, Some(embedded), None)
}

type Stop = Pin<Box<dyn Future<Output = ()> + Send>>;

fn serve_with(
    listener: std::net::TcpListener,
    opts: ServeOptions,
    embedded: Option<Embedded>,
    stop: Option<Stop>,
) -> Result<(), String> {
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let root = opts.root.canonicalize().map_err(|e| format!("cannot open {}: {e}", opts.root.display()))?;
    let config = match &embedded {
        Some(e) => phv_driver::config::parse_config(&root, e.config.as_deref().unwrap_or(""))?,
        None => {
            if !root.join("public").is_dir() {
                return Err(format!("{} has no `public/` directory", root.display()));
            }
            load_config(&root)?
        }
    };
    let mut state = State::new(root, opts.dev, opts.time_limit, config);
    state.watch = opts.watch && opts.dev && state.embedded.is_none();
    if let Some((path, _)) = &state.config.access_log {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| format!("cannot open the access log {}: {e}", path.display()))?;
        state.access_log = Some(Mutex::new(file));
    }
    if let Some(e) = &embedded {
        state.embedded_tags = e.statics.iter().map(|(k, v)| (k.clone(), Validators::of_bytes(v).etag)).collect();
    }
    state.embedded = embedded;
    let table = state.scan_routes()?;
    state.routes = Mutex::new((Instant::now(), Arc::new(table)));
    let acceptor = match &state.config.tls {
        Some(t) => Some(tls::acceptor(t, &state.public)?),
        None => None,
    };
    let state = Arc::new(state);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        // Scripts run on blocking threads; give them room for deep recursion.
        .thread_stack_size(16 * 1024 * 1024)
        .build()
        .map_err(|e| e.to_string())?;
    let result = rt.block_on(async move {
        let mut stop: Stop = stop.unwrap_or_else(|| Box::pin(os_shutdown()));
        let listener = TcpListener::from_std(listener).map_err(|e| format!("cannot listen on {addr}: {e}"))?;
        let mode = if state.dev { "development" } else { "production" };
        let scheme = if acceptor.is_some() { "https" } else { "http" };
        eprintln!("PHV server ({mode} mode) listening on {scheme}://{addr}");
        if state.dev && !addr.ip().is_loopback() {
            eprintln!("warning: development mode shows error details to every visitor; use --prod");
        }
        match &state.embedded {
            Some(e) => eprintln!("Serving the built site ({} pages, {} files)", e.pages.len(), e.statics.len()),
            None => eprintln!("Serving {}", state.public.display()),
        }
        let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
        // Tells the connections to finish their request and close.
        let (closing, closed) = tokio::sync::watch::channel(false);
        loop {
            let permit = tokio::select! {
                p = slots.clone().acquire_owned() => p.expect("semaphore is never closed"),
                _ = &mut stop => break,
            };
            let (stream, peer) = tokio::select! {
                c = listener.accept() => match c {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("accept failed: {e}");
                        continue;
                    }
                },
                _ = &mut stop => break,
            };
            let state = state.clone();
            let closed = closed.clone();
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let io = StallGuard::new(stream, WRITE_STALL);
                match acceptor {
                    Some(acceptor) => {
                        // A client that does not finish the handshake in time is dropped.
                        if let Ok(Ok(io)) = tokio::time::timeout(HEADER_TIMEOUT, acceptor.accept(io)).await {
                            if io.get_ref().1.alpn_protocol() == Some(b"h2") {
                                serve_h2(state, peer, io, closed).await;
                            } else {
                                serve_h1(state, peer, io, closed).await;
                            }
                        }
                    }
                    None if state.config.h2c => serve_auto(state, peer, io, closed).await,
                    None => serve_h1(state, peer, io, closed).await,
                }
                drop(permit);
            });
        }
        // New connections are refused; running requests get `shutdown_timeout` to finish.
        drop(listener);
        let wait = Duration::from_secs(state.config.shutdown_timeout);
        eprintln!("Stopping: letting running requests finish (at most {} s)", wait.as_secs());
        let _ = closing.send(true);
        let all = slots.acquire_many(MAX_CONNECTIONS as u32);
        if tokio::time::timeout(wait, all).await.is_err() {
            eprintln!("Stopped with requests still running");
        } else {
            eprintln!("Stopped");
        }
        Ok(())
    });
    // Scripts still running past the timeout are not waited for.
    rt.shutdown_timeout(Duration::from_millis(500));
    result
}

/// Runs connection `$conn` until it ends, or until the server stops (then
/// it finishes the requests it has). With `$idle`, an HTTP/2 connection that
/// has had no request for `H2_IDLE` is closed the same way.
macro_rules! drive {
    ($conn:expr, $closed:expr, $idle:expr) => {{
        let conn = $conn;
        tokio::pin!(conn);
        let idle: Option<Arc<Activity>> = $idle;
        let mut closed = $closed;
        loop {
            tokio::select! {
                _ = conn.as_mut() => break,
                _ = closed.changed() => {
                    conn.as_mut().graceful_shutdown();
                    let _ = conn.await;
                    break;
                }
                _ = tokio::time::sleep(Duration::from_secs(1)), if idle.is_some() => {
                    if idle.as_ref().is_some_and(|a| a.idle_for() >= H2_IDLE) {
                        conn.as_mut().graceful_shutdown();
                        let _ = conn.await;
                        break;
                    }
                }
            }
        }
    }};
}

/// Requests in progress on one connection, and when the last one ended.
struct Activity {
    open: AtomicUsize,
    since: Mutex<Instant>,
}

impl Activity {
    fn new() -> Arc<Activity> {
        Arc::new(Activity { open: AtomicUsize::new(0), since: Mutex::new(Instant::now()) })
    }

    fn idle_for(&self) -> Duration {
        if self.open.load(Ordering::SeqCst) > 0 {
            return Duration::ZERO;
        }
        self.since.lock().map(|t| t.elapsed()).unwrap_or_default()
    }
}

/// Counts a request as open while its handler runs.
struct OpenRequest(Arc<Activity>);

impl OpenRequest {
    fn new(a: &Arc<Activity>) -> OpenRequest {
        a.open.fetch_add(1, Ordering::SeqCst);
        OpenRequest(a.clone())
    }
}

impl Drop for OpenRequest {
    fn drop(&mut self) {
        if let Ok(mut t) = self.0.since.lock() {
            *t = Instant::now();
        }
        self.0.open.fetch_sub(1, Ordering::SeqCst);
    }
}

fn h1_builder() -> http1::Builder {
    let mut b = http1::Builder::new();
    b.timer(TokioTimer::new()).header_read_timeout(HEADER_TIMEOUT).max_buf_size(CONN_BUFFER);
    b
}

fn h2_builder() -> http2::Builder<TokioExecutor> {
    let mut b = http2::Builder::new(TokioExecutor::new());
    b.timer(TokioTimer::new())
        .max_concurrent_streams(H2_STREAMS)
        .max_header_list_size(CONN_BUFFER as u32)
        .initial_stream_window_size(H2_WINDOW)
        .initial_connection_window_size(H2_WINDOW)
        .adaptive_window(false)
        .max_send_buf_size(CONN_BUFFER)
        .max_pending_accept_reset_streams(20)
        .max_local_error_reset_streams(256)
        .keep_alive_interval(Some(Duration::from_secs(30)))
        .keep_alive_timeout(Duration::from_secs(20));
    b
}

type Closed = tokio::sync::watch::Receiver<bool>;

async fn serve_h1<I>(state: Arc<State>, peer: SocketAddr, io: I, closed: Closed)
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let service = service_fn(move |req| handle(state.clone(), peer, req));
    drive!(h1_builder().serve_connection(TokioIo::new(io), service), closed, None)
}

async fn serve_h2<I>(state: Arc<State>, peer: SocketAddr, io: I, closed: Closed)
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let activity = Activity::new();
    let a = activity.clone();
    let service = service_fn(move |req| {
        let open = OpenRequest::new(&a);
        let state = state.clone();
        async move {
            let r = handle(state, peer, req).await;
            drop(open);
            r
        }
    });
    drive!(h2_builder().serve_connection(TokioIo::new(io), service), closed, Some(activity))
}

/// `[server] h2c`: HTTP/1.1, or HTTP/2 when the client starts with its preface.
async fn serve_auto<I>(state: Arc<State>, peer: SocketAddr, io: I, closed: Closed)
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let activity = Activity::new();
    let a = activity.clone();
    let service = service_fn(move |req| {
        let open = OpenRequest::new(&a);
        let state = state.clone();
        async move {
            let r = handle(state, peer, req).await;
            drop(open);
            r
        }
    });
    let mut b = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
    b.http1().timer(TokioTimer::new()).header_read_timeout(HEADER_TIMEOUT).max_buf_size(CONN_BUFFER);
    b.http2()
        .timer(TokioTimer::new())
        .max_concurrent_streams(H2_STREAMS)
        .max_header_list_size(CONN_BUFFER as u32)
        .initial_stream_window_size(H2_WINDOW)
        .initial_connection_window_size(H2_WINDOW)
        .adaptive_window(false)
        .max_send_buf_size(CONN_BUFFER)
        .max_pending_accept_reset_streams(20)
        .max_local_error_reset_streams(256)
        .keep_alive_interval(Some(Duration::from_secs(30)))
        .keep_alive_timeout(Duration::from_secs(20));
    drive!(b.serve_connection(TokioIo::new(io), service), closed, Some(activity))
}

type Resp = Response<RespBody>;

/// HTTP/2 sends the host as `:authority` and may split the cookies into
/// several fields: the server and scripts see them as HTTP/1.1 sends them.
fn as_http1_headers(mut req: Request<Incoming>) -> Request<Incoming> {
    if req.version() != hyper::Version::HTTP_2 {
        return req;
    }
    if !req.headers().contains_key(hyper::header::HOST) {
        let authority = req.uri().authority().map(|a| a.as_str().to_string());
        if let Some(v) = authority.filter(|a| !a.contains('@')).and_then(|a| HeaderValue::from_str(&a).ok()) {
            req.headers_mut().insert(hyper::header::HOST, v);
        }
    }
    let cookies: Vec<&[u8]> = req.headers().get_all(hyper::header::COOKIE).iter().map(|v| v.as_bytes()).collect();
    if cookies.len() > 1 {
        let joined = cookies.join(&b"; "[..]);
        if let Ok(v) = HeaderValue::from_bytes(&joined) {
            req.headers_mut().insert(hyper::header::COOKIE, v);
        }
    }
    req
}

async fn handle(state: Arc<State>, peer: SocketAddr, req: Request<Incoming>) -> Result<Resp, Infallible> {
    let req = as_http1_headers(req);
    let start = Instant::now();
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let ip = peer.ip().to_canonical().to_string();
    let gzip_ok = state.config.compress && static_files::accepts_gzip(req.headers());
    // A page answering a request that carries credentials may hold the visitor's
    // secrets next to text from the request: it is never compressed (BREACH).
    let credentialed = req.headers().contains_key("cookie") || req.headers().contains_key("authorization");
    if state.watch && method == Method::GET && path.starts_with("/__phv/") {
        let mut r = match path.as_str() {
            "/__phv/live-reload.js" => simple(StatusCode::OK, "text/javascript; charset=utf-8", LIVE_RELOAD_JS.into()),
            "/__phv/changes" => {
                // One walk a second at most, however many tabs ask (they wait for it).
                let mut cached = state.watch_stamp.lock().await;
                let stamp = match cached.as_ref() {
                    Some((at, stamp)) if at.elapsed() < Duration::from_secs(1) => stamp.clone(),
                    _ => {
                        let root = state.root.clone();
                        let skip: Vec<PathBuf> = state
                            .config
                            .host
                            .permissions
                            .write
                            .iter()
                            .map(|w| root.join(w.trim_end_matches('/')))
                            .collect();
                        let stamp =
                            tokio::task::spawn_blocking(move || change_stamp(&root, &skip)).await.unwrap_or_default();
                        *cached = Some((Instant::now(), stamp.clone()));
                        stamp
                    }
                };
                drop(cached);
                simple(StatusCode::OK, "text/plain; charset=utf-8", stamp)
            }
            _ => simple(StatusCode::NOT_FOUND, "text/plain", "not found".into()),
        };
        r.headers_mut().insert("cache-control", HeaderValue::from_static("no-store"));
        add_security_headers(&state, &mut r);
        return Ok(r);
    }
    let origin = req.headers().get("origin").and_then(|v| v.to_str().ok()).map(str::to_string);
    let cors = state.config.cors.as_ref().zip(origin.as_deref()).filter(|(c, o)| c.allows(o)).map(|(c, _)| c);
    // A CORS preflight is answered here, before rate limits and pages.
    if state.config.cors.is_some()
        && method == Method::OPTIONS
        && req.headers().contains_key("access-control-request-method")
    {
        let mut r = simple(StatusCode::NO_CONTENT, "text/plain", String::new());
        if let (Some(c), Some(o)) = (cors, &origin) {
            let h = r.headers_mut();
            let mut set = |name: &'static str, value: String| {
                if let Ok(v) = HeaderValue::from_str(&value) {
                    h.insert(name, v);
                }
            };
            set("access-control-allow-origin", if c.any { "*".into() } else { o.clone() });
            set("access-control-allow-methods", c.methods.join(", "));
            if !c.headers.is_empty() {
                set("access-control-allow-headers", c.headers.join(", "));
            }
            set("access-control-max-age", c.max_age.to_string());
            if c.credentials {
                set("access-control-allow-credentials", "true".into());
            }
        }
        static_files::vary(r.headers_mut(), "Origin");
        add_security_headers(&state, &mut r);
        log_request(&state, &ip, &method, &path, r.status().as_u16(), start);
        return Ok(r);
    }
    let limit = state.config.requests_per_minute;
    let client = format!("ip:{}", phv_std::ratelimit::client_key(peer.ip()));
    let minute = Duration::from_secs(60);
    let limited = if limit == 0 {
        None
    } else if state.config.host.rate_limit_store.is_none() {
        (!phv_std::ratelimit::hit(&client, limit, minute)).then(|| phv_std::ratelimit::retry_after(&client, minute))
    } else {
        // Counted on the shared Redis server; if it cannot be reached, in this process.
        let (host, key) = (state.config.host.clone(), client.clone());
        let until = Instant::now() + Duration::from_secs(2);
        let shared =
            tokio::task::spawn_blocking(move || phv_std::ratelimit::hit_shared(&host, &key, limit, minute, until))
                .await;
        match shared {
            Ok(Ok((allowed, wait))) => (!allowed).then_some(wait),
            Ok(Err(e)) => {
                eprintln!("warning: the shared rate limit store cannot be used ({e}); counting in this process");
                (!phv_std::ratelimit::hit(&client, limit, minute))
                    .then(|| phv_std::ratelimit::retry_after(&client, minute))
            }
            Err(_) => None,
        }
    };
    let mut resp = if let Some(wait) = limited {
        let mut r = simple(StatusCode::TOO_MANY_REQUESTS, "text/plain", "too many requests".into());
        r.headers_mut().insert("retry-after", HeaderValue::from(wait));
        r
    } else {
        route(&state, ip.clone(), req).await
    };
    if let (Some(c), Some(o)) = (cors, &origin) {
        let h = resp.headers_mut();
        let value = if c.any { "*".to_string() } else { o.clone() };
        if let Ok(v) = HeaderValue::from_str(&value) {
            h.insert("access-control-allow-origin", v);
        }
        if c.credentials {
            h.insert("access-control-allow-credentials", HeaderValue::from_static("true"));
        }
    }
    if state.config.cors.as_ref().is_some_and(|c| !c.any) {
        static_files::vary(resp.headers_mut(), "Origin");
    }
    if state.config.compress {
        resp = compress(&state, resp, gzip_ok, credentialed).await;
    }
    add_security_headers(&state, &mut resp);
    log_request(&state, &ip, &method, &path, resp.status().as_u16(), start);
    Ok(resp)
}

/// The script `phv serve --watch` adds to pages: it asks for the stamp of the
/// site's files every second and reloads the page when it changes.
const LIVE_RELOAD_JS: &str = r#"// phv serve --watch: reloads the page when a file of the site changes.
(() => {
  let seen = null;
  const poll = async () => {
    try {
      const r = await fetch("/__phv/changes", { cache: "no-store" });
      const stamp = await r.text();
      if (seen !== null && stamp !== seen) {
        location.reload();
        return;
      }
      seen = stamp;
    } catch (e) {}
    setTimeout(poll, 1000);
  };
  poll();
})();
"#;

/// Files and directories looked at for changes, at most (a huge tree is cut short, not walked).
const WATCH_MAX_FILES: usize = 10_000;

/// The newest modification time and the number of the site's files: it
/// changes when a file is saved, added or removed. Data the site writes
/// (`[permissions] write`), hidden directories and packages are left out.
fn change_stamp(root: &Path, skip: &[PathBuf]) -> String {
    let (mut newest, mut count) = (0u128, 0usize);
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            if count >= WATCH_MAX_FILES {
                break;
            }
            let (path, name) = (e.path(), e.file_name().to_string_lossy().into_owned());
            let Ok(ty) = e.file_type() else { continue };
            if ty.is_symlink() || name.starts_with('.') || skip.contains(&path) {
                continue;
            }
            // Directories count too: a tree of empty directories is no cheaper to walk.
            count += 1;
            if ty.is_dir() {
                if !matches!(name.as_str(), "vendor" | "node_modules" | "target") {
                    dirs.push(path);
                }
                continue;
            }
            if let Ok(t) = e.metadata().and_then(|m| m.modified()) {
                let t = t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
                newest = newest.max(t);
            }
        }
    }
    format!("{newest}-{count}")
}

/// Adds the live-reload script before `</body>` (or at the end).
fn inject_live_reload(html: &mut String) {
    const TAG: &str = "<script src=\"/__phv/live-reload.js\" defer></script>";
    match html.to_ascii_lowercase().rfind("</body>") {
        Some(i) => html.insert_str(i, TAG),
        None => html.push_str(TAG),
    }
}

/// gzip for a text response in memory, when the client accepts it. Static
/// files are compressed once per version (`ETag`) and kept.
async fn compress(state: &State, mut resp: Resp, gzip_ok: bool, credentialed: bool) -> Resp {
    let text = resp.headers().get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(static_files::compressible);
    if text {
        static_files::vary(resp.headers_mut(), "Accept-Encoding");
    }
    let is_static = resp.extensions().get::<static_files::StaticFile>().is_some();
    let personal = resp.extensions().get::<static_files::Personal>().is_some() || (credentialed && !is_static);
    if !gzip_ok || personal || !static_files::may_compress(&resp) {
        return resp;
    }
    // The cache is keyed by the file too: two files of one size and time share an ETag.
    let tag = resp
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .zip(resp.extensions().get::<static_files::StaticFile>())
        .map(|(t, f)| format!("{}\n{t}", f.0));
    if let Some(gz) = tag.as_ref().and_then(|t| state.gz_cache.lock().unwrap().0.get(t).cloned()) {
        *resp.body_mut() = RespBody::full(gz);
        static_files::mark_gzip(resp.headers_mut());
        return resp;
    }
    let Some(bytes) = resp.body_mut().take_bytes() else { return resp };
    let max = if is_static { MAX_STATIC_COMPRESS as usize } else { static_files::MAX_COMPRESS };
    let packed = if (1024..=max).contains(&bytes.len()) {
        let data = bytes.clone();
        tokio::task::spawn_blocking(move || static_files::gzip(&data)).await.ok().flatten()
    } else {
        None
    };
    match packed {
        Some(gz) => {
            let gz = Bytes::from(gz);
            if let Some(t) = tag {
                let mut cache = state.gz_cache.lock().unwrap();
                // A bounded cache: emptied when full (files change rarely).
                if cache.1 + gz.len() > GZ_CACHE {
                    *cache = (HashMap::new(), 0);
                }
                cache.1 += gz.len();
                cache.0.insert(t, gz.clone());
            }
            // A page's compressed output counts like its output did.
            *resp.body_mut() =
                if is_static { RespBody::full(gz) } else { RespBody::counted(gz.to_vec(), &state.buffered) };
            static_files::mark_gzip(resp.headers_mut());
        }
        None => *resp.body_mut() = RespBody::full(bytes),
    }
    resp
}

/// One line on stderr, and in the access log when phv.toml sets one.
fn log_request(state: &State, ip: &str, method: &Method, path: &str, status: u16, start: Instant) {
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    eprintln!("{method} {path} {status} {ms:.1}ms");
    let (Some(file), Some((_, format))) = (&state.access_log, &state.config.access_log) else { return };
    let time = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let line = match format {
        LogFormat::Text => {
            let path = path.replace('\\', "\\\\").replace('"', "\\\"");
            format!("{time} {ip} \"{method} {path}\" {status} {ms:.1}ms\n")
        }
        LogFormat::Json => {
            let v = serde_json::json!({
                "time": time,
                "ip": ip,
                "method": method.as_str(),
                "path": path,
                "status": status,
                "ms": (ms * 10.0).round() / 10.0,
            });
            format!("{v}\n")
        }
    };
    use std::io::Write;
    if let Ok(mut f) = file.lock() {
        let _ = f.write_all(line.as_bytes());
    }
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
    /// A page, with the parameters of its path.
    Script(PathBuf, Vec<(String, String)>),
    Static(PathBuf),
    /// A static file of a built site, with its content type and tag.
    Embedded(Bytes, &'static str, String),
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
    if let Some(e) = &state.embedded {
        return resolve_embedded(state, e, rel);
    }
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
                return Some(Target::Script(p, Vec::new()));
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
    let table = state.routes().ok()?;
    let (file, params) = table.find(&segments)?;
    exact(file).map(|p| Target::Script(p, params))
}

/// Checks the pages with parameters under `public/` (`phv build` refuses a
/// site whose routes conflict, as the server does when it starts).
pub fn check_routes(public: &Path) -> Result<(), String> {
    routes::Table::scan(public).map(|_| ())
}

/// Static files are served only for these extensions, so stray source or
/// secret files (`.php`, `.bak`, `.toml`, `.sql`, ...) in public/ are not exposed.
/// Whether the server serves a file like `path` from public/ (by its type).
pub fn serves_static(path: &Path) -> bool {
    content_type(path).is_some()
}

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
    let gzip_ok = state.config.compress && static_files::accepts_gzip(req.headers());
    if let Err(e) = state.routes() {
        eprintln!("error in the routes of public/: {e}");
        return state.error_page("Route error", &e);
    }
    let target = resolve(state, req.uri().path());
    match target {
        None => match state.special("_404.phv") {
            Some(page) => run_script(state, page, Vec::new(), ip, req, Page::NotFound).await,
            None => not_found(),
        },
        Some(Target::Embedded(bytes, ct, tag)) => {
            if req.method() != Method::GET && req.method() != Method::HEAD {
                return simple(StatusCode::METHOD_NOT_ALLOWED, "text/plain", "method not allowed".into());
            }
            let v = Validators { etag: tag, modified: None };
            let len = bytes.len() as u64;
            let headers = req.headers();
            let mut r = if v.not_modified(headers) {
                let mut r = Response::new(RespBody::full(Bytes::new()));
                *r.status_mut() = StatusCode::NOT_MODIFIED;
                r
            } else {
                match static_files::range(headers, len).filter(|_| v.range_applies(headers)) {
                    Some(Err(())) => return static_files::unsatisfiable(len),
                    Some(Ok((a, b))) => partial(RespBody::full(bytes.slice(a as usize..=b as usize)), a, b, len),
                    None => Response::new(RespBody::full(bytes)),
                }
            };
            static_headers(state, &mut r, ct, &v, req.uri().path());
            r
        }
        Some(Target::Static(p)) => {
            if req.method() != Method::GET && req.method() != Method::HEAD {
                return simple(StatusCode::METHOD_NOT_ALLOWED, "text/plain", "method not allowed".into());
            }
            let Ok(mut file) = tokio::fs::File::open(&p).await else { return not_found() };
            let (len, modified) = match file.metadata().await {
                Ok(m) if m.is_file() => (m.len(), m.modified().ok()),
                _ => return not_found(),
            };
            if len > MAX_STATIC {
                return simple(StatusCode::PAYLOAD_TOO_LARGE, "text/plain", "file too large to serve".into());
            }
            let ct = content_type(&p).unwrap_or("application/octet-stream");
            let v = Validators::of_file(len, modified);
            let headers = req.headers();
            let mut r = if v.not_modified(headers) {
                let mut r = Response::new(RespBody::full(Bytes::new()));
                *r.status_mut() = StatusCode::NOT_MODIFIED;
                r
            } else {
                match static_files::range(headers, len).filter(|_| v.range_applies(headers)) {
                    Some(Err(())) => return static_files::unsatisfiable(len),
                    Some(Ok((a, b))) => {
                        use tokio::io::AsyncSeekExt;
                        if file.seek(std::io::SeekFrom::Start(a)).await.is_err() {
                            return not_found();
                        }
                        partial(RespBody::file(file, b - a + 1), a, b, len)
                    }
                    // Small text files a client takes gzipped are read whole, so `compress`
                    // can gzip them (once per version); others are streamed: memory does
                    // not grow with the file or the downloads.
                    None if gzip_ok && len <= MAX_STATIC_COMPRESS && static_files::compressible(ct) => {
                        use tokio::io::AsyncReadExt;
                        let mut data = Vec::with_capacity(len as usize);
                        if file.read_to_end(&mut data).await.is_err() {
                            return not_found();
                        }
                        Response::new(RespBody::counted(data, &state.buffered))
                    }
                    None => Response::new(RespBody::file(file, len)),
                }
            };
            static_headers(state, &mut r, ct, &v, &p.to_string_lossy());
            r
        }
        Some(Target::Script(p, params)) => run_script(state, p, params, ip, req, Page::Normal).await,
    }
}

/// A 206 answer for bytes `a..=b` of `len`.
fn partial(body: RespBody, a: u64, b: u64, len: u64) -> Resp {
    let mut r = Response::new(body);
    *r.status_mut() = StatusCode::PARTIAL_CONTENT;
    if let Ok(v) = HeaderValue::from_str(&format!("bytes {a}-{b}/{len}")) {
        r.headers_mut().insert("content-range", v);
    }
    r
}

fn static_headers(state: &State, r: &mut Resp, ct: &'static str, v: &Validators, file: &str) {
    r.extensions_mut().insert(static_files::StaticFile(file.to_string()));
    let h = r.headers_mut();
    h.insert(CONTENT_TYPE, HeaderValue::from_static(ct));
    h.insert("accept-ranges", HeaderValue::from_static("bytes"));
    v.headers(h, state.config.static_max_age);
}

/// What a script answers: a page (after its middleware), or a custom error page.
#[derive(Clone, Copy, PartialEq)]
enum Page {
    Normal,
    NotFound,
    Error,
}

/// `resolve` for a built site: its pages are known by their paths under
/// `public/`, which `State::compiled` looks up again.
fn resolve_embedded(state: &State, e: &Embedded, rel: String) -> Option<Target> {
    let candidates = if rel.is_empty() {
        vec!["index.phv".to_string()]
    } else if rel.ends_with(".phv") {
        vec![rel.clone()]
    } else {
        vec![format!("{rel}.phv"), format!("{rel}/index.phv")]
    };
    if let Some(page) = candidates.into_iter().find(|c| e.pages.contains_key(c)) {
        return Some(Target::Script(state.public.join(page), Vec::new()));
    }
    let Some(bytes) = e.statics.get(&rel) else {
        let table = state.routes().ok()?;
        let segments: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
        let (file, params) = table.find(&segments)?;
        return Some(Target::Script(state.public.join(file), params));
    };
    let lower = rel.to_ascii_lowercase();
    if lower.ends_with(".phv") || lower.ends_with(".phv.html") {
        return None;
    }
    let tag = state.embedded_tags.get(&rel).cloned().unwrap_or_else(|| Validators::of_bytes(bytes).etag);
    Some(Target::Embedded(bytes.clone(), content_type(Path::new(&rel))?, tag))
}

impl State {
    /// Compiles a page, reusing the cached bytecode until a source file changes.
    /// Pages of a built site are compiled already.
    fn compiled(&self, script: &Path) -> Result<Arc<Compiled>, String> {
        if let Some(e) = &self.embedded {
            let rel = script.strip_prefix(&self.public).map_err(|_| "not a page of this site".to_string())?;
            let rel = rel.to_string_lossy().replace('\\', "/");
            return e.pages.get(&rel).cloned().ok_or_else(|| format!("no page {rel} in this site"));
        }
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
    fn execute(
        &self,
        script: &Path,
        params: Vec<(String, String)>,
        ip: String,
        parts: hyper::http::request::Parts,
        body: Bytes,
        kind: Page,
    ) -> Resp {
        let headers: Vec<(String, String)> = parts
            .headers
            .iter()
            .map(|(k, v)| (k.as_str().to_ascii_lowercase(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
            .collect();
        let too_many = || simple(StatusCode::BAD_REQUEST, "text/plain", "bad request: too many fields".into());
        let Some(query) = fields(parts.uri.query().unwrap_or("").as_bytes()) else { return too_many() };
        let raw_content_type = parts.headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
        let content_type = raw_content_type.to_ascii_lowercase();
        let mut form = Vec::new();
        let mut files = Vec::new();
        let mut raw_body = true;
        if content_type.starts_with("application/x-www-form-urlencoded") {
            let Some(f) = fields(&body) else { return too_many() };
            form = f;
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
        if form.len() > MAX_FIELDS {
            return too_many();
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
            // Tokens are masked with a pad of their own on each page (BREACH).
            let ok = matches!((&expected, submitted), (Some(e), Some(s)) if phv_std::csrf_matches(e, s));
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
        let mut ctx = HttpCtx {
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
            params,
            status: match kind {
                Page::Normal => 200,
                Page::NotFound => 404,
                Page::Error => 500,
            },
            ..HttpCtx::default()
        };
        drop(body);
        // What the error page of this request gets: the request line and headers, nothing sent in it.
        let error_ctx = || HttpCtx {
            method: parts.method.as_str().to_string(),
            path: percent_encoding::percent_decode_str(parts.uri.path()).decode_utf8_lossy().into_owned(),
            headers: parts
                .headers
                .iter()
                .map(|(k, v)| (k.as_str().to_ascii_lowercase(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
                .collect(),
            remote_ip: String::new(),
            csp_nonce: self.config.csp.uses_nonce().then(|| phv_std::random_hex(16)),
            status: 500,
            ..HttpCtx::default()
        };
        // In production, a failing page is answered by `_500.phv` when the site has one.
        let failed = |title: &str, detail: &str| -> Resp {
            if !self.dev && kind != Page::Error {
                if let Some(page) = self.special("_500.phv") {
                    if let Some(r) = self.run_page(&page, error_ctx(), false) {
                        return r;
                    }
                }
            }
            self.error_page(title, detail)
        };

        // Middleware runs first, outermost first; one that answers (writes
        // output, sets a status, redirects or sends a file) ends the request.
        if kind == Page::Normal {
            for mw in self.middleware_for(script) {
                match self.run_one(&mw, ctx) {
                    Err((title, detail)) => return failed(&title, &detail),
                    Ok(outcome) => {
                        let answered = !outcome.output.is_empty()
                            || outcome.http.status != 200
                            || outcome.http.body_file.is_some();
                        if answered {
                            return self.respond(outcome, had_session);
                        }
                        ctx = outcome.http;
                    }
                }
            }
        }
        match self.run_one(script, ctx) {
            Ok(outcome) => self.respond(outcome, had_session),
            Err((title, detail)) => failed(&title, &detail),
        }
    }

    /// Runs one script for the request; a compile or runtime error gives its title and details.
    fn run_one(&self, script: &Path, ctx: HttpCtx) -> Result<phv_driver::RunOutcome, (String, String)> {
        let rel = script.strip_prefix(&self.root).unwrap_or(script).to_string_lossy().replace('\\', "/");
        let compiled = match self.compiled(script) {
            Ok(c) => c,
            Err(diagnostics) => {
                eprintln!("compile error in {rel}:\n{diagnostics}");
                return Err(("Compile error".into(), format!("{rel} could not be compiled.\n\n{diagnostics}")));
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
            return Err(("Runtime error".into(), text));
        }
        Ok(outcome)
    }

    /// Runs an error page; `None` if it fails too (the built-in page is used then).
    fn run_page(&self, script: &Path, ctx: HttpCtx, had_session: bool) -> Option<Resp> {
        self.run_one(script, ctx).ok().map(|o| self.respond(o, had_session))
    }

    /// The response for what a script did: status, headers, cookies and body.
    fn respond(&self, outcome: phv_driver::RunOutcome, had_session: bool) -> Resp {
        let mut http = outcome.http;
        let csp = self.config.csp.page_header(http.csp_nonce.as_deref());
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
            let mut output = outcome.output;
            if self.watch && http.content_type.as_deref().is_none_or(|c| c.starts_with("text/html")) {
                inject_live_reload(&mut output);
            }
            RespBody::counted(output.into_bytes(), &self.buffered)
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
        let personal = touched_session || sets_cookie || http.personalized;
        if personal && !h.contains_key("cache-control") {
            h.insert("cache-control", HeaderValue::from_static("private, no-store"));
        }
        if personal {
            r.extensions_mut().insert(static_files::Personal);
        }
        r
    }
}

/// The fields of a query string or a URL-encoded form; `None` past `MAX_FIELDS`
/// (counted while parsing, so millions of short fields are never all decoded).
fn fields(bytes: &[u8]) -> Option<Vec<(String, String)>> {
    let mut out = Vec::new();
    for (k, v) in form_urlencoded::parse(bytes) {
        if out.len() == MAX_FIELDS {
            return None;
        }
        out.push((k.into_owned(), v.into_owned()));
    }
    Some(out)
}

fn busy() -> Resp {
    let mut r = simple(StatusCode::SERVICE_UNAVAILABLE, "text/plain", "the server is busy; try again soon".into());
    r.headers_mut().insert("retry-after", HeaderValue::from_static("5"));
    r
}

/// Reads a request body of at most `MAX_BODY` bytes within `BODY_TIMEOUT`.
/// Past the first `SMALL_BODY` bytes, each KiB takes a permit from the body
/// budget as it arrives, so a client holds only what it has really sent, and
/// slow clients cannot pile up gigabytes. The permits are returned with the
/// body and are kept until its script ends.
async fn read_body(state: &State, mut body: Incoming) -> Result<(Bytes, Option<OwnedSemaphorePermit>), Box<Resp>> {
    let too_large = || simple(StatusCode::PAYLOAD_TOO_LARGE, "text/plain", "request body too large".into());
    let stated = hyper::body::Body::size_hint(&body).upper();
    if stated.is_some_and(|n| n > MAX_BODY as u64) {
        return Err(Box::new(too_large()));
    }
    let deadline = tokio::time::Instant::now() + BODY_TIMEOUT;
    let timed_out = || simple(StatusCode::REQUEST_TIMEOUT, "text/plain", "request body timed out".into());
    // One buffer, and the budget pays for its room (past `SMALL_BODY`), not only
    // for the bytes in it: the memory a body holds is what its permits say.
    // (Keeping the chunks as they arrive would cost a handle per chunk: a body
    // sent a byte at a time would hold many times its size.)
    let limit = stated.map_or(MAX_BODY, |n| n as usize);
    let mut data: Vec<u8> = Vec::new();
    let mut held: Option<OwnedSemaphorePermit> = None;
    let mut held_kib = 0usize;
    loop {
        let frame = match tokio::time::timeout_at(deadline, body.frame()).await {
            Err(_) => return Err(Box::new(timed_out())),
            Ok(None) => break,
            Ok(Some(Err(_))) => {
                return Err(Box::new(simple(StatusCode::BAD_REQUEST, "text/plain", "bad request body".into())))
            }
            Ok(Some(Ok(f))) => f,
        };
        let Ok(chunk) = frame.into_data() else { continue };
        let len = data.len() + chunk.len();
        if len > MAX_BODY {
            return Err(Box::new(too_large()));
        }
        if len > data.capacity() {
            // Room grows by doubling, up to the stated length (only as bytes arrive).
            let room = (data.capacity() * 2).max(4096).min(limit).max(len);
            let kib = room.saturating_sub(SMALL_BODY).div_ceil(1024);
            if kib > held_kib {
                let wait = deadline.min(tokio::time::Instant::now() + QUEUE_WAIT);
                let more = (kib - held_kib) as u32;
                let permit =
                    match tokio::time::timeout_at(wait, state.body_budget.clone().acquire_many_owned(more)).await {
                        Ok(Ok(p)) => p,
                        _ => return Err(Box::new(busy())),
                    };
                match &mut held {
                    Some(h) => h.merge(permit),
                    None => held = Some(permit),
                }
                held_kib = kib;
            }
            data.reserve_exact(room - data.len());
        }
        data.extend_from_slice(&chunk);
    }
    Ok((Bytes::from(data), held))
}

async fn run_script(
    state: &Arc<State>,
    script: PathBuf,
    params: Vec<(String, String)>,
    ip: String,
    req: Request<Incoming>,
    kind: Page,
) -> Resp {
    let (parts, body) = req.into_parts();
    // Memory is bounded by the scripts running at once, the bodies they read and
    // the output waiting to be sent; past a limit, new requests are turned away for now.
    let (body, reserved) = match read_body(state, body).await {
        Ok(b) => b,
        Err(r) => return *r,
    };
    if state.buffered.load(Ordering::Relaxed) > state.buffered_cap {
        return busy();
    }
    let Ok(Ok(permit)) = tokio::time::timeout(QUEUE_WAIT, state.workers.clone().acquire_owned()).await else {
        return busy();
    };
    let st = state.clone();
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let _reserved = reserved;
        st.execute(&script, params, ip, parts, body, kind)
    })
    .await
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("internal error: {e}");
            page(StatusCode::INTERNAL_SERVER_ERROR, "500 Internal Server Error", "<p>Something went wrong.</p>")
        }
    }
}

/// The site of a project for `phv test` (`std.testing`): requests are answered
/// in-process, the way `phv serve` answers them in development mode, except
/// that forms need no CSRF token.
pub fn test_site(root: &Path) -> Result<Arc<dyn phv_vm::Site>, String> {
    let root = root.canonicalize().map_err(|e| format!("cannot open {}: {e}", root.display()))?;
    if !root.join("public").is_dir() {
        return Err(format!("{} has no `public/` directory", root.display()));
    }
    let mut config = load_config(&root)?;
    config.csrf = false;
    let mut state = State::new(root, true, Duration::from_secs(30), config);
    let table = state.scan_routes()?;
    state.routes = Mutex::new((Instant::now(), Arc::new(table)));
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
    Ok(Arc::new(TestSite { state: Arc::new(state), rt }))
}

struct TestSite {
    state: Arc<State>,
    /// Reads the bodies of responses (a file a page sends is read from disk).
    rt: tokio::runtime::Runtime,
}

impl std::fmt::Debug for TestSite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TestSite({})", self.state.root.display())
    }
}

impl phv_vm::Site for TestSite {
    fn request(&self, req: phv_vm::SiteRequest) -> Result<phv_vm::SiteResponse, String> {
        let mut b = Request::builder().method(req.method.as_str()).uri(req.path.as_str());
        for (k, v) in &req.headers {
            b = b.header(k.as_str(), v.as_str());
        }
        let (parts, ()) = b.body(()).map_err(|e| format!("invalid request: {e}"))?.into_parts();
        let state = self.state.clone();
        let body = Bytes::from(req.body);
        // The page runs in a VM of its own, on a thread with the server's stack size.
        let resp = std::thread::scope(|s| {
            std::thread::Builder::new()
                .stack_size(16 * 1024 * 1024)
                .spawn_scoped(s, move || state.answer(parts, body))
                .map_err(|e| e.to_string())?
                .join()
                .map_err(|_| "the page stopped with an internal error".to_string())
        })?;
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
            .collect();
        let body = self.rt.block_on(resp.into_body().collect()).map_err(|e| e.to_string())?.to_bytes().to_vec();
        Ok(phv_vm::SiteResponse { status, headers, body })
    }
}

impl State {
    /// What `route` answers, without a network (for `phv test`).
    fn answer(&self, parts: hyper::http::request::Parts, body: Bytes) -> Resp {
        let ip = "127.0.0.1".to_string();
        if let Err(e) = self.routes() {
            return self.error_page("Route error", &e);
        }
        match resolve(self, parts.uri.path()) {
            None => match self.special("_404.phv") {
                Some(page) => self.execute(&page, Vec::new(), ip, parts, body, Page::NotFound),
                None => not_found(),
            },
            Some(Target::Script(p, params)) => self.execute(&p, params, ip, parts, body, Page::Normal),
            Some(Target::Static(p)) => match std::fs::read(&p) {
                Ok(data) => {
                    let mut r = Response::new(RespBody::full(data));
                    let ct = content_type(&p).unwrap_or("application/octet-stream");
                    r.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static(ct));
                    r
                }
                Err(_) => not_found(),
            },
            Some(Target::Embedded(bytes, ct, _)) => {
                let mut r = Response::new(RespBody::full(bytes));
                r.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static(ct));
                r
            }
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
        assert!(matches!(resolve(&st, "/"), Some(Target::Script(..))));
        assert!(matches!(resolve(&st, "/blog/post"), Some(Target::Script(..))));
        assert!(matches!(resolve(&st, "/blog/post.phv"), Some(Target::Script(..))));
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
