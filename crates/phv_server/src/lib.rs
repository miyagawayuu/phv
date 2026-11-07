//! Development/production web server: routes `public/` by file name and runs
//! each request in a fresh VM (nothing is shared between requests except the
//! compiled, immutable bytecode).

mod admin;
mod body;
mod bus;
mod jobs;
mod log;
mod metrics;
mod multipart;
mod routes;
mod session;
mod static_files;
mod telemetry;
mod tls;
mod ws;

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
use phv_driver::{compile_cached, load_config, run, Compiled, Config, RunOptions, Teardown};
use phv_vm::value::html_escape;
use phv_vm::{FlushSink, HttpCtx, Limits, SessionData, WsCtx};
use static_files::Validators;
use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
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
/// `response()->flush()` gives up (returns false) when the client has taken
/// nothing for this long: a client that stops reading holds the page's
/// worker this long at most, not until the page's time limit.
const FLUSH_STALL: Duration = Duration::from_secs(10);
/// Time a request waits for a free worker before it gets 503.
const QUEUE_WAIT: Duration = Duration::from_secs(10);
/// Static files larger than this are sent uncompressed.
const MAX_STATIC_COMPRESS: u64 = 1 << 20;
/// Bytes of compressed static files kept, so each version is compressed once.
const GZ_CACHE: usize = 32 << 20;
/// A connection's read and write buffers (hyper's default is about 400 KB):
/// a client that stops reading holds at most this much (and a file's 64 KB).
const CONN_BUFFER: usize = 64 * 1024;
/// Requests one HTTP/2 connection may have open at once, and the bytes of a
/// response it holds per stream: with the 16 KB a file is read in, an HTTP/2
/// connection holds about what an HTTP/1.1 one does (found by the 1.0 audit:
/// 64 streams of 64 KB let 200 connections hold 2.5 GB).
const H2_STREAMS: u32 = 8;
const H2_SEND_BUF: usize = 16 * 1024;
/// After an HTTP/2 connection is told to close, the longest wait for its
/// streams before it is dropped (a client may never take them).
const H2_GRACE: Duration = Duration::from_secs(10);
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
    /// The jobs of `[jobs] dir`, by name.
    pub jobs: HashMap<String, Arc<Compiled>>,
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
    /// Key for CSRF tokens: derived from `[security] secret_env`, or new on
    /// every start (open forms then need a reload).
    csrf_secret: Vec<u8>,
    /// The key derived from `secret_old_env`, still accepted while the secret
    /// is being replaced.
    csrf_old: Option<Vec<u8>>,
    /// The server's secret (`[security] secret_env`), when the site has one.
    secret: Option<Vec<u8>>,
    csrf_cookie: &'static str,
    /// Compiled pages, and a lock for each page being compiled (one thread
    /// compiles it; the others wait for its result).
    cache: Mutex<HashMap<PathBuf, Arc<CachedPage>>>,
    compiling: Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>,
    /// When the server started (the clock of `CachedPage::checked`).
    started: Instant,
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
    /// Bytes of event streams waiting to be sent: a budget of their own,
    /// `events_cap`, so streams cannot make pages answer 503 (found by the 1.1
    /// audit).
    events_buffered: Arc<AtomicUsize>,
    /// No new script runs while `buffered` and `events_buffered` together are
    /// above this.
    buffered_cap: usize,
    /// A quarter of `buffered_cap`: pages always have the other three
    /// quarters, and both together stay within `buffered_cap` (found by the
    /// 1.1.1 review: each had all of it, so the output reached twice the cap).
    events_cap: usize,
    /// The open event streams, by id: what each holds and how to drop its
    /// connection (to make room for streams whose clients read).
    streams: Mutex<HashMap<String, ws::Registered>>,
    /// Pages with parameters, and when `public/` was last scanned for them
    /// (again after a second in development mode, where files change).
    routes: Mutex<(Instant, Arc<routes::Table>)>,
    /// Writes the line of each request (and the access log).
    log: log::Logger,
    /// `Content-Security-Policy` for responses without a nonce (made once).
    csp_plain: Option<HeaderValue>,
    /// Small static files a production server keeps in memory, by the path
    /// they are asked for, and their total size.
    static_cache: Mutex<(HashMap<String, Arc<StaticEntry>>, usize)>,
    /// What request paths led to, for a production server (see `locate`).
    found_cache: Mutex<HashMap<String, Arc<FoundEntry>>>,
    /// Where the special pages a production server asked for are, by the
    /// name asked for (`admin/_middleware.phv`, `_404.phv`; see `special`).
    specials: Mutex<HashMap<String, Arc<SpecialEntry>>>,
    /// Tags of the static files of a built site, by path.
    embedded_tags: HashMap<String, String>,
    /// Compressed static files by `ETag`, and their total size.
    gz_cache: Mutex<(HashMap<String, Bytes>, usize)>,
    /// A lock for each static file being compressed: one request compresses it,
    /// the others wait and take the result from `gz_cache`.
    gz_flight: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// `phv serve --watch`: pages get a script that reloads them on changes.
    watch: bool,
    /// The last change stamp and when it was taken (every open tab asks each second).
    watch_stamp: tokio::sync::Mutex<Option<(Instant, String)>>,
    /// WebSocket connections and the topics they joined.
    hub: Arc<Mutex<ws::Hub>>,
    /// `[server] publish_store`: carries `publish()` to the other server processes.
    bus: Option<bus::Bus>,
    /// A slot for each WebSocket connection that may be open.
    ws_slots: Arc<tokio::sync::Semaphore>,
    /// Set when the server stops: WebSocket connections are closed.
    ws_closing: tokio::sync::watch::Sender<bool>,
    /// What `/metrics` shows.
    metrics: Arc<metrics::Metrics>,
    /// The server is stopping (`/health` answers 503).
    stopping: std::sync::atomic::AtomicBool,
    /// A slot for each connection that may be open.
    conn_slots: Arc<tokio::sync::Semaphore>,
    /// `[telemetry]`: where spans go, when traces are exported.
    telemetry: Option<Arc<telemetry::Exporter>>,
}

impl State {
    fn new(root: PathBuf, dev: bool, time_limit: Duration, config: Config) -> State {
        let mut secret = vec![0u8; 32];
        getrandom::fill(&mut secret).expect("operating system random number generator failed");
        let csp_plain = config.csp.header(None).and_then(|c| HeaderValue::from_str(&c).ok());
        let metrics = Arc::new(metrics::Metrics::default());
        State {
            public: root.join("public"),
            sessions: match config.session_db.as_ref().and_then(|n| config.host.databases.get(n)) {
                Some(spec) => session::Store::with_database(
                    Duration::from_secs(config.session_ttl),
                    config.secure_cookies,
                    spec.clone(),
                    metrics.health.clone(),
                    config.host.store_namespace.clone(),
                ),
                None => session::Store::new(
                    Duration::from_secs(config.session_ttl),
                    config.secure_cookies,
                    config.session_dir.clone(),
                ),
            },
            csrf_cookie: if config.secure_cookies { "__Host-PHVCSRF" } else { "PHVCSRF" },
            csrf_secret: secret,
            csrf_old: None,
            secret: None,
            workers: Arc::new(tokio::sync::Semaphore::new(config.workers)),
            body_budget: Arc::new(tokio::sync::Semaphore::new(config.workers.max(1) * (MAX_BODY / 1024))),
            buffered: Arc::new(AtomicUsize::new(0)),
            events_buffered: Arc::new(AtomicUsize::new(0)),
            // As much output as the workers could make at once.
            buffered_cap: config.workers * (config.memory_limit_mb as usize) * 1024 * 1024,
            events_cap: config.workers * (config.memory_limit_mb as usize) * 1024 * 1024 / 4,
            streams: Mutex::new(HashMap::new()),
            config,
            root,
            dev,
            time_limit,
            cache: Mutex::new(HashMap::new()),
            compiling: Mutex::new(HashMap::new()),
            started: Instant::now(),
            embedded: None,
            routes: Mutex::new((Instant::now(), Arc::new(routes::Table::default()))),
            log: log::Logger::start(None),
            csp_plain,
            static_cache: Mutex::new((HashMap::new(), 0)),
            found_cache: Mutex::new(HashMap::new()),
            specials: Mutex::new(HashMap::new()),
            embedded_tags: HashMap::new(),
            gz_cache: Mutex::new((HashMap::new(), 0)),
            gz_flight: Mutex::new(HashMap::new()),
            watch: false,
            watch_stamp: tokio::sync::Mutex::new(None),
            hub: Arc::new(Mutex::new(ws::Hub::default())),
            bus: None,
            ws_slots: Arc::new(tokio::sync::Semaphore::new(ws::MAX_WEBSOCKETS)),
            ws_closing: tokio::sync::watch::channel(false).0,
            metrics,
            stopping: std::sync::atomic::AtomicBool::new(false),
            conn_slots: Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS)),
            telemetry: None,
        }
    }

    /// Delivers what a script published: to the connections of this process,
    /// then (queued: the script never waits for the store) to the others.
    fn deliver(&self, publishes: &[(String, String)]) {
        if publishes.is_empty() {
            return;
        }
        {
            let mut hub = self.hub.lock().unwrap_or_else(|e| e.into_inner());
            for (topic, text) in publishes {
                hub.publish(topic, text);
            }
        }
        self.metrics.publish_local.fetch_add(publishes.len() as u64, Ordering::Relaxed);
        if let Some(bus) = &self.bus {
            for (topic, text) in publishes {
                bus.send(topic, text);
            }
        }
    }

    /// The text of `/metrics`: the counters, and the gauges read now.
    fn metrics_text(&self) -> String {
        self.metrics.jobs_enqueued.store(phv_std::jobs::added_count(), Ordering::Relaxed);
        let busy = self.config.workers.saturating_sub(self.workers.available_permits());
        let conns = MAX_CONNECTIONS.saturating_sub(self.conn_slots.available_permits());
        let sockets = ws::MAX_WEBSOCKETS.saturating_sub(self.ws_slots.available_permits());
        let ready = !self.stopping.load(Ordering::SeqCst) && self.metrics.health.down().is_empty();
        self.metrics.render(&[
            (
                "phv_up",
                "1 while the server runs, 0 once it is stopping.",
                f64::from(u8::from(!self.stopping.load(Ordering::SeqCst))),
            ),
            ("phv_ready", "1 when the server and the stores it uses answer.", f64::from(u8::from(ready))),
            ("phv_uptime_seconds", "Seconds since the server started.", self.started.elapsed().as_secs_f64()),
            ("phv_workers", "Scripts that may run at once ([server] workers).", self.config.workers as f64),
            ("phv_workers_busy", "Scripts running now.", busy as f64),
            ("phv_connections", "Open connections.", conns as f64),
            ("phv_websockets", "Open WebSocket connections.", sockets as f64),
            (
                "phv_output_waiting_bytes",
                "Bytes of output waiting to be sent.",
                self.buffered.load(Ordering::Relaxed) as f64,
            ),
            (
                "phv_events_waiting_bytes",
                "Bytes of event streams waiting to be sent.",
                self.events_buffered.load(Ordering::Relaxed) as f64,
            ),
            ("phv_async_running", "Operations of *Async methods running now.", phv_std::async_running() as f64),
        ])
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
    /// A production server remembers the answer and looks again at most every
    /// `RECHECK` (one request looks when that is due; the others take the last
    /// answer meanwhile), as it does for pages: every page asks for the
    /// `_middleware.phv` of each directory above it, and looking at files is
    /// slow (and serializes requests on Windows). A development server looks
    /// every time.
    fn special(&self, rel: &str) -> Option<PathBuf> {
        if let Some(e) = &self.embedded {
            return e.pages.contains_key(rel).then(|| self.public.join(rel));
        }
        // Long names (from a WebSocket request's path) are not remembered,
        // as `locate` does not remember long paths.
        if self.dev || rel.len() > FOUND_KEY_MAX {
            return self.look_for_special(rel);
        }
        let known = self.specials.lock().unwrap().get(rel).cloned();
        if let Some(e) = known {
            let (now, last) = (self.now_ms(), e.checked.load(Ordering::Relaxed));
            let due = now.saturating_sub(last) >= RECHECK.as_millis() as u64;
            if !due || e.checked.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_err() {
                return e.path.clone();
            }
        }
        let path = self.look_for_special(rel);
        let mut cache = self.specials.lock().unwrap();
        if cache.len() >= SPECIAL_KEEP {
            cache.clear();
        }
        cache.insert(rel.to_string(), Arc::new(SpecialEntry { path: path.clone(), checked: self.now_ms().into() }));
        path
    }

    /// Looks for a special page on disk (see `special`).
    fn look_for_special(&self, rel: &str) -> Option<PathBuf> {
        let path = self.public.join(rel);
        // A link to another machine is not resolved (see `resolve`).
        if phv_std::safefs::on_another_machine_below(&self.public, &path) {
            return None;
        }
        let c = path.canonicalize().ok()?;
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
    serve_with(Some(listener), opts, None, None)
}

/// `phv jobs run`: runs the site's jobs (and its admin port, if it has one)
/// without serving its pages, until Ctrl-C or SIGTERM, or `stop`.
pub fn run_jobs(
    opts: ServeOptions,
    embedded: Option<Embedded>,
    stop: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
) -> Result<(), String> {
    serve_with(None, opts, embedded, stop)
}

/// Serves until `stop` completes, then lets running requests finish.
pub fn serve_until(
    listener: std::net::TcpListener,
    opts: ServeOptions,
    stop: impl Future<Output = ()> + Send + 'static,
) -> Result<(), String> {
    serve_with(Some(listener), opts, None, Some(Box::pin(stop)))
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
    serve_with(Some(listener), opts, Some(embedded), None)
}

type Stop = Pin<Box<dyn Future<Output = ()> + Send>>;

/// The server's secret and the previous one (`[security] secret_env`, `secret_old_env`).
type Secrets = (Vec<u8>, Option<Vec<u8>>);

/// The shortest secret `[security] secret_env` may hold.
const MIN_SECRET: usize = 32;

/// Reads `[security] secret_env` and `secret_old_env`: the secret and the
/// previous one. A variable that is unset or too short stops the server; its
/// value is never shown.
fn server_secrets(
    config: &Config,
    var: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Result<Option<Secrets>, String> {
    let read = |name: &str| -> Result<Vec<u8>, String> {
        let value = var(name)
            .ok_or_else(|| format!("[security] secret_env: the environment variable {name} is not set"))?
            .into_string()
            .map_err(|_| format!("[security] secret_env: the environment variable {name} is not UTF-8 text"))?;
        if value.len() < MIN_SECRET {
            return Err(format!(
                "[security] secret_env: the environment variable {name} must hold at least {MIN_SECRET} bytes \
                 (random_token() makes one)"
            ));
        }
        Ok(value.into_bytes())
    };
    let Some(name) = &config.secret_env else { return Ok(None) };
    let secret = read(name)?;
    let old = config.secret_old_env.as_deref().map(read).transpose()?;
    Ok(Some((secret, old)))
}

/// Opens `[server] access_log` (a file inside the project) for appending,
/// making its directories: links, junctions and hard links on the way are
/// refused, so a project cannot make the server append to a file elsewhere.
fn open_access_log(root: &Path, path: &Path) -> Result<std::fs::File, String> {
    use phv_std::safefs::{open_parent, FsError};
    let describe = |e: FsError| phv_std::safefs::describe(&e);
    let rel = path.strip_prefix(root).map_err(|_| "it is not inside the project".to_string())?;
    let parts: Vec<&std::ffi::OsStr> = rel.iter().collect();
    for i in 1..parts.len() {
        let dir: PathBuf = parts[..i].iter().collect();
        let (parent, name) = open_parent(root, &dir).map_err(describe)?;
        parent.make_dir(&name).map_err(describe)?;
    }
    let (dir, name) = open_parent(root, rel).map_err(describe)?;
    dir.open_append(&name).map_err(describe)
}

fn serve_with(
    listener: Option<std::net::TcpListener>,
    opts: ServeOptions,
    embedded: Option<Embedded>,
    stop: Option<Stop>,
) -> Result<(), String> {
    let addr = match &listener {
        Some(l) => {
            l.set_nonblocking(true).map_err(|e| e.to_string())?;
            Some(l.local_addr().map_err(|e| e.to_string())?)
        }
        None => None,
    };
    let root = opts.root.canonicalize().map_err(|e| format!("cannot open {}: {e}", opts.root.display()))?;
    let config = match &embedded {
        Some(e) => {
            // A built site's jobs are those of its bundle.
            let mut names: Vec<String> = e.jobs.keys().cloned().collect();
            names.sort();
            phv_driver::config::parse_config_with_jobs(&root, e.config.as_deref().unwrap_or(""), Some(names))?
        }
        None => {
            if !root.join("public").is_dir() {
                return Err(format!("{} has no `public/` directory", root.display()));
            }
            load_config(&root)?
        }
    };
    let secrets = server_secrets(&config, |v| std::env::var_os(v))?;
    let admin = match &config.admin {
        None => None,
        Some(a) => {
            let token = match &a.token_env {
                None => None,
                Some(name) => {
                    let t = std::env::var(name)
                        .map_err(|_| format!("[admin] token_env: the environment variable {name} is not set"))?;
                    if t.len() < 16 {
                        return Err(format!(
                            "[admin] token_env: the environment variable {name} must hold at least 16 bytes"
                        ));
                    }
                    Some(t.into_bytes())
                }
            };
            let l = std::net::TcpListener::bind(a.listen)
                .map_err(|e| format!("cannot listen on the admin address {}: {e}", a.listen))?;
            l.set_nonblocking(true).map_err(|e| e.to_string())?;
            Some((l, token))
        }
    };
    let mut state = State::new(root, opts.dev, opts.time_limit, config);
    let dropped = state.metrics.telemetry_dropped.clone();
    state.telemetry = state.config.telemetry.as_ref().and_then(|t| telemetry::Exporter::start(t, dropped));
    if let Some((secret, old)) = secrets {
        state.csrf_secret = phv_std::server_key(&secret, "phv csrf v1");
        state.csrf_old = old.map(|o| phv_std::server_key(&o, "phv csrf v1"));
        state.secret = Some(secret);
    }
    if let Some(name) = &state.config.publish_store {
        let spec = state.config.host.databases.get(name).cloned();
        if let (Some(spec), Some(secret)) = (spec, &state.secret) {
            state.bus = Some(bus::Bus::start(spec, secret, state.hub.clone(), state.metrics.clone()));
        }
    }
    state.watch = opts.watch && opts.dev && state.embedded.is_none();
    if let Some((path, _)) = &state.config.access_log {
        let file = open_access_log(&state.root, path)
            .map_err(|e| format!("cannot open the access log {}: {e}", path.display()))?;
        state.log = log::Logger::start(Some(file));
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
    let log = state.log.clone();
    if listener.is_none() && state.config.jobs.is_none() {
        return Err("phv.toml has no [jobs] section: there are no jobs to run".into());
    }
    let runner = jobs::start(&state, listener.is_some())?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        // Scripts run on blocking threads; give them room for deep recursion.
        .thread_stack_size(16 * 1024 * 1024)
        .build()
        .map_err(|e| e.to_string())?;
    let result = rt.block_on(async move {
        let mut stop: Stop = stop.unwrap_or_else(|| Box::pin(os_shutdown()));
        let listener = match (listener, addr) {
            (Some(l), Some(addr)) => {
                let l = TcpListener::from_std(l).map_err(|e| format!("cannot listen on {addr}: {e}"))?;
                let mode = if state.dev { "development" } else { "production" };
                let scheme = if acceptor.is_some() { "https" } else { "http" };
                eprintln!("PHV server ({mode} mode) listening on {scheme}://{addr}");
                if state.dev && !addr.ip().is_loopback() {
                    eprintln!("warning: development mode shows error details to every visitor; use --prod");
                }
                // Copies of a project (staging, production) have the same name: on
                // one database they would share sessions, rate limits and jobs
                // (found by the 1.1 audit).
                let c = &state.config;
                let shared = c.session_db.is_some()
                    || c.host.rate_limit_store.is_some()
                    || c.jobs.as_ref().is_some_and(|j| j.store.is_some());
                if shared && !c.store_namespace_set {
                    eprintln!(
                        "note: sessions, rate limits and jobs in shared stores are kept under {}; set [security] \
                         store_namespace if another copy of this site uses the same database",
                        match &c.name {
                            Some(n) => format!("the project's name \"{}\"", phv_syntax::diag::log_escape(n)),
                            None => "no name (shared with every site without one)".to_string(),
                        }
                    );
                }
                match &state.embedded {
                    Some(e) => {
                        eprintln!("Serving the built site ({} pages, {} files)", e.pages.len(), e.statics.len())
                    }
                    None => eprintln!("Serving {}", state.public.display()),
                }
                Some(l)
            }
            _ => None,
        };
        if let Some(j) = &state.config.jobs {
            if runner.is_some() {
                eprintln!("Running jobs of {}/ ({} at once)", j.dir, j.workers);
            }
        }
        if let Some((l, token)) = admin {
            let at = l.local_addr().map_err(|e| e.to_string())?;
            let l = TcpListener::from_std(l).map_err(|e| format!("cannot listen on {at}: {e}"))?;
            eprintln!("Admin port (/health, /ready, /metrics) on http://{at}");
            tokio::spawn(admin::serve(state.clone(), l, token));
        }
        let slots = state.conn_slots.clone();
        // Tells the connections to finish their request and close.
        let (closing, closed) = tokio::sync::watch::channel(false);
        // Without a listener (`phv jobs run`), only the jobs run, until the stop.
        if listener.is_none() {
            (&mut stop).await;
        }
        while let Some(listener) = &listener {
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
                // Bytes of responses the connection has taken (see `Sending`).
                let written = Arc::new(AtomicU64::new(0));
                match acceptor {
                    Some(acceptor) => {
                        // A client that does not finish the handshake in time is dropped.
                        if let Ok(Ok(io)) = tokio::time::timeout(HEADER_TIMEOUT, acceptor.accept(io)).await {
                            if io.get_ref().1.alpn_protocol() == Some(b"h2") {
                                serve_h2(state, peer, io, closed, written).await;
                            } else {
                                serve_h1(state, peer, io, closed).await;
                            }
                        }
                    }
                    None if state.config.h2c => serve_auto(state, peer, io, closed, written).await,
                    None => serve_h1(state, peer, io, closed).await,
                }
                drop(permit);
            });
        }
        // New connections are refused; running requests get `shutdown_timeout` to finish.
        state.stopping.store(true, Ordering::SeqCst);
        if let Some(r) = &runner {
            r.stop();
        }
        drop(listener);
        let wait = Duration::from_secs(state.config.shutdown_timeout);
        eprintln!("Stopping: letting running requests finish (at most {} s)", wait.as_secs());
        let _ = closing.send(true);
        let _ = state.ws_closing.send(true);
        let all = slots.acquire_many(MAX_CONNECTIONS as u32);
        let started = Instant::now();
        let mut finished = tokio::time::timeout(wait, all).await.is_ok();
        // Jobs that are running get what is left of the time (an unfinished one runs again elsewhere).
        if let Some(r) = runner {
            let left = wait.saturating_sub(started.elapsed());
            finished &= tokio::task::spawn_blocking(move || r.wait(left)).await.unwrap_or(false);
        }
        if let Some(b) = &state.bus {
            b.stop();
        }
        // The spans not sent yet go now (a few seconds at most).
        if let Some(t) = state.telemetry.clone() {
            let _ = tokio::task::spawn_blocking(move || t.close(Duration::from_secs(5))).await;
        }
        eprintln!("{}", if finished { "Stopped" } else { "Stopped with requests or jobs still running" });
        Ok(())
    });
    // Scripts still running past the timeout are not waited for.
    rt.shutdown_timeout(Duration::from_millis(500));
    log.flush();
    result
}

/// Drops the connection a request came on (each request carries one): an
/// event stream that ends while holding output its client has not taken
/// frees it at once, with everything the connection queued (found by the
/// 1.1.1 review: it stayed counted until the connection stalled for 30 s, or
/// for as long as the client read a little now and then).
#[derive(Clone)]
struct Abort(Arc<tokio::sync::Notify>);

impl Abort {
    fn new() -> Abort {
        Abort(Arc::new(tokio::sync::Notify::new()))
    }

    /// Kept until the connection looks (`notify_one`), so it is never missed.
    fn now(&self) {
        self.0.notify_one();
    }
}

/// How long an event stream may hold output without getting back to having
/// sent all of it: as long as a write may stall (`WRITE_STALL`), unless a test
/// shortened it.
static EVENT_DRAIN_MS: AtomicU64 = AtomicU64::new(0);

fn event_drain() -> Duration {
    match EVENT_DRAIN_MS.load(Ordering::Relaxed) {
        0 => WRITE_STALL,
        ms => Duration::from_millis(ms),
    }
}

/// For tests only (not a setting): shortens the time an event stream may
/// hold output without sending all of it, for every server of this process.
#[doc(hidden)]
pub fn set_event_drain_for_tests(limit: Duration) {
    EVENT_DRAIN_MS.store(limit.as_millis().max(1) as u64, Ordering::Relaxed);
}

/// Runs connection `$conn` until it ends, until the server stops (then it
/// finishes the requests it has), or until `$abort` drops it. With `$idle`,
/// an HTTP/2 connection that has had no request for `H2_IDLE` is closed the
/// same way.
macro_rules! drive {
    ($conn:expr, $closed:expr, $idle:expr, $abort:expr) => {{
        let conn = $conn;
        tokio::pin!(conn);
        let idle: Option<Arc<Activity>> = $idle;
        let abort: Abort = $abort;
        let mut closed = $closed;
        // Responses waiting to be sent, and when bytes last went out.
        let (mut last_written, mut progress) = (0u64, Instant::now());
        loop {
            tokio::select! {
                _ = conn.as_mut() => break,
                // Dropped here: the socket, the connection's queues and its
                // streams' bodies go with it.
                _ = abort.0.notified() => break,
                _ = closed.changed() => {
                    conn.as_mut().graceful_shutdown();
                    let _ = conn.await;
                    break;
                }
                _ = tokio::time::sleep(Duration::from_secs(1)), if idle.is_some() => {
                    let Some(a) = idle.as_ref() else { break };
                    let written = a.written.load(Ordering::Relaxed);
                    if written != last_written || a.sending.load(Ordering::SeqCst) == 0 {
                        (last_written, progress) = (written, Instant::now());
                    } else if progress.elapsed() >= WRITE_STALL {
                        // Responses are waiting and the connection has taken
                        // none of their bytes for as long as an HTTP/1.1 write
                        // may stall: the client gives no flow-control window.
                        break;
                    }
                    if a.idle_for() >= H2_IDLE {
                        conn.as_mut().graceful_shutdown();
                        let _ = tokio::time::timeout(H2_GRACE, conn.as_mut()).await;
                        break;
                    }
                }
            }
        }
    }};
}

/// Requests in progress on one connection, responses still being sent, and
/// when the last of them ended; and the bytes of responses it has taken (a
/// client without a flow-control window lets it take none: pings and other
/// frames still go out, so the socket's writes do not tell).
struct Activity {
    open: AtomicUsize,
    sending: AtomicUsize,
    /// When a request or response last ended: milliseconds since `born`.
    since: AtomicU64,
    born: Instant,
    written: Arc<AtomicU64>,
}

impl Activity {
    fn new(written: Arc<AtomicU64>) -> Arc<Activity> {
        Arc::new(Activity {
            open: AtomicUsize::new(0),
            sending: AtomicUsize::new(0),
            since: AtomicU64::new(0),
            born: Instant::now(),
            written,
        })
    }

    fn touch(&self) {
        self.since.store(self.born.elapsed().as_millis() as u64, Ordering::SeqCst);
    }

    fn idle_for(&self) -> Duration {
        if self.open.load(Ordering::SeqCst) > 0 || self.sending.load(Ordering::SeqCst) > 0 {
            return Duration::ZERO;
        }
        let now = self.born.elapsed().as_millis() as u64;
        Duration::from_millis(now.saturating_sub(self.since.load(Ordering::SeqCst)))
    }
}

/// A response body counted as being sent on its connection until it is dropped.
struct Sending {
    body: RespBody,
    activity: Arc<Activity>,
}

impl Sending {
    fn new(body: RespBody, activity: &Arc<Activity>) -> Sending {
        activity.sending.fetch_add(1, Ordering::SeqCst);
        Sending { body, activity: activity.clone() }
    }
}

impl Drop for Sending {
    fn drop(&mut self) {
        self.activity.touch();
        self.activity.sending.fetch_sub(1, Ordering::SeqCst);
    }
}

impl hyper::body::Body for Sending {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Bytes>, std::io::Error>>> {
        let r = Pin::new(&mut self.body).poll_frame(cx);
        if let std::task::Poll::Ready(Some(Ok(f))) = &r {
            if let Some(d) = f.data_ref() {
                self.activity.written.fetch_add(d.len() as u64 + 1, Ordering::Relaxed);
            }
        }
        r
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        self.body.size_hint()
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
        self.0.touch();
        self.0.open.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The options of HTTP/1.1 connections, on a builder of either kind.
macro_rules! h1_options {
    ($b:expr) => {
        $b.timer(TokioTimer::new()).header_read_timeout(HEADER_TIMEOUT).max_buf_size(CONN_BUFFER)
    };
}

/// The options of HTTP/2 connections, on a builder of either kind.
macro_rules! h2_options {
    ($b:expr) => {
        $b.timer(TokioTimer::new())
            .max_concurrent_streams(H2_STREAMS)
            .max_header_list_size(CONN_BUFFER as u32)
            .initial_stream_window_size(H2_WINDOW)
            .initial_connection_window_size(H2_WINDOW)
            .adaptive_window(false)
            .max_send_buf_size(H2_SEND_BUF)
            .max_pending_accept_reset_streams(20)
            .max_local_error_reset_streams(256)
            .keep_alive_interval(Some(Duration::from_secs(30)))
            .keep_alive_timeout(Duration::from_secs(20))
    };
}

fn h1_builder() -> http1::Builder {
    let mut b = http1::Builder::new();
    h1_options!(b);
    b
}

fn h2_builder() -> http2::Builder<TokioExecutor> {
    let mut b = http2::Builder::new(TokioExecutor::new());
    h2_options!(b);
    b
}

type Closed = tokio::sync::watch::Receiver<bool>;

async fn serve_h1<I>(state: Arc<State>, peer: SocketAddr, io: I, closed: Closed)
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let abort = Abort::new();
    let a = abort.clone();
    let service = service_fn(move |mut req: Request<Incoming>| {
        req.extensions_mut().insert(a.clone());
        handle(state.clone(), peer, req)
    });
    drive!(h1_builder().serve_connection(TokioIo::new(io), service).with_upgrades(), closed, None, abort)
}

async fn serve_h2<I>(state: Arc<State>, peer: SocketAddr, io: I, closed: Closed, written: Arc<AtomicU64>)
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let activity = Activity::new(written);
    let a = activity.clone();
    let abort = Abort::new();
    let ab = abort.clone();
    let service = service_fn(move |mut req: Request<Incoming>| {
        req.extensions_mut().insert(ab.clone());
        let open = OpenRequest::new(&a);
        let (state, a) = (state.clone(), a.clone());
        async move {
            let r = handle(state, peer, req).await;
            drop(open);
            r.map(|resp| resp.map(|b| Sending::new(b, &a)))
        }
    });
    drive!(h2_builder().serve_connection(TokioIo::new(io), service), closed, Some(activity), abort)
}

/// `[server] h2c`: HTTP/1.1, or HTTP/2 when the client starts with its preface.
async fn serve_auto<I>(state: Arc<State>, peer: SocketAddr, io: I, closed: Closed, written: Arc<AtomicU64>)
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let activity = Activity::new(written);
    let a = activity.clone();
    let abort = Abort::new();
    let ab = abort.clone();
    let service = service_fn(move |mut req: Request<Incoming>| {
        req.extensions_mut().insert(ab.clone());
        let open = OpenRequest::new(&a);
        let (state, a) = (state.clone(), a.clone());
        async move {
            let r = handle(state, peer, req).await;
            drop(open);
            r.map(|resp| resp.map(|b| Sending::new(b, &a)))
        }
    });
    let mut b = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
    h1_options!(b.http1());
    h2_options!(b.http2());
    drive!(b.serve_connection_with_upgrades(TokioIo::new(io), service), closed, Some(activity), abort)
}

type Resp = Response<RespBody>;

/// What a WebSocket event's script left: the connection's kept values (a
/// response extension, read by `ws.rs`).
#[derive(Clone)]
struct WsDone(WsCtx);

/// HTTP/2 sends the host as `:authority` and may split the cookies into
/// several fields: the server and scripts see them as HTTP/1.1 sends them.
fn as_http1_headers(mut req: Request<Incoming>) -> Result<Request<Incoming>, Box<Resp>> {
    // The authority of the request target (HTTP/2's `:authority`, or an
    // absolute target in HTTP/1.1) is the host: a `host` field that says
    // otherwise is replaced (found by the 1.0 audit, and its ways around by
    // the 1.0.0 one). One with user information (`user@host`) is refused.
    let authority = req.uri().authority().map(|a| a.as_str().to_string());
    if let Some(a) = authority {
        let bad =
            || Box::new(simple(StatusCode::BAD_REQUEST, "text/plain", "bad request: invalid request target".into()));
        if a.contains('@') {
            return Err(bad());
        }
        let v = HeaderValue::from_str(&a).map_err(|_| bad())?;
        req.headers_mut().insert(hyper::header::HOST, v);
    }
    if req.version() != hyper::Version::HTTP_2 {
        return Ok(req);
    }
    let cookies: Vec<&[u8]> = req.headers().get_all(hyper::header::COOKIE).iter().map(|v| v.as_bytes()).collect();
    if cookies.len() > 1 {
        let joined = cookies.join(&b"; "[..]);
        if let Ok(v) = HeaderValue::from_bytes(&joined) {
            req.headers_mut().insert(hyper::header::COOKIE, v);
        }
    }
    Ok(req)
}

async fn handle(state: Arc<State>, peer: SocketAddr, req: Request<Incoming>) -> Result<Resp, Infallible> {
    let req = match as_http1_headers(req) {
        Ok(req) => req,
        Err(bad) => return Ok(*bad),
    };
    // Two `Host` fields: the server and a proxy in front may read different ones.
    if req.headers().get_all(hyper::header::HOST).iter().count() > 1 {
        return Ok(simple(StatusCode::BAD_REQUEST, "text/plain", "bad request: more than one Host".into()));
    }
    let start = Instant::now();
    let mut req = req;
    let version = req.version();
    let id = (state.config.request_ids || state.telemetry.is_some()).then(|| {
        request_id(req.headers(), state.config.trust_request_id, state.config.request_ids, state.telemetry.as_deref())
    });
    if let Some(id) = &id {
        req.extensions_mut().insert(id.clone());
    }
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
        finish(&state, &mut r, id.as_ref(), &ip, &method, &path, start, version);
        return Ok(r);
    }
    let limited = rate_limited(&state, peer.ip()).await;
    let mut resp = if let Some(wait) = limited {
        state.metrics.reject(metrics::Reject::RateLimit);
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
    finish(&state, &mut resp, id.as_ref(), &ip, &method, &path, start, version);
    Ok(resp)
}

/// What answered a request, for `/metrics` (a response extension).
#[derive(Clone)]
struct Served {
    kind: metrics::Kind,
    route: Option<String>,
}

/// The id of a request and its W3C trace context (`[server] request_ids`, `[telemetry]`).
#[derive(Clone)]
struct RequestId {
    id: String,
    /// The trace (32 hex digits) and the flags of `traceparent`.
    trace: String,
    flags: String,
    /// This request's span (16 hex digits), and the span of the caller that
    /// a trusted `traceparent` named.
    span: String,
    parent: Option<String>,
    /// `[server] request_ids`: the id is shown (`X-Request-Id`, the log, `request_id()`).
    shown: bool,
}

impl RequestId {
    /// The `traceparent` the request's calls to other services send: this
    /// request's trace, with a span of its own.
    fn traceparent(&self) -> String {
        format!("00-{}-{}-{}", self.trace, phv_std::random_hex(8), self.flags)
    }

    /// Whether the trace is recorded (the sampled flag).
    fn sampled(&self) -> bool {
        u8::from_str_radix(&self.flags, 16).is_ok_and(|f| f & 1 == 1)
    }
}

/// A new id for a request, or with `trust` the one the proxy in front sent
/// (`X-Request-Id`, and the trace of `traceparent`), when it is well formed.
/// A new trace is sampled as `[telemetry] sample` says.
fn request_id(
    headers: &hyper::HeaderMap,
    trust: bool,
    shown: bool,
    exporter: Option<&telemetry::Exporter>,
) -> RequestId {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::trim);
    let caller = trust.then(|| header("traceparent")).flatten().and_then(parse_traceparent);
    let given = trust.then(|| header("x-request-id")).flatten().filter(|v| {
        (1..=128).contains(&v.len()) && v.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    });
    let (trace, flags, parent) = match caller {
        Some((trace, span, flags)) => (trace, flags, Some(span)),
        None => {
            let sampled = exporter.is_none_or(|e| e.sample());
            (phv_std::random_hex(16), if sampled { "01" } else { "00" }.to_string(), None)
        }
    };
    let id = given.map(str::to_string).unwrap_or_else(|| trace.clone());
    RequestId { id, trace, flags, span: phv_std::random_hex(8), parent, shown }
}

/// The trace id, span id and flags of a W3C `traceparent` (version 00), if it is valid.
fn parse_traceparent(v: &str) -> Option<(String, String, String)> {
    let parts: Vec<&str> = v.split('-').collect();
    let hex = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    let [version, trace, span, flags] = parts[..] else { return None };
    let ok = version == "00"
        && hex(trace, 32)
        && hex(span, 16)
        && hex(flags, 2)
        && trace.bytes().any(|b| b != b'0')
        && span.bytes().any(|b| b != b'0');
    ok.then(|| (trace.to_string(), span.to_string(), flags.to_string()))
}

/// The last steps of every request: its id in the response, the log line,
/// and the metrics.
#[allow(clippy::too_many_arguments)]
fn finish(
    state: &State,
    resp: &mut Resp,
    id: Option<&RequestId>,
    ip: &str,
    method: &Method,
    path: &str,
    start: Instant,
    version: hyper::Version,
) {
    let shown = id.filter(|i| i.shown);
    if let Some(v) = shown.and_then(|i| HeaderValue::from_str(&i.id).ok()) {
        resp.headers_mut().insert("x-request-id", v);
    }
    let status = resp.status().as_u16();
    log_request(state, ip, method, path, status, start, shown.map(|i| i.id.as_str()));
    let served = resp.extensions_mut().remove::<Served>();
    let (kind, route) = match &served {
        Some(s) => (s.kind, s.route.as_deref()),
        None => (metrics::Kind::Other, None),
    };
    // A WebSocket's time is that of its upgrade, not of the connection.
    state.metrics.request(kind, route, status, start.elapsed());
    // The request's span: until its head is sent (a streamed page goes on after it).
    if let (Some(exporter), Some(id)) = (&state.telemetry, id.filter(|i| i.sampled())) {
        let end = std::time::SystemTime::now();
        let route = match kind {
            metrics::Kind::Static => Some("static"),
            _ => route,
        };
        // A standard method or `_OTHER`: a request may name any (found by the 1.2 audit).
        let method = phv_vm::Span::method(method.as_str());
        let mut attributes = vec![
            ("http.request.method", phv_vm::SpanValue::Str(method.into())),
            ("http.response.status_code", phv_vm::SpanValue::Int(status as i64)),
            ("url.scheme", phv_vm::SpanValue::Str(if state.config.tls.is_some() { "https" } else { "http" }.into())),
            ("phv.kind", phv_vm::SpanValue::Str(kind.label().into())),
        ];
        let proto = match version {
            hyper::Version::HTTP_2 => Some("2"),
            hyper::Version::HTTP_11 => Some("1.1"),
            hyper::Version::HTTP_10 => Some("1.0"),
            _ => None,
        };
        if let Some(p) = proto {
            attributes.push(("network.protocol.version", phv_vm::SpanValue::Str(p.into())));
        }
        if let Some(r) = route {
            attributes.push(("http.route", phv_vm::SpanValue::Str(r.to_string())));
        }
        if status >= 500 {
            attributes.push(("error.type", phv_vm::SpanValue::Str(status.to_string())));
        }
        exporter.send(vec![phv_vm::Span {
            trace_id: id.trace.clone(),
            span_id: id.span.clone(),
            parent_id: id.parent.clone(),
            // No path: it may hold ids and names (the route of a page has none).
            name: {
                let method = if method == "_OTHER" { "HTTP" } else { method };
                match route {
                    Some(r) => format!("{method} {r}"),
                    None => method.to_string(),
                }
            },
            kind: phv_vm::SpanKind::Server,
            start: end.checked_sub(start.elapsed()).unwrap_or(end),
            end,
            attributes,
            error: status >= 500,
        }]);
    }
}

/// `[security] requests_per_minute`: counts one request (or WebSocket
/// message) of the client at `ip`; the seconds to wait when it is over.
async fn rate_limited(state: &State, ip: std::net::IpAddr) -> Option<u64> {
    let limit = state.config.requests_per_minute;
    let client = format!("ip:{}", phv_std::ratelimit::client_key(ip));
    let minute = Duration::from_secs(60);
    if limit == 0 {
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
            Ok(Ok((allowed, wait))) => {
                state.metrics.health.ok(metrics::StoreKind::RateLimit);
                (!allowed).then_some(wait)
            }
            Ok(Err(e)) => {
                state.metrics.health.failed(metrics::StoreKind::RateLimit);
                eprintln!("warning: the shared rate limit store cannot be used ({e}); counting in this process");
                (!phv_std::ratelimit::hit(&client, limit, minute))
                    .then(|| phv_std::ratelimit::retry_after(&client, minute))
            }
            Err(_) => None,
        }
    }
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
    // One request compresses a version of a static file; the others wait for
    // it and share its copy (found by the 1.0.0 audit: on a cold cache each
    // response compressed and held a copy of its own).
    let gate = tag.as_ref().map(|t| state.gz_flight.lock().unwrap().entry(t.clone()).or_default().clone());
    let _compressing = match &gate {
        Some(g) => Some(g.lock().await),
        None => None,
    };
    if let Some(gz) = tag.as_ref().and_then(|t| state.gz_cache.lock().unwrap().0.get(t).cloned()) {
        *resp.body_mut() = RespBody::full(gz);
        static_files::mark_gzip(resp.headers_mut());
        return resp;
    }
    let done = |state: &State| {
        if let Some(t) = &tag {
            state.gz_flight.lock().unwrap().remove(t);
        }
    };
    let Some(bytes) = resp.body_mut().take_bytes() else {
        done(state);
        return resp;
    };
    // A static file is compressed only when the copy can be kept (one served
    // from memory reaches here too; found by the 1.0.0 audit).
    if is_static && !state.gz_room(bytes.len()) {
        done(state);
        *resp.body_mut() = RespBody::full(bytes);
        return resp;
    }
    let max = if is_static { MAX_STATIC_COMPRESS as usize } else { static_files::MAX_COMPRESS };
    let packed = if (1024..=max).contains(&bytes.len()) {
        let data = bytes.clone();
        tokio::task::spawn_blocking(move || static_files::gzip(&data)).await.ok().flatten()
    } else {
        None
    };
    match packed {
        Some(gz) => {
            // A bounded cache, kept while there is room (emptying it would let a
            // visitor make a new copy per response). A copy that is not kept
            // counts as output, as a page's compressed output does.
            let mut kept = None;
            if let Some(t) = &tag {
                let mut cache = state.gz_cache.lock().unwrap();
                if cache.1 + gz.len() <= GZ_CACHE {
                    let gz = Bytes::from(gz.clone());
                    cache.1 += gz.len();
                    if let Some(old) = cache.0.insert(t.clone(), gz.clone()) {
                        cache.1 -= old.len();
                    }
                    kept = Some(gz);
                }
            }
            done(state);
            match kept {
                Some(gz) => *resp.body_mut() = RespBody::full(gz),
                // A static file whose copy cannot be kept is sent as it is.
                None if is_static => {
                    *resp.body_mut() = RespBody::full(bytes);
                    return resp;
                }
                None => *resp.body_mut() = RespBody::counted(gz, &state.buffered),
            }
            static_files::mark_gzip(resp.headers_mut());
        }
        None => {
            done(state);
            *resp.body_mut() = RespBody::full(bytes);
        }
    }
    resp
}

/// The most bytes of a request path in a log line.
const LOG_PATH_MAX: usize = 2048;

/// `s` cut to at most `max` bytes (at a character boundary), marked with `...`.
fn clip(s: &str, max: usize) -> std::borrow::Cow<'_, str> {
    if s.len() <= max {
        return s.into();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end]).into()
}

/// One line on stderr, and in the access log when phv.toml sets one.
/// The line of a request on standard error, and in the access log. The id
/// (`[server] request_ids`) goes to the JSON access log only, so the text
/// formats stay as they were.
fn log_request(state: &State, ip: &str, method: &Method, path: &str, status: u16, start: Instant, id: Option<&str>) {
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    // A path of a request may be 64 KB: lines show its start (found by the
    // 1.0.0 audit: the queue of lines then held gigabytes for a slow stderr).
    let path = clip(path, LOG_PATH_MAX);
    // An extension method may be as long as a path (found by the 1.0.0 audit too).
    let method = clip(method.as_str(), 32);
    let line = format!("{method} {path} {status} {ms:.1}ms\n");
    let format = state.config.access_log.as_ref().map(|(_, f)| f).filter(|_| state.log.has_file());
    let Some(format) = format else { return state.log.line(line, None) };
    let time = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let access = match format {
        LogFormat::Text => {
            let path = path.replace('\\', "\\\\").replace('"', "\\\"");
            format!("{time} {ip} \"{method} {path}\" {status} {ms:.1}ms\n")
        }
        LogFormat::Json => {
            let mut v = serde_json::json!({
                "time": time,
                "ip": ip,
                "method": &*method,
                "path": path,
                "status": status,
                "ms": (ms * 10.0).round() / 10.0,
            });
            if let Some(id) = id {
                v["id"] = serde_json::Value::from(id);
            }
            format!("{v}\n")
        }
    };
    state.log.line(line, Some(access));
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
    // Constant values are not parsed per response.
    let mut set = |name: &'static str, value: HeaderValue| {
        if !h.contains_key(name) {
            h.insert(name, value);
        }
    };
    set("x-content-type-options", HeaderValue::from_static("nosniff"));
    set("x-frame-options", HeaderValue::from_static("SAMEORIGIN"));
    set("referrer-policy", HeaderValue::from_static("strict-origin-when-cross-origin"));
    if state.config.secure_cookies {
        set("strict-transport-security", HeaderValue::from_static("max-age=31536000"));
    }
    // Pages set their own policy with a nonce; everything else gets one without.
    if let Some(csp) = &state.csp_plain {
        if !h.contains_key("content-security-policy") {
            h.insert("content-security-policy", csp.clone());
        }
    }
}

#[derive(Clone)]
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
    let rel = url_rel(url_path)?;
    let segments: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
    if let Some(e) = &state.embedded {
        return resolve_embedded(state, e, &rel, &segments);
    }
    let candidates = page_candidates(&rel, ".phv");
    // The file `rel` names, only if its canonical path inside public/ is spelled exactly `rel`.
    let exact = |rel: &str| -> Option<PathBuf> {
        let path = state.public.join(rel);
        // A link to another machine is not resolved: that would contact the
        // host at each request (found by the 15th assessment).
        if phv_std::safefs::on_another_machine_below(&state.public, &path) {
            return None;
        }
        let c = path.canonicalize().ok()?;
        let inner = c.strip_prefix(&state.public).ok()?;
        let same = inner.components().map(|x| x.as_os_str().to_str()).eq(rel.split('/').map(Some));
        (same && c.is_file()).then_some(c)
    };
    let name_of = |p: &Path| p.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    for c in candidates {
        if let Some(p) = exact(&c) {
            let n = name_of(&p);
            if !n.ends_with(".phv.html") && !n.ends_with(".ws.phv") {
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

/// The WebSocket script (`public/<path>.ws.phv`) for an upgrade request to
/// `url_path`, with the parameters of its path; the same rules as `resolve`.
fn resolve_ws(state: &State, url_path: &str) -> Option<(PathBuf, Vec<(String, String)>)> {
    let rel = url_rel(url_path)?;
    let segments: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
    let candidates = page_candidates(&rel, ".ws.phv");
    if let Some(e) = &state.embedded {
        if let Some(c) = candidates.into_iter().find(|c| e.pages.contains_key(c)) {
            return Some((state.public.join(c), Vec::new()));
        }
    } else {
        for c in candidates {
            if let Some(p) = state.special(&c) {
                return Some((p, Vec::new()));
            }
        }
    }
    let table = state.routes().ok()?;
    let (file, params) = table.find_ws(&segments)?;
    match &state.embedded {
        Some(_) => Some((state.public.join(file), params)),
        None => state.special(file).map(|p| (p, params)),
    }
}

/// The path of a request as a name under `public/`: decoded, with its
/// segments joined by `/`. `None` when it could escape `public/` or names a
/// hidden (`.x`) or private (`_x`) file.
fn url_rel(url_path: &str) -> Option<String> {
    let decoded = percent_encoding::percent_decode_str(url_path).decode_utf8().ok()?;
    if decoded.contains(['\0', '\\', ':']) {
        return None;
    }
    let segments: Vec<&str> = decoded.split('/').filter(|s| !s.is_empty()).collect();
    if segments.iter().any(|s| s.starts_with('.') || s.starts_with('_')) {
        return None;
    }
    Some(segments.join("/"))
}

/// The files a path names, in the order they are tried: `index<ext>` for the
/// root; the path itself when it is a page (`ext` `.phv`); else `<rel><ext>`
/// and `<rel>/index<ext>`.
fn page_candidates(rel: &str, ext: &str) -> Vec<String> {
    if rel.is_empty() {
        vec![format!("index{ext}")]
    } else if ext == ".phv" && rel.ends_with(".phv") {
        vec![rel.to_string()]
    } else {
        vec![format!("{rel}{ext}"), format!("{rel}/index{ext}")]
    }
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

/// What the path of a request leads to.
#[derive(Clone)]
enum Found {
    /// The routes of `public/` conflict.
    RouteError(String),
    /// A WebSocket script, for an upgrade request.
    Ws(PathBuf, Vec<(String, String)>),
    Target(Target),
    /// Nothing; the site's `_404.phv`, if it has one.
    Nothing(Option<PathBuf>),
}

/// Finds what `url_path` leads to. It looks at files: it runs on a blocking
/// thread (except for a built site, whose files are in memory).
fn find(state: &State, url_path: &str, upgrade: bool) -> Found {
    if let Err(e) = state.routes() {
        return Found::RouteError(e);
    }
    if upgrade {
        if let Some((script, params)) = resolve_ws(state, url_path) {
            return Found::Ws(script, params);
        }
    }
    match resolve(state, url_path) {
        Some(t) => Found::Target(t),
        None => Found::Nothing(state.special("_404.phv")),
    }
}

async fn route(state: &Arc<State>, ip: String, req: Request<Incoming>) -> Resp {
    let mut r = route_inner(state, ip, req).await;
    if r.extensions().get::<Served>().is_none() {
        let kind = if r.status() == StatusCode::SWITCHING_PROTOCOLS { metrics::Kind::Ws } else { metrics::Kind::Other };
        r.extensions_mut().insert(Served { kind, route: None });
    }
    r
}

async fn route_inner(state: &Arc<State>, ip: String, req: Request<Incoming>) -> Resp {
    // `TRACE` (an echo of the request, for debugging) and `CONNECT` (a tunnel)
    // are not for pages: they are refused before any page runs (found by the
    // 1.1.1 audit: `TRACE` ran the page and answered 200).
    if matches!(*req.method(), Method::TRACE | Method::CONNECT) {
        return method_not_allowed();
    }
    let gzip_ok = state.config.compress && static_files::accepts_gzip(req.headers());
    let get = req.method() == Method::GET || req.method() == Method::HEAD;
    let as_static = |mut r: Resp| {
        r.extensions_mut().insert(Served { kind: metrics::Kind::Static, route: None });
        r
    };
    if get {
        if let Some(r) = cached_static(state, &req).await {
            return as_static(r);
        }
    }
    let found = locate(state, req.uri().path(), ws::is_upgrade(&req)).await;
    // Server-Sent Events: a path no page answers, whose `*.ws.phv` does.
    if matches!(found, Found::Nothing(_)) && ws::wants_events(&req) {
        if let Found::Ws(script, params) = locate(state, req.uri().path(), true).await {
            return ws::events(state, script, params, ip, req).await;
        }
    }
    match found {
        Found::RouteError(e) => {
            eprintln!("error in the routes of public/: {e}");
            state.error_page("Route error", &e)
        }
        Found::Ws(script, params) => ws::upgrade(state, script, params, ip, req).await,
        Found::Nothing(Some(page)) => run_script(state, page, Vec::new(), ip, req, Page::NotFound).await,
        Found::Nothing(None) => not_found(),
        Found::Target(Target::Embedded(bytes, ct, tag)) => {
            if !get {
                return method_not_allowed();
            }
            let v = Validators { etag: tag, modified: None };
            // Labelled by its name: `compress` keeps one copy per file however
            // the path is spelled (found by the 1.0.0 audit).
            let label = url_rel(req.uri().path()).unwrap_or_default();
            as_static(bytes_response(state, req.headers(), bytes, ct, &v, &label))
        }
        Found::Target(Target::Static(p)) => {
            if !get {
                return method_not_allowed();
            }
            as_static(serve_static(state, &req, p, gzip_ok).await)
        }
        Found::Target(Target::Script(p, params)) => run_script(state, p, params, ip, req, Page::Normal).await,
    }
}

/// The most request paths a production server remembers what they led to
/// (forgotten all at once when full).
const FOUND_KEEP: usize = 10_000;

/// What a request path led to, and when that was last found: milliseconds
/// since `State::started`.
struct FoundEntry {
    found: Found,
    checked: AtomicU64,
}

/// The most special pages a production server remembers having looked for
/// (forgotten all at once when full: the names come from the directories of
/// the site's pages, but the map must not grow without bound).
const SPECIAL_KEEP: usize = 4096;

/// Where a special page was (or that there was none), and when that was last
/// looked at: milliseconds since `State::started`.
struct SpecialEntry {
    path: Option<PathBuf>,
    checked: AtomicU64,
}

/// `find`, run on a blocking thread. A production server remembers what a
/// path leads to and looks again at most every `RECHECK` (a request that
/// finds the answer due looks, the others take the last one meanwhile), as it
/// does for the files of compiled pages: looking at files is slow (and
/// serializes requests on Windows). A development server looks every time.
async fn locate(state: &Arc<State>, url_path: &str, upgrade: bool) -> Found {
    if state.embedded.is_some() {
        return find(state, url_path, upgrade);
    }
    // Long paths are not remembered (found by the 1.0.0 audit: 10,000 paths of
    // 64 KB each held 680 MB).
    let key = (!state.dev && url_path.len() <= FOUND_KEY_MAX)
        .then(|| format!("{}{url_path}", if upgrade { "ws " } else { "" }));
    if let Some(k) = &key {
        let known = state.found_cache.lock().unwrap().get(k).cloned();
        if let Some(e) = known {
            let (now, last) = (state.now_ms(), e.checked.load(Ordering::Relaxed));
            let due = now.saturating_sub(last) >= RECHECK.as_millis() as u64;
            if !due || e.checked.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_err() {
                return e.found.clone();
            }
        }
    }
    let (st, path) = (state.clone(), url_path.to_string());
    let found = match tokio::task::spawn_blocking(move || find(&st, &path, upgrade)).await {
        Ok(found) => found,
        Err(_) => return Found::RouteError("the request could not be routed".into()),
    };
    if let Some(k) = key {
        if !matches!(found, Found::RouteError(_)) {
            let mut cache = state.found_cache.lock().unwrap();
            if cache.len() >= FOUND_KEEP {
                cache.clear();
            }
            cache.insert(k, Arc::new(FoundEntry { found: found.clone(), checked: state.now_ms().into() }));
        }
    }
    found
}

fn method_not_allowed() -> Resp {
    simple(StatusCode::METHOD_NOT_ALLOWED, "text/plain", "method not allowed".into())
}

/// Static files at most this large are kept in memory by a production server
/// (one copy, shared by every response); others are streamed as they are sent.
const STATIC_KEEP: u64 = 256 * 1024;

/// The most bytes of static files a production server keeps in memory. Files
/// are kept while there is room (the cache is not emptied to make room: a
/// visitor going through more files than fit would get a new copy of each per
/// response); the others are streamed.
const STATIC_KEEP_TOTAL: usize = 32 * 1024 * 1024;

/// Request paths longer than this are not remembered by `locate`.
const FOUND_KEY_MAX: usize = 1024;

/// A small static file a production server keeps in memory. It is looked at
/// again at most every `RECHECK`, as compiled pages are.
struct StaticEntry {
    path: PathBuf,
    data: Bytes,
    ct: &'static str,
    v: Validators,
    /// When it was last looked at: milliseconds since `State::started`.
    checked: AtomicU64,
}

/// A static file opened to be sent.
enum Opened {
    File {
        file: std::fs::File,
        len: u64,
        modified: Option<std::time::SystemTime>,
        /// The whole content, when it was read at once.
        data: Option<Vec<u8>>,
    },
    TooLarge,
    Missing,
}

/// Opens the file at `p`, a path inside `public` found earlier (up to a second
/// earlier in production): a link or junction put on the way since then must
/// not lead outside public/ (found by the 1.0.0 audit).
///
/// On Unix the path is walked from `public` one directory at a time without
/// following links, and the file is opened without blocking, so a FIFO is
/// refused as not a file (found by the 1.1.1 review: the name was looked up
/// twice after the file was opened, and opening a FIFO stopped a thread).
#[cfg(unix)]
fn open_beneath(public: &Path, p: &Path) -> Option<std::fs::File> {
    let rel = p.strip_prefix(public).ok()?;
    let (dir, name) = phv_std::safefs::open_parent(public, rel).ok()?;
    dir.open(&name, false).ok()
}

/// On Windows the handle's own path must still be `p`.
#[cfg(not(unix))]
fn open_beneath(_public: &Path, p: &Path) -> Option<std::fs::File> {
    let file = std::fs::File::open(p).ok()?;
    phv_std::safefs::opened_at(&file, p).ok()?;
    Some(file)
}

/// Opens static file `p` beneath `public` (a blocking call): refused when it
/// has another name too, or when it is no longer the file at `p`; read at once
/// when it has at most `read_up_to` bytes.
fn open_static(public: &Path, p: &Path, read_up_to: u64) -> Opened {
    use std::io::Read;
    let Some(file) = open_beneath(public, p) else { return Opened::Missing };
    // A file with another name (a hard link) may be one kept outside
    // public/, such as the TLS key (found by the 1.0 audit).
    if phv_std::safefs::single_file(&file).is_err() {
        return Opened::Missing;
    }
    let (len, modified) = match file.metadata() {
        Ok(m) if m.is_file() => (m.len(), m.modified().ok()),
        _ => return Opened::Missing,
    };
    if len > MAX_STATIC {
        return Opened::TooLarge;
    }
    let data = if len <= read_up_to {
        let mut data = Vec::with_capacity(len as usize);
        if (&file).take(len).read_to_end(&mut data).is_err() || data.len() as u64 != len {
            return Opened::Missing;
        }
        Some(data)
    } else {
        None
    };
    Opened::File { file, len, modified, data }
}

/// A static file in memory, or the part of it a `Range` asks for (or 304).
fn bytes_response(
    state: &State,
    headers: &hyper::HeaderMap,
    bytes: Bytes,
    ct: &'static str,
    v: &Validators,
    label: &str,
) -> Resp {
    let len = bytes.len() as u64;
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
    static_headers(state, &mut r, ct, v, label);
    r
}

/// Sends static file `p`. Its checks and reads run in one blocking call.
///
/// A file is read at once only when one copy serves many responses: a small
/// file a production server keeps, or a text file a client takes gzipped
/// (compressed once per version, and counted as output). A range is always
/// streamed from the file, so a response holds its buffer, never the file
/// (found by the 1.0.0 audit: stalled range requests each held a whole file).
async fn serve_static(state: &Arc<State>, req: &Request<Incoming>, p: PathBuf, gzip_ok: bool) -> Resp {
    let ct = content_type(&p).unwrap_or("application/octet-stream");
    let ranged = req.headers().contains_key(hyper::header::RANGE);
    // Read at once to be compressed only when the copy can be kept (found by the
    // 1.0.0 audit: with the cache full, each response held a copy of its own);
    // a copy already kept is still sent (see `compress`).
    let gzipped = !ranged && gzip_ok && static_files::compressible(ct) && state.gz_room(MAX_STATIC_COMPRESS as usize);
    let keepable = !ranged && !state.dev && state.embedded.is_none() && state.static_room(STATIC_KEEP as usize);
    let read_up_to = match (gzipped, keepable) {
        (true, _) => MAX_STATIC_COMPRESS,
        (false, true) => STATIC_KEEP,
        (false, false) => 0,
    };
    let q = p.clone();
    let public = state.public.clone();
    let Ok(opened) = tokio::task::spawn_blocking(move || open_static(&public, &q, read_up_to)).await else {
        return not_found();
    };
    let (file, len, modified, data) = match opened {
        Opened::File { file, len, modified, data } => (file, len, modified, data),
        Opened::TooLarge => {
            return simple(StatusCode::PAYLOAD_TOO_LARGE, "text/plain", "file too large to serve".into())
        }
        Opened::Missing => return not_found(),
    };
    let v = Validators::of_file(len, modified);
    let label = p.to_string_lossy().into_owned();
    if let Some(data) = data {
        let key = url_rel(req.uri().path()).filter(|_| keepable && len <= STATIC_KEEP);
        let bytes = match key {
            Some(key) if state.static_room(data.len()) => {
                let bytes = Bytes::from(data);
                let entry =
                    StaticEntry { path: p, data: bytes.clone(), ct, v: v.clone(), checked: state.now_ms().into() };
                state.keep_static(key, entry);
                bytes
            }
            _ => body::counted_bytes(data, &state.buffered),
        };
        return bytes_response(state, req.headers(), bytes, ct, &v, &label);
    }
    // Large files are streamed: memory does not grow with the file or the downloads.
    let mut file = tokio::fs::File::from_std(file);
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
            None => Response::new(RespBody::file(file, len)),
        }
    };
    static_headers(state, &mut r, ct, &v, &label);
    r
}

/// A static file a production server keeps in memory, when it is still the
/// one the path names: looked at again (in a blocking call) when it was last
/// looked at more than `RECHECK` ago.
async fn cached_static(state: &Arc<State>, req: &Request<Incoming>) -> Option<Resp> {
    if state.dev || state.embedded.is_some() {
        return None;
    }
    let key = url_rel(req.uri().path())?;
    let entry = state.static_cache.lock().unwrap().0.get(&key).cloned()?;
    let (now, last) = (state.now_ms(), entry.checked.load(Ordering::Relaxed));
    if now.saturating_sub(last) >= RECHECK.as_millis() as u64
        && entry.checked.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_ok()
    {
        let (st, path, e) = (state.clone(), req.uri().path().to_string(), entry.clone());
        let same = tokio::task::spawn_blocking(move || still_kept(&st, &path, &e)).await.unwrap_or(false);
        if !same {
            let mut cache = state.static_cache.lock().unwrap();
            if cache.0.get(&key).is_some_and(|e| Arc::ptr_eq(e, &entry)) {
                cache.0.remove(&key);
                cache.1 -= entry.data.len();
            }
            return None;
        }
    }
    let label = entry.path.to_string_lossy();
    Some(bytes_response(state, req.headers(), entry.data.clone(), entry.ct, &entry.v, &label))
}

/// Whether the path still names the kept file, with the same content (the
/// same checks as when it was first sent).
fn still_kept(state: &State, url_path: &str, e: &StaticEntry) -> bool {
    matches!(resolve(state, url_path), Some(Target::Static(p)) if p == e.path)
        && matches!(open_static(&state.public, &e.path, STATIC_KEEP), Opened::File { data: Some(d), .. } if d == e.data)
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
    /// A message or the closing of a WebSocket connection (its opening is
    /// `Normal`: middleware runs for it).
    Event,
}

/// `resolve` for a built site: its pages are known by their paths under
/// `public/`, which `State::compiled` looks up again.
fn resolve_embedded(state: &State, e: &Embedded, rel: &str, segments: &[&str]) -> Option<Target> {
    let candidates = page_candidates(rel, ".phv");
    if let Some(page) = candidates.into_iter().find(|c| e.pages.contains_key(c) && !routes::is_ws(c)) {
        return Some(Target::Script(state.public.join(page), Vec::new()));
    }
    let Some(bytes) = e.statics.get(rel) else {
        let table = state.routes().ok()?;
        let (file, params) = table.find(segments)?;
        return Some(Target::Script(state.public.join(file), params));
    };
    let lower = rel.to_ascii_lowercase();
    if lower.ends_with(".phv") || lower.ends_with(".phv.html") {
        return None;
    }
    let tag = state.embedded_tags.get(rel).cloned().unwrap_or_else(|| Validators::of_bytes(bytes).etag);
    Some(Target::Embedded(bytes.clone(), content_type(Path::new(rel))?, tag))
}

/// A compiled page in the cache.
struct CachedPage {
    compiled: Arc<Compiled>,
    /// When its files were last checked: milliseconds since `State::started`.
    checked: AtomicU64,
}

/// How often a production server looks at the files of a page it compiled.
/// A development server looks at every request: a page saved and reloaded at
/// once shows the change.
const RECHECK: Duration = Duration::from_secs(1);

impl State {
    /// Milliseconds since the server started (the clock of the caches).
    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// Whether `bytes` more of compressed static files fit in `gz_cache`.
    fn gz_room(&self, bytes: usize) -> bool {
        self.gz_cache.lock().unwrap().1 + bytes <= GZ_CACHE
    }

    /// Whether `bytes` more of static files fit in memory.
    fn static_room(&self, bytes: usize) -> bool {
        self.static_cache.lock().unwrap().1 + bytes <= STATIC_KEEP_TOTAL
    }

    /// Keeps a small static file in memory, if there is room (see `cached_static`).
    fn keep_static(&self, key: String, entry: StaticEntry) {
        let mut cache = self.static_cache.lock().unwrap();
        let old = cache.0.get(&key).map_or(0, |e| e.data.len());
        if cache.1 - old + entry.data.len() > STATIC_KEEP_TOTAL {
            return;
        }
        cache.1 = cache.1 - old + entry.data.len();
        cache.0.insert(key, Arc::new(entry));
    }

    fn cached(&self, script: &Path) -> Option<Arc<CachedPage>> {
        self.cache.lock().unwrap().get(script).cloned()
    }

    /// Whether a cached page was built from the files as they are. The files
    /// are looked at without holding the cache's lock.
    fn still_fresh(&self, page: &Arc<CachedPage>, script: &Path) -> bool {
        if !self.dev {
            let now = self.now_ms();
            let last = page.checked.load(Ordering::Relaxed);
            if now.saturating_sub(last) < RECHECK.as_millis() as u64 {
                return true;
            }
            // One request looks; the others use the page meanwhile.
            if page.checked.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_err() {
                return true;
            }
        }
        if !page.compiled.is_stale() {
            return true;
        }
        // Changed: the next requests wait for the page to be compiled again.
        let mut cache = self.cache.lock().unwrap();
        if cache.get(script).is_some_and(|p| Arc::ptr_eq(p, page)) {
            cache.remove(script);
        }
        false
    }

    /// Compiles a page, reusing the cached bytecode until a source file changes.
    /// Pages of a built site are compiled already.
    fn compiled(&self, script: &Path) -> Result<Arc<Compiled>, String> {
        if let Some(e) = &self.embedded {
            let rel = script.strip_prefix(&self.public).map_err(|_| "not a page of this site".to_string())?;
            let rel = rel.to_string_lossy().replace('\\', "/");
            return e.pages.get(&rel).cloned().ok_or_else(|| format!("no page {rel} in this site"));
        }
        if let Some(page) = self.cached(script) {
            if self.still_fresh(&page, script) {
                return Ok(page.compiled.clone());
            }
        }
        // One request compiles the page; the others wait and take its result.
        let gate = self.compiling.lock().unwrap().entry(script.to_path_buf()).or_default().clone();
        let _compiling = gate.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(page) = self.cached(script) {
            if self.still_fresh(&page, script) {
                return Ok(page.compiled.clone());
            }
        }
        let mut c = compile_cached(&self.root, script).map_err(|e| e.render(false))?;
        c.enable_jit();
        let c = Arc::new(c);
        if !c.warnings.is_empty() {
            eprint!("{}", c.warnings);
        }
        let checked = AtomicU64::new(self.now_ms());
        self.cache.lock().unwrap().insert(script.to_path_buf(), Arc::new(CachedPage { compiled: c.clone(), checked }));
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
    ///
    /// With the response comes the teardown of the script that made it (the
    /// page, or the middleware or error page that answered), if a script did:
    /// what remains of its VM, whose drop returns its database connections.
    /// The caller sends the response first and drops the teardown after, on
    /// this same thread (see `phv_driver::Teardown`).
    // The request's parts, and what kind of run it is (a page, an error page,
    // a WebSocket event with its connection).
    #[allow(clippy::too_many_arguments)]
    fn execute(
        &self,
        script: &Path,
        params: Vec<(String, String)>,
        ip: String,
        parts: hyper::http::request::Parts,
        body: Bytes,
        kind: Page,
        ws: Option<WsCtx>,
        stream: Option<Box<dyn FlushSink>>,
    ) -> (Resp, Option<Teardown>) {
        let headers: Vec<(String, String)> = parts
            .headers
            .iter()
            .map(|(k, v)| (k.as_str().to_ascii_lowercase(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
            .collect();
        let too_many = || (simple(StatusCode::BAD_REQUEST, "text/plain", "bad request: too many fields".into()), None);
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
                Err(e) => return (simple(StatusCode::BAD_REQUEST, "text/plain", format!("bad request: {e}")), None),
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
            let submitted = form
                .iter()
                .find(|(k, _)| k == "_csrf")
                .map(|(_, v)| v.as_str())
                .or_else(|| parts.headers.get("x-csrf-token").and_then(|v| v.to_str().ok()));
            // Tokens are masked with a pad of their own on each page (BREACH).
            // While the secret is replaced, tokens made with the old one still pass.
            let ok = match (&csrf_cookie, submitted) {
                (Some(c), Some(s)) => std::iter::once(&self.csrf_secret)
                    .chain(self.csrf_old.as_ref())
                    .any(|key| phv_std::csrf_matches(&phv_std::csrf_expected(key, c), s)),
                _ => false,
            };
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
                return (page(StatusCode::FORBIDDEN, "403 Forbidden", &body), None);
            }
        }

        let had_session = session.id.is_some();
        let mut ctx = HttpCtx {
            ws,
            can_publish: true,
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
            request_id: parts.extensions.get::<RequestId>().filter(|r| r.shown).map(|r| r.id.clone()),
            // With [telemetry], each call the script makes gets a span of its own (see `trace`).
            traceparent: parts
                .extensions
                .get::<RequestId>()
                .filter(|_| self.telemetry.is_none())
                .map(RequestId::traceparent),
            trace: self.telemetry.as_ref().and(parts.extensions.get::<RequestId>()).map(|r| phv_vm::TraceCtx {
                trace_id: r.trace.clone(),
                span_id: r.span.clone(),
                sampled: r.sampled(),
                spans: Default::default(),
            }),
            status: match kind {
                Page::Normal | Page::Event => 200,
                Page::NotFound => 404,
                Page::Error => 500,
            },
            stream,
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
        let failed = |title: &str, detail: &str| -> (Resp, Option<Teardown>) {
            if !self.dev && kind != Page::Error {
                if let Some(page) = self.special("_500.phv") {
                    if let Some(r) = self.run_page(&page, error_ctx(), false) {
                        return r;
                    }
                }
            }
            (self.error_page(title, detail), None)
        };
        // A page that failed after flush() sent its response: the response is
        // cut off (when its stream is dropped); this answer goes nowhere.
        let stopped = || (simple(StatusCode::INTERNAL_SERVER_ERROR, "text/plain", String::new()), None);

        // Middleware runs first, outermost first; one that answers (writes
        // output, sets a status, redirects or sends a file) ends the request.
        if kind == Page::Normal {
            for mw in self.middleware_for(script) {
                match self.run_one(&mw, ctx) {
                    Err((_, _, true)) => return stopped(),
                    Err((title, detail, _)) => return failed(&title, &detail),
                    Ok(mut outcome) => {
                        let answered = !outcome.output.is_empty()
                            || outcome.http.status != 200
                            || outcome.http.body_file.is_some()
                            || outcome.http.flushed;
                        if answered {
                            let teardown = outcome.teardown.take();
                            return (self.respond(outcome, had_session), teardown);
                        }
                        // The rest of its VM goes now, before the page runs: a
                        // connection it held goes back to its pool, where the
                        // page may need it (a pool of `pool_size` serves every
                        // worker; held through the page, the page would wait
                        // for its own thread).
                        ctx = outcome.http;
                    }
                }
            }
        }
        match self.run_one(script, ctx) {
            Ok(mut outcome) => {
                let teardown = outcome.teardown.take();
                (self.respond(outcome, had_session), teardown)
            }
            Err((_, _, true)) => stopped(),
            Err((title, detail, _)) => failed(&title, &detail),
        }
    }

    /// Runs one script for the request; a compile or runtime error gives its
    /// title and details, and whether the page had sent its response with
    /// flush() (it is cut off then: no error page can follow).
    fn run_one(&self, script: &Path, ctx: HttpCtx) -> Result<phv_driver::RunOutcome, (String, String, bool)> {
        let rel = script.strip_prefix(&self.root).unwrap_or(script).to_string_lossy().replace('\\', "/");
        let compiled = match self.compiled(script) {
            Ok(c) => c,
            Err(diagnostics) => {
                self.metrics.failure(metrics::Failure::Compile);
                eprintln!("compile error in {rel}:\n{diagnostics}");
                return Err(("Compile error".into(), format!("{rel} could not be compiled.\n\n{diagnostics}"), false));
            }
        };
        let limits = Limits {
            max_time: Some(self.time_limit),
            max_memory: Some(self.config.memory_limit_mb as usize * 1024 * 1024),
            ..Limits::default()
        };
        let host = Some(self.config.host.clone());
        let outcome = run(&compiled, RunOptions { html: true, limits, http: Some(ctx), host });
        self.export_spans(outcome.http.trace.as_ref());
        if let Some(err) = &outcome.error {
            self.metrics.failure(metrics::Failure::of(&err.message));
            let text = err.render();
            eprintln!("runtime error in {rel}:\n{}", err.render_for_log());
            return Err(("Runtime error".into(), text, outcome.http.flushed));
        }
        Ok(outcome)
    }

    /// Sends the spans a script made (its requests to other services).
    fn export_spans(&self, trace: Option<&phv_vm::TraceCtx>) {
        if let (Some(exporter), Some(t)) = (&self.telemetry, trace) {
            let spans = std::mem::take(&mut *t.spans.lock().unwrap_or_else(|e| e.into_inner()));
            exporter.send(spans);
        }
    }

    /// Runs an error page; `None` if it fails too (the built-in page is used
    /// then). With the response, the page's teardown (as `execute` gives it).
    fn run_page(&self, script: &Path, ctx: HttpCtx, had_session: bool) -> Option<(Resp, Option<Teardown>)> {
        let mut outcome = self.run_one(script, ctx).ok()?;
        let teardown = outcome.teardown.take();
        Some((self.respond(outcome, had_session), teardown))
    }

    /// The response for what a script did: status, headers, cookies and body.
    fn respond(&self, outcome: phv_driver::RunOutcome, had_session: bool) -> Resp {
        let mut http = outcome.http;
        // What the script asked of its WebSocket connection, and published.
        let ws = http.ws.take();
        if let Some(w) = &ws {
            self.hub.lock().unwrap_or_else(|e| e.into_inner()).apply(w);
        }
        self.deliver(&std::mem::take(&mut http.publishes));
        // A page that sent its response with flush(): the rest of its output ends it.
        if http.flushed {
            if let Some(mut sink) = http.stream.take() {
                sink.finish(outcome.output.into_bytes());
            }
            return simple(StatusCode::OK, "text/plain", String::new());
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
        let mut r = self.head(&mut http, had_session, body, false);
        if let Some(w) = ws {
            r.extensions_mut().insert(WsDone(w));
        }
        r
    }

    /// The response for a script's status, headers, cookies and session, with
    /// `body`. The session is saved now; with `keep`, the script goes on
    /// running (flush()) and keeps reading the session's values.
    fn head(&self, http: &mut HttpCtx, had_session: bool, body: RespBody, keep: bool) -> Resp {
        let csp = self.config.csp.page_header(http.csp_nonce.as_deref());
        let flags = if self.config.secure_cookies { "; Secure" } else { "" };
        let session = std::mem::take(&mut http.session);
        let touched_session = had_session || session.changed;
        if keep {
            http.session =
                SessionData { id: session.id.clone(), values: session.values.clone(), ..SessionData::default() };
        }
        if let Some(cookie) = self.sessions.save(session) {
            http.resp_headers.push(("Set-Cookie".into(), cookie));
        }
        if let Some(c) = http.new_csrf_cookie.take() {
            http.resp_headers.push((
                "Set-Cookie".into(),
                format!("{}={c}; Path=/; HttpOnly; SameSite=Lax{flags}", self.csrf_cookie),
            ));
            // Tokens made after flush() use the cookie just sent.
            http.csrf_cookie = Some(c);
        }
        let status = StatusCode::from_u16(http.status).unwrap_or(StatusCode::OK);
        let mut r = Response::new(body);
        *r.status_mut() = status;
        let ct = http.content_type.clone().unwrap_or_else(|| "text/html; charset=utf-8".into());
        let h = r.headers_mut();
        if let Ok(v) = HeaderValue::from_str(&ct) {
            h.insert(CONTENT_TYPE, v);
        }
        let mut sets_cookie = false;
        for (name, value) in std::mem::take(&mut http.resp_headers) {
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
        // A streamed page (flush()) may still make itself personal after its head
        // is sent (a CSRF token needs the visitor's cookie): its head is personal
        // when the request carries a cookie or credentials (found by the 1.2 audit).
        let credentials =
            keep && http.headers.iter().any(|(k, v)| (k == "cookie" || k == "authorization") && !v.trim().is_empty());
        let personal = touched_session || sets_cookie || http.personalized || credentials;
        // What the page does after flush() knows whether its head was personal.
        http.personalized = personal;
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
async fn read_body(
    state: &State,
    mut body: Incoming,
    small: usize,
) -> Result<(Bytes, Option<OwnedSemaphorePermit>), Box<Resp>> {
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
            let kib = room.saturating_sub(small).div_ceil(1024);
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
    // Over HTTP/2 a connection has several requests at once: none of their
    // bodies is free of the budget (found by the 1.0 audit).
    let small = if parts.version == hyper::Version::HTTP_2 { 0 } else { SMALL_BODY };
    let served = Served { kind: metrics::Kind::Page, route: Some(metrics::route_of(&state.public, &script)) };
    let (body, reserved) = match read_body(state, body, small).await {
        Ok(b) => b,
        Err(r) => {
            let why = if r.status() == StatusCode::PAYLOAD_TOO_LARGE {
                Some(metrics::Reject::TooLarge)
            } else if r.status() == StatusCode::SERVICE_UNAVAILABLE {
                Some(metrics::Reject::Body)
            } else {
                None
            };
            if let Some(why) = why {
                state.metrics.reject(why);
            }
            return *r;
        }
    };
    // Event streams count too: theirs is a quarter of the cap at most.
    if state.buffered.load(Ordering::Relaxed) + state.events_buffered.load(Ordering::Relaxed) > state.buffered_cap {
        state.metrics.reject(metrics::Reject::Output);
        return busy();
    }
    let waited = Instant::now();
    let Ok(Ok(permit)) = tokio::time::timeout(QUEUE_WAIT, state.workers.clone().acquire_owned()).await else {
        state.metrics.reject(metrics::Reject::Busy);
        return busy();
    };
    state.metrics.worker_wait(waited.elapsed());
    let st = state.clone();
    // A page that calls response()->flush() is answered when it first does;
    // it goes on running (and holding its worker) while it sends the rest.
    let (head_tx, mut head_rx) = tokio::sync::oneshot::channel();
    let sink: Option<Box<dyn FlushSink>> = (kind == Page::Normal).then(|| {
        Box::new(PageStream::new(state.clone(), head_tx, Instant::now() + state.time_limit)) as Box<dyn FlushSink>
    });
    // The response is handed over as soon as the page made it; the end of its
    // VM (its database connections rolling back and going back to their pools:
    // round trips) comes after, on the same thread, while the worker is still
    // held, so no more pages run at once than `workers` allows.
    let (done_tx, mut done_rx) = tokio::sync::oneshot::channel();
    let mut task = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let _reserved = reserved;
        let (resp, teardown) = st.execute(&script, params, ip, parts, body, kind, None, sink);
        // Not received: the request is gone (its connection closed).
        let _ = done_tx.send(resp);
        drop(teardown);
    });
    let done = tokio::select! {
        biased;
        Ok(mut head) = &mut head_rx => {
            head.extensions_mut().insert(served);
            return head;
        }
        Ok(resp) = &mut done_rx => {
            // The teardown goes on; a panic in it is still seen and logged.
            tokio::spawn(async move {
                if let Err(e) = task.await {
                    eprintln!("internal error: {e}");
                }
            });
            Ok(resp)
        }
        done = &mut task => match done {
            // The response was sent just before the task ended (or never: a bug).
            Ok(()) => done_rx.try_recv().map_err(|_| "the page gave no response".to_string()),
            Err(e) => Err(e.to_string()),
        },
    };
    // The head may have been sent just before the page ended.
    if let Ok(mut head) = head_rx.try_recv() {
        head.extensions_mut().insert(served);
        return head;
    }
    match done {
        Ok(mut r) => {
            r.extensions_mut().insert(served);
            r
        }
        Err(e) => {
            eprintln!("internal error: {e}");
            page(StatusCode::INTERNAL_SERVER_ERROR, "500 Internal Server Error", "<p>Something went wrong.</p>")
        }
    }
}

/// Where a page's output goes once it calls `response()->flush()`: the head
/// is handed to the request (as its response), then each chunk is sent
/// through a channel of one, so a page runs at most one chunk ahead of what
/// the client has read. A chunk waits for the client while it keeps reading
/// (never past the page's time limit); once it has taken nothing for
/// `FLUSH_STALL`, or is gone, flush() returns false and later output is dropped.
struct PageStream {
    state: Arc<State>,
    head: Option<tokio::sync::oneshot::Sender<Resp>>,
    body: Option<tokio::sync::mpsc::Sender<Bytes>>,
    aborted: Arc<std::sync::atomic::AtomicBool>,
    progress: Arc<body::Progress>,
    handle: tokio::runtime::Handle,
    deadline: Instant,
    /// The page ended normally: the body ends instead of failing.
    finished: bool,
}

impl PageStream {
    fn new(state: Arc<State>, head: tokio::sync::oneshot::Sender<Resp>, deadline: Instant) -> PageStream {
        PageStream {
            state,
            head: Some(head),
            body: None,
            aborted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            progress: body::Progress::new(),
            handle: tokio::runtime::Handle::current(),
            deadline,
            finished: false,
        }
    }

    /// Hands `chunk` to the connection; gives it back when it was not sent.
    fn send(&mut self, chunk: Vec<u8>) -> Result<(), Vec<u8>> {
        let Some(tx) = &self.body else { return Err(chunk) };
        if chunk.is_empty() {
            return if tx.is_closed() { Err(chunk) } else { Ok(()) };
        }
        let (progress, deadline, buffered) = (&self.progress, self.deadline, &self.state.buffered);
        let began = Instant::now();
        // A runtime that is shutting down (the server stopping) panics on timers: the client is gone then.
        let sent = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.handle.block_on(async {
                loop {
                    // The client may take its time while it keeps reading (found by
                    // the 1.2 audit: one that did not read held the page's worker
                    // until the page's time limit).
                    let now = Instant::now();
                    let idle = now.saturating_duration_since(progress.last().max(began));
                    if idle >= FLUSH_STALL || now >= deadline {
                        return Err(chunk);
                    }
                    let wait = (FLUSH_STALL - idle).min(deadline - now);
                    match tokio::time::timeout(wait, tx.reserve()).await {
                        Ok(Ok(permit)) => {
                            // Counted in the output total from now until it is written.
                            permit.send(body::counted_bytes(chunk, buffered));
                            return Ok(());
                        }
                        Ok(Err(_)) => return Err(chunk),
                        Err(_) => {}
                    }
                }
            })
        }));
        match sent {
            Ok(Ok(())) => Ok(()),
            // The client is gone, or has not read for too long: what is held is cut off.
            failed => {
                self.aborted.store(true, Ordering::Release);
                self.body = None;
                failed.unwrap_or_else(|_| Err(Vec::new()))
            }
        }
    }
}

impl FlushSink for PageStream {
    fn flush(&mut self, http: &mut HttpCtx, chunk: Vec<u8>) -> Result<(), Vec<u8>> {
        if let Some(head) = self.head.take() {
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            let had_session = http.session.id.is_some();
            let body = RespBody::chunks(rx, self.aborted.clone(), self.progress.clone());
            let resp = self.state.head(http, had_session, body, true);
            if head.send(resp).is_err() {
                return Err(chunk);
            }
            self.body = Some(tx);
        }
        self.send(chunk)
    }

    fn finish(&mut self, chunk: Vec<u8>) {
        if self.send(chunk).is_ok() || self.body.is_none() {
            self.finished = true;
        }
        self.body = None;
    }
}

impl Drop for PageStream {
    fn drop(&mut self) {
        if !self.finished {
            self.aborted.store(true, Ordering::Release);
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
            // (The teardown is dropped at once: the test waits for the response anyway.)
            None => match self.special("_404.phv") {
                Some(page) => self.execute(&page, Vec::new(), ip, parts, body, Page::NotFound, None, None).0,
                None => not_found(),
            },
            Some(Target::Script(p, params)) => self.execute(&p, params, ip, parts, body, Page::Normal, None, None).0,
            // Static files get the server's checks and headers (read whole: the
            // test reads the body at once).
            Some(Target::Static(p)) => match open_static(&self.public, &p, MAX_STATIC) {
                Opened::File { mut file, len, modified, data } => {
                    let data = match data {
                        Some(d) => d,
                        None => {
                            use std::io::Read;
                            let mut d = Vec::new();
                            if (&mut file).take(len).read_to_end(&mut d).is_err() {
                                return not_found();
                            }
                            d
                        }
                    };
                    let ct = content_type(&p).unwrap_or("application/octet-stream");
                    let v = Validators::of_file(len, modified);
                    bytes_response(self, &parts.headers, Bytes::from(data), ct, &v, &p.to_string_lossy())
                }
                Opened::TooLarge => {
                    simple(StatusCode::PAYLOAD_TOO_LARGE, "text/plain", "file too large to serve".into())
                }
                Opened::Missing => not_found(),
            },
            Some(Target::Embedded(bytes, ct, tag)) => {
                let v = Validators { etag: tag, modified: None };
                bytes_response(self, &parts.headers, bytes, ct, &v, parts.uri.path())
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
