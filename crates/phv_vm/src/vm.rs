//! The register-based bytecode interpreter.
//!
//! Each `Vm` owns one heap and is used for exactly one script run or HTTP
//! request; the compiled `Program` is shared and immutable.

use crate::bytecode::*;
use crate::jit::*;
pub use crate::jit::{Installed, JitCtx, JitEntry, JitLimits, JitTable};
use crate::value::*;
use indexmap::IndexMap;
use std::cell::{Cell, RefCell};
use std::fmt::Write as _;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub type BuiltinFn = fn(&mut Vm, &[Value]) -> Result<Value, VmError>;

pub struct TraceLine {
    pub func: String,
    pub file: String,
    pub line: usize,
}

pub struct Exc {
    pub value: Value,
    /// Where this exception was thrown (`Vm::thrown_at`), once it was. Only
    /// the identities of the calls are kept; the lines of the trace are made
    /// when they are read (`Vm::exception_trace_lines`).
    at: Option<ThrownAt>,
}

impl Exc {
    /// An exception carrying `value`, not thrown yet.
    pub fn new(value: Value) -> Exc {
        Exc { value, at: None }
    }
}

/// The calls on the stack where an exception was thrown: `(function, pc)` of
/// each call kept, innermost first, and how many calls there were. It does
/// not refer to the stack: the trace can be made after the calls returned.
/// The calls are shared between the exception and the record by object
/// (`Traces`), so a throw allocates them once.
#[derive(Clone)]
struct ThrownAt {
    calls: Rc<[(u32, u32)]>,
    depth: usize,
}

/// Where the exception objects of a run were first thrown, by object: the
/// text of a trace is made only when `getTraceAsString()` asks for it.
#[derive(Default)]
struct Traces {
    by_object: std::collections::HashMap<usize, (std::rc::Weak<RefCell<Object>>, ThrownAt), PtrHash>,
    /// Entries of objects that are gone are dropped when there are this many.
    prune_at: usize,
    /// By class: the slot of its `trace` field (`Exception`), or `u32::MAX`.
    /// Filled at the first throw, so a throw does not look the field up by name.
    trace_slot: Vec<u32>,
}

/// Hashes the address of an object (`Traces::by_object`): a multiply and a
/// fold, which spread an aligned pointer over the low bits the table uses.
#[derive(Default, Clone)]
struct PtrHash;

impl std::hash::BuildHasher for PtrHash {
    type Hasher = PtrHasher;
    fn build_hasher(&self) -> PtrHasher {
        PtrHasher(0)
    }
}

struct PtrHasher(u64);

impl std::hash::Hasher for PtrHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u64(b as u64);
        }
    }
    fn write_u64(&mut self, i: u64) {
        let h = (self.0 ^ i).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        self.0 = h ^ (h >> 29);
    }
    fn write_usize(&mut self, i: usize) {
        self.write_u64(i as u64);
    }
}

pub enum VmError {
    /// A PHV exception; can be caught with `try`/`catch`.
    Exception(Box<Exc>),
    /// An uncatchable error such as exceeding the time limit.
    Fatal(String),
}

pub struct OutBuf {
    pub buf: String,
    /// When true, plain strings are HTML-escaped as they are written.
    pub html: bool,
}

pub struct UploadedFile {
    pub field: String,
    /// The client's file name with any directory part removed.
    pub filename: String,
    pub content_type: String,
    pub data: Vec<u8>,
}

/// Session values for the current request. The server loads them before the
/// script runs and stores them afterwards.
#[derive(Default)]
pub struct SessionData {
    /// Id of an existing session (from the cookie), if any.
    pub id: Option<String>,
    pub values: SessionValues,
    pub changed: bool,
    /// `regenerate()` was called: issue a new id (prevents session fixation).
    pub regenerate: bool,
    pub destroyed: bool,
}

/// The values of a session, as a map of strings. The store hands them over
/// as it keeps them (the text of its record, or the map of the memory store,
/// shared) and they are decoded, or copied, at the first read or write, so a
/// page that never looks at its session does not pay for them. Reads and
/// writes go through `Deref` and `DerefMut`, so the map is used as before.
#[derive(Clone, Default)]
pub struct SessionValues {
    /// The values as the store keeps them, with the store's decoder; decoded
    /// into `map` at the first use. The decoder is given text the store has
    /// checked already, so it does not fail.
    encoded: Option<(String, SessionDecoder)>,
    map: std::cell::OnceCell<std::sync::Arc<SessionMap>>,
}

/// A session's values once decoded.
pub type SessionMap = indexmap::IndexMap<String, String>;

/// A store's decoder of the text it keeps a session's values as.
pub type SessionDecoder = fn(&str) -> SessionMap;

impl SessionValues {
    /// Values a store keeps as text, decoded by `decode` at the first use.
    pub fn encoded(text: String, decode: SessionDecoder) -> SessionValues {
        SessionValues { encoded: Some((text, decode)), map: std::cell::OnceCell::new() }
    }

    /// Values the memory store keeps: shared until written (then copied).
    pub fn shared(map: std::sync::Arc<SessionMap>) -> SessionValues {
        SessionValues { encoded: None, map: std::cell::OnceCell::from(map) }
    }

    /// Whether the values have been decoded (or copied) yet.
    pub fn is_decoded(&self) -> bool {
        self.map.get().is_some()
    }

    fn force(&self) -> &std::sync::Arc<SessionMap> {
        self.map.get_or_init(|| {
            std::sync::Arc::new(match &self.encoded {
                Some((text, decode)) => decode(text),
                None => SessionMap::new(),
            })
        })
    }

    /// The map, shared with the memory store's entry when nothing wrote to it.
    pub fn into_shared(self) -> std::sync::Arc<SessionMap> {
        self.force();
        self.map.into_inner().expect("initialized above")
    }

    pub fn into_map(self) -> SessionMap {
        std::sync::Arc::unwrap_or_clone(self.into_shared())
    }
}

impl From<SessionMap> for SessionValues {
    fn from(map: SessionMap) -> SessionValues {
        SessionValues::shared(std::sync::Arc::new(map))
    }
}

impl std::ops::Deref for SessionValues {
    type Target = SessionMap;

    fn deref(&self) -> &SessionMap {
        self.force()
    }
}

impl std::ops::DerefMut for SessionValues {
    fn deref_mut(&mut self) -> &mut SessionMap {
        self.force();
        std::sync::Arc::make_mut(self.map.get_mut().expect("initialized above"))
    }
}

impl<'a> IntoIterator for &'a SessionValues {
    type Item = (&'a String, &'a String);
    type IntoIter = indexmap::map::Iter<'a, String, String>;

    fn into_iter(self) -> Self::IntoIter {
        self.force().iter()
    }
}

/// What a stored session's values must be: a JSON object of strings.
/// Deserializing into it checks that shape without building anything, so a
/// store can check a record as it reads it and decode it later (or never).
pub struct SessionValuesShape;

impl<'de> serde::Deserialize<'de> for SessionValuesShape {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<SessionValuesShape, D::Error> {
        struct Shape;
        impl<'de> serde::de::Visitor<'de> for Shape {
            type Value = SessionValuesShape;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an object of strings")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<SessionValuesShape, A::Error> {
                while map.next_entry::<StrShape, StrShape>()?.is_some() {}
                Ok(SessionValuesShape)
            }
        }
        d.deserialize_map(Shape)
    }
}

/// A JSON string, not kept.
struct StrShape;

impl<'de> serde::Deserialize<'de> for StrShape {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<StrShape, D::Error> {
        struct Shape;
        impl serde::de::Visitor<'_> for Shape {
            type Value = StrShape;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a string")
            }

            fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<StrShape, E> {
                Ok(StrShape)
            }
        }
        d.deserialize_str(Shape)
    }
}

/// Request data and response state for scripts running under the web server.
pub struct HttpCtx {
    pub method: String,
    pub path: String,
    /// Address of the connected client (the proxy's, behind a reverse proxy).
    pub remote_ip: String,
    pub query: Vec<(String, String)>,
    pub form: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub files: Vec<UploadedFile>,
    pub session: SessionData,
    /// Records a consumed OAuth state in the session store before token exchange.
    pub oauth_states: Option<std::sync::Arc<dyn OAuthStates>>,
    /// Available image conversion slots, shared by the HTTP server's pages.
    pub image_slots: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
    /// Server secret for CSRF tokens (HMAC key).
    pub csrf_secret: Vec<u8>,
    /// The browser's CSRF cookie, if it sent a valid one.
    pub csrf_cookie: Option<String>,
    /// A CSRF cookie the server must set (new visitor, or rotated after login).
    pub new_csrf_cookie: Option<String>,
    /// Nonce of this response's Content-Security-Policy, added to `<script>` tags.
    pub csp_nonce: Option<String>,
    pub status: u16,
    pub resp_headers: Vec<(String, String)>,
    pub content_type: Option<String>,
    /// A file sent with `response()->file()`, already opened and checked; it
    /// replaces the written output and is streamed from disk.
    pub body_file: Option<std::sync::Arc<std::fs::File>>,
    /// Bytes made by the script that replace the written output, such as an
    /// image from `std.image`'s `send()` (not UTF-8 text, so not output).
    pub body_bytes: Option<Vec<u8>>,
    /// The response is for this visitor only (it carries a CSRF token), so it
    /// gets `Cache-Control: private, no-store`.
    pub personalized: bool,
    /// Parameters of the page's path (`public/posts/[id].phv`).
    pub params: Vec<(String, String)>,
    /// Values middleware passes on to the page (`locals()`).
    pub locals: Vec<(String, String)>,
    /// The WebSocket event this script handles (`public/*.ws.phv`), if any.
    pub ws: Option<WsCtx>,
    /// Messages for the WebSocket connections that joined a topic (`publish()`),
    /// delivered by the server after the script ends.
    pub publishes: Vec<(String, String)>,
    /// `publish()` can be used: the script runs in `phv serve`.
    pub can_publish: bool,
    /// The request's id (`[server] request_ids`), and the W3C `traceparent`
    /// this request's calls to other services send.
    pub request_id: Option<String>,
    pub traceparent: Option<String>,
    /// The job this script runs for (`[jobs]`), if it is one.
    pub job: Option<JobCtx>,
    /// Where `response()->flush()` sends the response while the page runs
    /// (`phv serve`); None elsewhere, where flush() keeps the output.
    pub stream: Option<Box<dyn FlushSink>>,
    /// flush() has sent the response's head: its status, headers, cookies and
    /// the session can no longer change.
    pub flushed: bool,
    /// The trace the script runs in, when traces are exported (`[telemetry]`).
    pub trace: Option<TraceCtx>,
    /// The locale of the request for `std.i18n` (one of `[i18n] locales`):
    /// chosen by `set_locale()`, or from `Accept-Language` when first needed.
    pub locale: Option<String>,
    /// The locale came from `Accept-Language`: the response varies by it
    /// (`phv serve` sends `Vary: Accept-Language`).
    pub locale_from_header: bool,
}

/// The trace of a request or job whose spans are exported (`[telemetry]`).
#[derive(Clone, Debug)]
pub struct TraceCtx {
    /// 32 lowercase hex digits.
    pub trace_id: String,
    /// The span of the request or job (16 hex digits): the parent of the
    /// spans its scripts make.
    pub span_id: String,
    /// Whether the trace is recorded (the `traceparent` flags say so to the services called).
    pub sampled: bool,
    /// Spans the scripts made (the requests they sent), for the server to export.
    pub spans: std::sync::Arc<std::sync::Mutex<Vec<Span>>>,
}

/// One finished span, in the terms of OpenTelemetry.
#[derive(Clone, Debug)]
pub struct Span {
    pub trace_id: String,
    pub span_id: String,
    pub parent_id: Option<String>,
    pub name: String,
    pub kind: SpanKind,
    pub start: std::time::SystemTime,
    pub end: std::time::SystemTime,
    pub attributes: Vec<(&'static str, SpanValue)>,
    pub error: bool,
}

impl Span {
    /// A request's method as spans record it: a standard method, or `_OTHER`
    /// (the OpenTelemetry convention). A request may name any method, up to
    /// the length of its head (found by the 1.2 audit: 63 KB names, one per
    /// span, filled the export queue with gigabytes).
    pub fn method(m: &str) -> &'static str {
        const KNOWN: [&str; 9] = ["GET", "HEAD", "POST", "PUT", "DELETE", "CONNECT", "OPTIONS", "TRACE", "PATCH"];
        KNOWN.into_iter().find(|k| *k == m).unwrap_or("_OTHER")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpanKind {
    Internal,
    Server,
    Client,
    Consumer,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SpanValue {
    Str(String),
    Int(i64),
}

/// Sends a page's response while it runs (`response()->flush()`).
pub trait FlushSink: Send {
    /// Sends the response's head the first time (from `http`), then `chunk`
    /// of the body. `Ok` when the chunk was handed on: its bytes leave this
    /// thread (they are freed elsewhere, so the script's memory count lets
    /// them go). `Err` gives the chunk back once the client has gone away (or
    /// has stopped reading): it is freed on this thread, where it was counted.
    fn flush(&mut self, http: &mut HttpCtx, chunk: Vec<u8>) -> Result<(), Vec<u8>>;
    /// The page ended: `chunk` is the rest of its output. Without this call
    /// (the page failed), the response is cut off.
    fn finish(&mut self, chunk: Vec<u8>);
}

/// What happened on a WebSocket connection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WsEvent {
    /// The connection is being opened: the script may refuse it.
    #[default]
    Open,
    Message,
    Close,
}

/// One event of a WebSocket connection, and what the script asked for.
#[derive(Clone, Debug, Default)]
pub struct WsCtx {
    pub event: WsEvent,
    /// The connection's id (random, the same for all its events).
    pub id: String,
    /// The text message received (for `Message`).
    pub message: String,
    /// Values kept for the connection from one event to the next.
    pub state: indexmap::IndexMap<String, String>,
    /// Messages to send to this connection.
    pub sends: Vec<String>,
    pub joins: Vec<String>,
    pub leaves: Vec<String>,
    /// Close the connection with this code and reason.
    pub close: Option<(u16, String)>,
}

impl Default for HttpCtx {
    fn default() -> Self {
        HttpCtx {
            method: "GET".into(),
            path: "/".into(),
            remote_ip: String::new(),
            query: Vec::new(),
            form: Vec::new(),
            headers: Vec::new(),
            body: String::new(),
            files: Vec::new(),
            session: SessionData::default(),
            oauth_states: None,
            image_slots: None,
            csrf_secret: Vec::new(),
            csrf_cookie: None,
            new_csrf_cookie: None,
            csp_nonce: None,
            status: 200,
            resp_headers: Vec::new(),
            content_type: None,
            body_file: None,
            body_bytes: None,
            personalized: false,
            params: Vec::new(),
            locals: Vec::new(),
            ws: None,
            publishes: Vec::new(),
            can_publish: false,
            request_id: None,
            traceparent: None,
            job: None,
            stream: None,
            flushed: false,
            trace: None,
            locale: None,
            locale_from_header: false,
        }
    }
}

#[derive(Clone, Copy)]
pub struct Limits {
    pub max_time: Option<Duration>,
    pub max_frames: usize,
    /// Bytes the script may allocate (requires the counting allocator).
    pub max_memory: Option<usize>,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { max_time: None, max_frames: 10_000, max_memory: None }
    }
}

/// What a script may access outside its own heap (from `phv.toml`).
#[derive(Clone, Default, Debug)]
pub struct Permissions {
    /// Directories (relative to the project root) that may be read.
    pub read: Vec<String>,
    /// Directories that may be written (also readable).
    pub write: Vec<String>,
    /// Environment variable names; a trailing `*` matches a prefix.
    pub env: Vec<String>,
    /// Variables no script may read whatever `env` says: the server's secret,
    /// the admin token, and the URLs and headers of `[databases]`, `[http]`,
    /// `[websockets]` and `[mail]` (found by the 1.1 audit: `env = ["APP_*"]`
    /// gave scripts `APP_SECRET`).
    pub hidden_env: Vec<String>,
}

pub trait OAuthStates: Send + Sync {
    fn claim(&self, session: &str, digest: &str, now: i64, expires: i64) -> bool;
}

#[derive(Clone, Default, Debug)]
pub struct Host {
    pub root: std::path::PathBuf,
    pub permissions: Permissions,
    /// Database servers scripts may connect to, by name (`[databases]` in phv.toml).
    pub databases: std::collections::BTreeMap<String, DatabaseSpec>,
    /// WebSocket servers scripts may connect to, by name (`[websockets]` in phv.toml).
    pub websockets: std::collections::BTreeMap<String, WebSocketSpec>,
    /// HTTP services scripts may send requests to, by name (`[http]` in phv.toml).
    pub http: std::collections::BTreeMap<String, HttpSpec>,
    /// The Redis database (a name in `databases`) that rate limits are counted
    /// in, so every server process shares them; None counts in this process.
    pub rate_limit_store: Option<String>,
    /// The site `phv test` sends requests to (`std.testing`), when the project has one.
    pub site: Option<std::sync::Arc<dyn Site>>,
    /// Mail servers scripts may send through, by name (`[mail]` in phv.toml).
    pub mail: std::collections::BTreeMap<String, MailSpec>,
    /// A script run with `phv run`: it may read its arguments and standard input.
    pub cli: bool,
    /// The arguments after the script's name (`phv run script.phv a b`).
    pub args: Vec<String>,
    /// Standard input is not the script's (under `phv dap` it carries the protocol).
    pub no_stdin: bool,
    /// `[jobs]` of phv.toml: where `std.jobs` adds jobs.
    pub jobs: Option<JobsSpec>,
    /// From `[security] store_namespace` or `[project] name`: what this site
    /// keeps in a shared store (sessions, rate limits, jobs) is apart from
    /// other sites using the same database (found by the 1.1 audit: a session
    /// of one site was valid on another).
    pub store_namespace: Option<String>,
    /// Sign-in providers, by name (`[oauth]` in phv.toml).
    pub oauth: std::collections::BTreeMap<String, OAuthSpec>,
    /// The address `phv serve` listens on: scripts it runs cannot connect to
    /// it (a page waiting for its own server holds a worker). None elsewhere.
    pub listen: Option<std::net::SocketAddr>,
}

/// `[jobs]` of phv.toml, as `std.jobs` uses it.
#[derive(Clone, Debug, Default)]
pub struct JobsSpec {
    /// The database of `databases` jobs are kept in; None keeps them in this
    /// process's memory.
    pub store: Option<String>,
    /// The project's jobs (the scripts of `[jobs] dir`), when they are known.
    pub names: Option<Vec<String>>,
    /// Names the in-memory queue of the project (its root).
    pub memory_key: String,
}

/// The job a script runs for (`std.jobs`: `job()`).
#[derive(Clone, Debug, Default)]
pub struct JobCtx {
    pub name: String,
    pub id: String,
    pub payload: String,
    /// 1 for the first run, 2 for the first retry, ...
    pub attempt: u32,
}

/// A mail server named in phv.toml (`[mail.<name>]`). The URL holds credentials
/// and is never shown.
#[derive(Clone)]
pub struct MailSpec {
    /// `smtp://` or `smtps://` URL, or None when the environment variable is not set.
    pub url: Option<String>,
    pub url_env: Option<String>,
    /// Some(false) allows a server on another machine without TLS.
    pub tls: Option<bool>,
    /// The sender: `Name <address>` or `address`.
    pub from: String,
}

impl std::fmt::Debug for MailSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MailSpec").field("url_env", &self.url_env).field("from", &self.from).finish_non_exhaustive()
    }
}

/// A request a test sends to the site (`TestClient` in `std.testing`).
pub struct SiteRequest {
    pub method: String,
    /// The path with its query string.
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

pub struct SiteResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Answers requests the way the web server does, without a network.
pub trait Site: Send + Sync + std::fmt::Debug {
    fn request(&self, req: SiteRequest) -> Result<SiteResponse, String>;
}

/// A database server named in phv.toml. Scripts only ever see the name: the
/// URL holds credentials and is never shown (not even in Debug output).
#[derive(Clone)]
pub struct DatabaseSpec {
    /// `postgres://`, `mysql://`, `redis://` or `rediss://` URL, or None when
    /// the environment variable named by `url_env` is not set.
    pub url: Option<String>,
    /// The environment variable the URL comes from, for error messages.
    pub url_env: Option<String>,
    /// Some(false) allows plain connections to remote hosts; None means TLS
    /// for every host except localhost.
    pub tls: Option<bool>,
    /// Connections kept per server process.
    pub pool_size: usize,
}

impl std::fmt::Debug for DatabaseSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DatabaseSpec")
            .field("url", &self.url.as_ref().map(|_| "<redacted>"))
            .field("url_env", &self.url_env)
            .field("tls", &self.tls)
            .field("pool_size", &self.pool_size)
            .finish()
    }
}

/// A sign-in provider named in phv.toml (`[oauth.<name>]`, std.oauth). The
/// client secret is never shown.
#[derive(Clone, Default)]
pub struct OAuthSpec {
    /// OpenID Connect: the issuer, whose discovery document names the endpoints.
    pub issuer: Option<String>,
    /// OAuth 2.0 without OpenID Connect: the endpoints themselves.
    pub authorize_url: Option<String>,
    pub token_url: Option<String>,
    /// None when `client_id_env` names a variable that is not set.
    pub client_id: Option<String>,
    pub client_id_env: Option<String>,
    /// None for a public client (PKCE only), or when the variable is not set.
    pub client_secret: Option<String>,
    pub client_secret_env: Option<String>,
    /// The site's callback URL, as registered with the provider.
    pub redirect_uri: Option<String>,
    pub redirect_uri_env: Option<String>,
    pub scopes: Vec<String>,
    /// How the secret is sent to the token endpoint: Some(true) in the body
    /// (`client_secret_post`), Some(false) with HTTP Basic, None as the
    /// provider's discovery document allows (Basic when it says nothing).
    pub secret_in_body: Option<bool>,
    /// Some(false) allows `http://` endpoints on another machine.
    pub tls: Option<bool>,
}

impl std::fmt::Debug for OAuthSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthSpec")
            .field("issuer", &self.issuer)
            .field("authorize_url", &self.authorize_url)
            .field("client_id", &self.client_id)
            .field("client_secret_env", &self.client_secret_env)
            .field("redirect_uri", &self.redirect_uri)
            .finish_non_exhaustive()
    }
}

/// A WebSocket server named in phv.toml. Like a database URL, the URL may hold
/// a password (in its path or query), so it is never shown.
#[derive(Clone)]
pub struct WebSocketSpec {
    /// `ws://` or `wss://` URL, or None when the environment variable named by
    /// `url_env` is not set.
    pub url: Option<String>,
    /// The environment variable the URL comes from, for error messages.
    pub url_env: Option<String>,
    /// Some(false) allows `ws://` to remote hosts; None means TLS for every
    /// host except localhost.
    pub tls: Option<bool>,
}

impl std::fmt::Debug for WebSocketSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSocketSpec")
            .field("url", &self.url.as_ref().map(|_| "<redacted>"))
            .field("url_env", &self.url_env)
            .field("tls", &self.tls)
            .finish()
    }
}

/// An HTTP service named in phv.toml. Scripts give only the name and a path;
/// the base URL and the headers from environment variables (API keys) stay
/// in the configuration and are never shown.
#[derive(Clone)]
pub struct HttpSpec {
    /// `http://` or `https://` base URL, or None when the environment variable
    /// named by `url_env` is not set.
    pub url: Option<String>,
    /// The environment variable the URL comes from, for error messages.
    pub url_env: Option<String>,
    /// Some(false) allows `http://` to remote hosts; None means TLS for every
    /// host except localhost.
    pub tls: Option<bool>,
    /// Headers sent with every request: name, and the value (None when it
    /// comes from an environment variable that is not set).
    pub headers: Vec<(String, Option<String>)>,
    /// For each header whose value comes from an environment variable: the
    /// header name and the variable (scripts cannot replace these headers).
    pub headers_env: Vec<(String, String)>,
}

impl std::fmt::Debug for HttpSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.headers.iter().map(|(n, _)| n.as_str()).collect();
        f.debug_struct("HttpSpec")
            .field("url", &self.url.as_ref().map(|_| "<redacted>"))
            .field("url_env", &self.url_env)
            .field("tls", &self.tls)
            .field("headers", &names)
            .finish()
    }
}

const MAX_NATIVE_DEPTH: u32 = 200;

/// A call from Rust (a callback of a builtin) is made only with this much of
/// the thread's stack left: room for the interpreter and the builtins it
/// calls before the next such check. A level of callbacks takes about 3 KB
/// (a release build), so 200 of them fit in any thread that runs scripts.
const CALL_STACK: usize = 512 << 10;

/// Exception traces keep this many innermost and outermost calls.
const TRACE_ENDS: usize = 50;

/// `==` checks the time limit every this many bytes compared.
const EQUAL_STEP: usize = 16 << 20;

pub(crate) const BUDGET: u32 = 10_000;

/// Gives the record of objects room for as many again as it holds (at least
/// 1024), so that it fills, and is looked at, only after that many more.
fn room_for_twice<T>(objects: &mut Vec<T>) {
    let want = (objects.len() * 2).max(1024);
    if objects.capacity() > want * 2 {
        objects.shrink_to(want);
    }
    if objects.capacity() < want {
        objects.reserve_exact(want - objects.len());
    }
}

/// The number of live objects at which the cycle collector first runs.
const GC_FIRST: usize = 4096;

/// The cycle collector looks at no more than this many values per object
/// made since its last run (see `Vm::collect_cycles`).
const GC_WORK: usize = 16;
/// Called before each statement of a program compiled with statement markers
/// (`phv dap`): it may look at the VM, wait for the debugger's commands, or
/// stop the program by returning an error.
pub trait DebugHook {
    fn statement(&mut self, vm: &Vm, file: u32, offset: u32) -> Result<(), VmError>;
}

/// A frame of the call stack, for the debugger: the function, the instruction
/// after the call it is in (for frames below the top), and its registers.
#[derive(Clone, Copy, Debug)]
pub struct DebugFrame {
    pub func: u32,
    pub pc: u32,
    pub base: usize,
}

/// Ticks between the samples of `phv run --profile`.
const PROFILE_BUDGET: u32 = 1_000;

/// `phv run --profile`: calls of each function, and the time spent in each
/// (`self_time`: in its own code and the builtins it called; `total_time`:
/// also in the functions it called). Calls are counted exactly; time is
/// charged to the call stack every few thousand steps, so short functions
/// may show no time.
#[derive(Clone, Debug, Default)]
pub struct Profile {
    pub calls: Vec<u64>,
    pub self_time: Vec<Duration>,
    pub total_time: Vec<Duration>,
    last: Option<Instant>,
    /// Per function, the sample in which its total time was last charged
    /// (a recursive function is charged once per sample).
    seen: Vec<u64>,
    samples: u64,
}

impl Profile {
    pub fn new(funcs: usize) -> Profile {
        Profile {
            calls: vec![0; funcs],
            self_time: vec![Duration::ZERO; funcs],
            total_time: vec![Duration::ZERO; funcs],
            last: None,
            seen: vec![0; funcs],
            samples: 0,
        }
    }

    /// Charges the time since the last sample to the functions on `stack` (innermost last).
    fn sample(&mut self, stack: impl DoubleEndedIterator<Item = u32>) {
        let now = Instant::now();
        let Some(last) = self.last.replace(now) else { return };
        let elapsed = now - last;
        self.samples += 1;
        let mut stack = stack.rev();
        if let Some(top) = stack.next() {
            self.self_time[top as usize] += elapsed;
            self.total_time[top as usize] += elapsed;
            self.seen[top as usize] = self.samples;
        }
        for f in stack {
            if self.seen[f as usize] != self.samples {
                self.seen[f as usize] = self.samples;
                self.total_time[f as usize] += elapsed;
            }
        }
    }
}

struct Frame {
    func: u32,
    pc: u32,
    base: usize,
    ret: Reg,
    /// Set on frames entered from Rust (`call_function`): returning from it
    /// leaves the interpreter loop.
    stop: bool,
    /// The frame of a generator a loop resumed (see `GenLink`).
    gen: bool,
}

/// A generator is an object of the prelude's class `Generator`, whose fields
/// hold its state and, while it waits, its frame: the cycle collector sees
/// the values the frame holds, the memory they take counts against the
/// script's limit, and they are freed with the generator.
const G_FUNC: usize = 0;
/// Where it resumes.
const G_PC: usize = 1;
/// The `Yield` it waits at (-1 before it started): a generator closed there
/// goes on from the next instruction.
const G_AT: usize = 2;
/// `G_NEW`, `G_WAITING`, `G_RUNNING` or `G_DONE`.
const G_STATE: usize = 3;
/// The next int key of a `yield` without one.
const G_KEY: usize = 4;
/// The exception handlers of its `try` blocks while it waits: their catch
/// targets and exception registers, as a list of ints (or null).
const G_HANDLERS: usize = 5;
/// The registers of its frame while it waits.
const G_REGS: usize = 6;
/// Not iterated yet.
const G_NEW: i64 = 0;
/// Claimed by a loop, and waiting for it to ask for the next value.
const G_WAITING: i64 = 1;
const G_RUNNING: i64 = 2;
const G_DONE: i64 = 3;

/// A generator running on top of the loop that resumed it (the frame below):
/// where its next key and value go, and where the loop goes on when it ends.
struct GenLink {
    /// The generator's frame.
    frame: usize,
    gen: Rc<RefCell<Object>>,
    key: Reg,
    val: Reg,
    /// The loop's exit (`GenNext`), or `GEN_CLOSING` (`GenClose`: the loop
    /// goes on after that instruction).
    end: u32,
}

const GEN_CLOSING: u32 = u32::MAX;

/// A closure a builtin calls once per element, from `Vm::callback`. The calls
/// of one builtin share what `callback` checked; the closure is still a frame
/// of the call stack during each call, so an exception it throws has the
/// same trace, and an error the same position, as through `call_value`.
pub struct Callback {
    clo: Rc<Closure>,
    /// Checked by `Vm::callback`: `call` may skip to `call_fast`.
    fast: bool,
}

impl Callback {
    /// Calls the closure with `args`.
    #[inline]
    pub fn call(&self, vm: &mut Vm, args: &[Value]) -> Result<Value, VmError> {
        if self.fast {
            vm.call_fast(self.clo.func, &self.clo.caps, args)
        } else {
            vm.call_inner(self.clo.func, &self.clo.caps, args)
        }
    }
}

struct Handler {
    frame: usize,
    catch_pc: u32,
    exc_reg: Reg,
    out_depth: usize,
    /// The length of that output when the `try` began. An exception thrown
    /// inside a `<script>`, tag or attribute (a guard) may leave a string or
    /// attribute open, so what the `try` wrote is dropped and the `catch`
    /// writes where the `try` began.
    out_len: usize,
    /// Whether the `try` itself began inside such HTML (a guard of its frame).
    in_guard: bool,
    /// `guards` when the `try` began.
    guards: usize,
    /// `Vm::flushes` when the `try` began.
    flushes: u64,
}

/// Why output was refused (see `Vm::guards`).
const GUARDED_OUTPUT: &str = "a function wrote output inside a `<script>`, an attribute, a URL, CSS, a comment, \
    `<svg>`/`<math>`, a `<select>` or the text of an element such as `<textarea>`, where HTML escaping does not protect it (or \
    browsers read its markup in another way); return the value (or `Html`) from the function and insert it with `<?= ?>`";

/// Why a caught exception stopped the page (see `Vm::output_flushed`).
const FLUSHED_GUARD: &str = "an exception was caught after response()->flush() sent part of a tag, an attribute, \
    a `<script>` or similar HTML that the exception left unfinished, so the `catch` cannot write in its place; \
    call flush() outside such HTML, or catch the exception where it is thrown";

pub struct Vm {
    pub prog: Arc<Program>,
    /// `prog` again, for `dispatch`: a callback from a builtin (`sortBy`,
    /// `map` ...) enters it once per call, and counting this reference is
    /// cheaper than an atomic count of the `Arc`.
    #[allow(clippy::redundant_allocation)]
    code: Rc<Arc<Program>>,
    builtins: Arc<[BuiltinFn]>,
    regs: Vec<Value>,
    frames: Vec<Frame>,
    handlers: Vec<Handler>,
    /// Statements running in HTML other than element text, as (frame, output
    /// depth, `flushes` when it began): functions they call may not write to
    /// that output, where text escaping would not protect what they write.
    guards: Vec<(usize, usize, u64)>,
    /// How many times `response()->flush()` has sent the output so far.
    flushes: u64,
    consts: Vec<Option<Value>>,
    pub out: Vec<OutBuf>,
    pub http: HttpCtx,
    /// The response is JavaScript (`text/javascript`): its output is not
    /// escaped, so a plain string written to it would be code.
    pub js_output: bool,
    pub host: Arc<Host>,
    /// Native handles owned by this run (e.g. database connections).
    pub resources: Vec<Option<Box<dyn std::any::Any>>>,
    pub jit: Option<Arc<JitTable>>,
    jit_rt: JitRt,
    /// `JitCtx::stack_limit` of the outermost native call running, or 0.
    jit_stack_limit: usize,
    /// A native call that ran out of stack is being run by the interpreter:
    /// while more than this many frames are on the stack, calls are not
    /// made natively.
    jit_paused: Option<usize>,
    limits: Limits,
    mem_base: isize,
    deadline: Option<Instant>,
    pub(crate) budget: u32,
    /// Bytes echoed since the limits were last checked for output.
    echoed: usize,
    /// Bytes compared by `equal` since the limits were last checked: kept
    /// across calls, so a builtin comparing many values is timed as a whole
    /// (found by the 1.1 audit: `contains()` ran 18 s past a 1 s limit).
    equal_since: Cell<usize>,
    pub rng: u64,
    native_depth: u32,
    /// Per function, once looked at: 1 when its body has no `try`, 2 when it
    /// has one (see `callback`); empty until the first callback.
    plain_bodies: Vec<u8>,
    /// Tests only: every `Callback` takes the full path of `call_value`.
    slow_callbacks: bool,
    argbuf: Vec<Value>,
    /// The keys of a path assignment (`SetPath`, `AppendPath`), kept for reuse.
    keybuf: Vec<Value>,
    /// The arguments of a native call that may have to run again in the
    /// interpreter (see `call_jit`), kept for reuse.
    jit_rerun: Vec<Value>,
    /// Enum cases are singletons within one VM (one request): per class, per case.
    enum_cache: Vec<Vec<Option<Value>>>,
    /// Per class: the default values of its fields, made once and copied
    /// into each new object.
    field_defaults: RefCell<Vec<Option<Vec<Value>>>>,
    /// `phv dap`: called before each statement (see `DebugHook`).
    pub debug: Option<Box<dyn DebugHook>>,
    /// `phv run --profile`: calls and time per function (the JIT is not used then).
    pub profile: Option<Profile>,
    /// `phv test --coverage`: the statements that ran, as (file, source offset).
    pub coverage: Option<std::collections::HashSet<(u32, u32)>>,
    /// Objects created by this run. Reference counting cannot free cycles
    /// (`$node->next = $node`): the cycle collector (`cycles.rs`) looks for
    /// them while the script runs, and the fields of all objects still alive
    /// are cleared when the run ends.
    objects: RefCell<Vec<std::rc::Weak<RefCell<Object>>>>,
    /// Where each exception object was first thrown (see `Traces`).
    traces: RefCell<Traces>,
    /// The number of live objects at which the cycle collector runs next.
    gc_next: std::cell::Cell<usize>,
    /// Set when that number is reached; the collector runs at the next check
    /// of the time limit, where no object is in use by an instruction.
    gc_due: std::cell::Cell<bool>,
    /// The length of `objects` when the cycle collector last ran (less the
    /// objects found gone since): the objects made since are those after it.
    gc_base: Cell<usize>,
    /// The values the cycle collector looked at when it last ran.
    gc_visited: Cell<usize>,
    /// Memory in use (bytes) above which a new object also makes the
    /// collector run (three quarters of the limit): cycles of a few objects
    /// may hold much memory (found by the 1.0.0 audit: the collector ran on
    /// their number only).
    gc_pressure: isize,
    /// The generators running, innermost last (see `GenLink`).
    gen_links: Vec<GenLink>,
    /// The class of generators (`Generator` in the prelude), or `u32::MAX`.
    gen_class: u32,
}

impl Drop for Vm {
    fn drop(&mut self) {
        for w in std::mem::take(self.objects.get_mut()) {
            if let Some(o) = w.upgrade() {
                let fields = std::mem::take(&mut o.borrow_mut().fields);
                drop(o);
                drop(fields);
            }
        }
    }
}

#[cold]
#[inline(never)]
fn bad(expected: &str, v: &Value) -> ! {
    panic!("phv vm: expected {expected}, found {} (compiler bug)", v.type_name())
}

/// The memory limit of a run, as the interpreter and native code check it.
#[derive(Clone, Copy)]
pub(crate) struct MemLimit {
    pub max: Option<usize>,
    pub base: isize,
}

impl MemLimit {
    fn used(&self) -> isize {
        crate::alloc::thread_allocated() - self.base
    }

    /// Fails before an allocation of `bytes` that would exceed the limit (or
    /// 4 GB without one); the error holds that limit.
    pub fn reserve(&self, bytes: usize) -> Result<(), usize> {
        let limit = self.max.unwrap_or(4 << 30);
        let used = self.used().max(0) as usize;
        if bytes > limit || used.saturating_add(bytes) > limit {
            return Err(limit);
        }
        Ok(())
    }

    /// Fails when more than the limit has been allocated; the error holds the limit.
    pub fn check(&self) -> Result<(), usize> {
        match self.max {
            Some(max) if self.used() > max as isize => Err(max),
            _ => Ok(()),
        }
    }
}

fn memory_error(limit: usize) -> VmError {
    VmError::Fatal(format!("allowed memory size of {} MB exhausted", limit / (1024 * 1024)))
}

// String instructions, shared by the interpreter and the JIT's helpers.
// `regs` is the function's register window.

/// `ToStr`: the string form of a scalar.
pub(crate) fn op_to_str(regs: &mut [Value], d: Reg, a: Reg) {
    let v = match &regs[a as usize] {
        Value::Str(s) => Value::Str(s.clone()),
        other => Value::str(other.to_display_string()),
    };
    regs[d as usize] = v;
}

/// `Concat`: joins `n` registers from `start`. The error is the memory limit.
pub(crate) fn op_concat(
    regs: &mut [Value],
    mem: MemLimit,
    d: Reg,
    start: Reg,
    n: u16,
    html: bool,
) -> Result<(), usize> {
    let parts = &regs[start as usize..start as usize + n as usize];
    let mut size = 0usize;
    for p in parts {
        size += match p {
            Value::Str(s) | Value::Html(s) => s.len(),
            _ => 24,
        };
    }
    // Escaping can make a string up to 6 times longer (`"` -> `&quot;`).
    let need = if html { size.saturating_mul(6) } else { size };
    if need > 1 << 12 {
        mem.reserve(need)?;
    }
    let mut s = String::with_capacity(size);
    // The operands are temporaries the compiler made for this instruction:
    // the strings are moved out, so that they do not stay shared.
    for p in &mut regs[start as usize..start as usize + n as usize] {
        match &*p {
            // An `Html` result never contains unescaped plain strings.
            Value::Str(x) if html => html_escape_into(x, &mut s),
            v => v.append_to(&mut s),
        }
        if matches!(p, Value::Str(_) | Value::Html(_)) {
            *p = Value::Null;
        }
    }
    let big = s.len() > 1 << 20;
    regs[d as usize] = if html { Value::html(s) } else { Value::str(s) };
    if big {
        mem.check()?;
    }
    Ok(())
}

/// `AppendStr`: `$d .= $s`, in place when the string is not shared. The
/// error is the memory limit.
/// A string operation over this many bytes counts as long (see `Vm::long_op`).
pub(crate) const LONG_OP: usize = 64 << 10;

/// The length of a string value (0 for other values).
pub(crate) fn str_len(v: &Value) -> usize {
    match v {
        Value::Str(s) | Value::Html(s) => s.len(),
        _ => 0,
    }
}

/// Returns the bytes it went over (appended, and copied when the string was shared).
/// Always inlined: `$s .= ...` in a loop is a hot path, and whole-program
/// optimization otherwise decides afresh with every change elsewhere.
#[inline(always)]
pub(crate) fn op_append(regs: &mut [Value], mem: MemLimit, d: Reg, s: Reg) -> Result<usize, usize> {
    let src = regs[s as usize].clone();
    append_value(&mut regs[d as usize], &src, mem)
}

/// `$target .= $src` on a string, in place when it is not shared (see `op_append`).
#[inline(always)]
fn append_value(target: &mut Value, src: &Value, mem: MemLimit) -> Result<usize, usize> {
    let grow = match src {
        Value::Str(x) | Value::Html(x) => x.len(),
        _ => 24,
    };
    let (cur, shared) = match &*target {
        Value::Str(x) => (x.capacity(), Rc::strong_count(x) > 1),
        _ => (0, false),
    };
    // A string also held elsewhere is copied first (found by the 1.0 audit:
    // `$t = $s; $s .= "x";` copied 20 MB untimed and unaccounted).
    let copied = if shared { str_len(target) } else { 0 };
    if copied > 0 {
        mem.reserve(copied.saturating_add(grow))?;
    }
    if grow > 1 << 20 || (cur > 1 << 20 && cur < grow + cur) {
        // The string may double its buffer.
        mem.reserve(grow.max(cur) * 2)?;
    }
    let len = match target {
        Value::Str(rc) => {
            let s = Rc::make_mut(rc);
            src.append_to(s);
            s.len()
        }
        v => bad("string", v),
    };
    if len > 1 << 20 && len.is_power_of_two() {
        mem.check()?;
    }
    Ok(grow + copied)
}

/// Why a path assignment failed: the memory limit (holding the limit), or an
/// exception (`IndexError`, `KeyError`) with its class and message.
pub(crate) enum PathError {
    Memory(usize),
    Throw(&'static str, String),
}

impl From<(&'static str, String)> for PathError {
    fn from((class, msg): (&'static str, String)) -> PathError {
        PathError::Throw(class, msg)
    }
}

/// `cur . src`, as `Concat` makes it.
fn concat_pair(cur: Value, src: &Value, mem: MemLimit) -> Result<Value, usize> {
    let mut pair = [cur, src.clone()];
    op_concat(&mut pair, mem, 0, 0, 2, false)?;
    Ok(std::mem::take(&mut pair[0]))
}

/// `AppendPath` on `target` (a variable or a property) with the values of its
/// keys: `src` is appended to the string at the end of the path in place,
/// when it is still `cur` (the value read before `src` was evaluated);
/// otherwise the element becomes `cur . src`, as the assignment of that
/// would make it. Returns the bytes gone over (see `Vm::long_op`).
pub(crate) fn path_append(
    target: &mut Value,
    keys: &[Value],
    create: &[Option<Const>],
    cur: Value,
    src: &Value,
    mem: MemLimit,
) -> Result<usize, PathError> {
    let slot = match keys.split_last() {
        None => target,
        Some((last, inner)) => {
            let mut c = target;
            for (i, k) in inner.iter().enumerate() {
                c = index_mut(c, k, create.get(i).and_then(|c| c.as_ref()))?;
            }
            match c {
                Value::List(l) => {
                    let len = l.len();
                    let i = match last {
                        Value::Int(i) => *i,
                        v => bad("int", v),
                    };
                    match usize::try_from(i).ok().filter(|i| *i < len) {
                        Some(i) => &mut Rc::make_mut(l)[i],
                        None => {
                            return Err(("IndexError", format!("list index {i} is out of range (length {len})")).into())
                        }
                    }
                }
                Value::Map(m) => match Rc::make_mut(m).entry(last.to_key()) {
                    indexmap::map::Entry::Occupied(e) => e.into_mut(),
                    // Removed while `src` was evaluated: set again.
                    indexmap::map::Entry::Vacant(e) => {
                        let v = concat_pair(cur, src, mem).map_err(PathError::Memory)?;
                        let n = str_len(&v);
                        e.insert(v);
                        return Ok(n);
                    }
                },
                v => bad("list or map", v),
            }
        }
    };
    match (&*slot, &cur) {
        (Value::Str(a), Value::Str(b)) if Rc::ptr_eq(a, b) => {
            drop(cur);
            append_value(slot, src, mem).map_err(PathError::Memory)
        }
        _ => {
            let v = concat_pair(cur, src, mem).map_err(PathError::Memory)?;
            let n = str_len(&v);
            *slot = v;
            Ok(n)
        }
    }
}

/// Ordering of two floats for `<`, `<=` and `<=>`, like `compare` (NaN compares equal).
fn float_order(x: f64, y: f64) -> std::cmp::Ordering {
    x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal)
}

pub fn const_value(c: &Const) -> Value {
    match c {
        Const::Null => Value::Null,
        Const::Bool(b) => Value::Bool(*b),
        Const::Int(i) => Value::Int(*i),
        Const::Float(f) => Value::Float(*f),
        Const::Str(s) => Value::str(s.as_str()),
        Const::EmptyList => Value::list(Vec::new()),
        Const::EmptyMap => Value::map(IndexMap::new()),
        Const::EmptySet => Value::set(crate::value::KeySet::new()),
    }
}

/// `values_equal(&const_value(c), v)`, without making the constant's value.
fn const_equal(c: &Const, v: &Value) -> bool {
    match (c, v) {
        (Const::Null, Value::Null) => true,
        (Const::Bool(x), Value::Bool(y)) => x == y,
        (Const::Int(x), Value::Int(y)) => x == y,
        (Const::Int(x), Value::Float(y)) => (*x as f64) == *y,
        (Const::Float(x), Value::Float(y)) => x == y,
        (Const::Float(x), Value::Int(y)) => *x == (*y as f64),
        (Const::Str(x), Value::Str(y)) => x.as_str() == y.as_str(),
        (Const::EmptyList, Value::List(l)) => l.is_empty(),
        (Const::EmptyMap, Value::Map(m)) => m.is_empty(),
        (Const::EmptySet, Value::Set(s)) => s.is_empty(),
        _ => false,
    }
}

impl Vm {
    pub fn new(prog: Arc<Program>, builtins: Arc<[BuiltinFn]>, html_output: bool, limits: Limits) -> Vm {
        use std::hash::{BuildHasher, Hasher};
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(Instant::now().elapsed().as_nanos());
        let seed = h.finish() | 1;
        let nconsts = prog.consts.len();
        let gen_class = prog.exc_classes.get("Generator").copied().unwrap_or(u32::MAX);
        Vm {
            code: Rc::new(Arc::clone(&prog)),
            prog,
            builtins,
            regs: Vec::with_capacity(256),
            frames: Vec::new(),
            handlers: Vec::new(),
            guards: Vec::new(),
            flushes: 0,
            consts: vec![None; nconsts],
            out: vec![OutBuf { buf: String::new(), html: html_output }],
            http: HttpCtx::default(),
            js_output: false,
            host: Arc::new(Host::default()),
            resources: Vec::new(),
            jit: None,
            jit_rt: JitRt::default(),
            jit_stack_limit: 0,
            jit_paused: None,
            limits,
            mem_base: crate::alloc::thread_allocated(),
            deadline: limits.max_time.map(|d| Instant::now() + d),
            budget: BUDGET,
            echoed: 0,
            equal_since: Cell::new(0),
            rng: seed,
            native_depth: 0,
            plain_bodies: Vec::new(),
            slow_callbacks: false,
            argbuf: Vec::new(),
            keybuf: Vec::new(),
            jit_rerun: Vec::new(),
            enum_cache: Vec::new(),
            field_defaults: RefCell::new(Vec::new()),
            objects: RefCell::new(Vec::new()),
            traces: RefCell::new(Traces::default()),
            gc_next: std::cell::Cell::new(GC_FIRST),
            gc_due: std::cell::Cell::new(false),
            gc_base: Cell::new(0),
            gc_visited: Cell::new(0),
            gc_pressure: limits.max_memory.map_or(isize::MAX, |m| (m / 4 * 3).min(isize::MAX as usize) as isize),
            gen_links: Vec::new(),
            gen_class,
            coverage: None,
            profile: None,
            debug: None,
        }
    }

    pub fn run_main(&mut self) -> Result<(), VmError> {
        let main = self.prog.main;
        self.call_function(main, &[]).map(|_| ())
    }

    /// The call stack, outermost first (for the debugger).
    pub fn debug_frames(&self) -> Vec<DebugFrame> {
        self.frames.iter().map(|f| DebugFrame { func: f.func, pc: f.pc, base: f.base }).collect()
    }

    /// Register `i` of the register file (`DebugFrame::base` plus a register number).
    pub fn register(&self, i: usize) -> Option<&Value> {
        self.regs.get(i)
    }

    /// Takes everything written to the base output buffer so far.
    pub fn take_output(&mut self) -> String {
        self.out.truncate(1);
        std::mem::take(&mut self.out[0].buf)
    }

    /// `response()->flush()` took the first `sent` bytes of the base output
    /// and sent them: a `catch` can no longer take them back, so each `try`
    /// that began before now drops only what was written after them.
    pub fn output_flushed(&mut self, sent: usize) {
        self.flushes += 1;
        for h in &mut self.handlers {
            if h.out_depth == 1 {
                h.out_len = h.out_len.saturating_sub(sent);
            }
        }
    }

    /// Calls a PHV function from Rust (also used by builtins such as `map`).
    pub fn call_function(&mut self, func: u32, args: &[Value]) -> Result<Value, VmError> {
        self.call_inner(func, &[], args)
    }

    /// Calls a closure value from Rust.
    pub fn call_value(&mut self, callee: &Value, args: &[Value]) -> Result<Value, VmError> {
        match callee {
            // `callee` is not in the VM, so its captures need no copy of the closure.
            Value::Closure(c) => self.call_inner(c.func, &c.caps, args),
            v => bad("closure", v),
        }
    }

    /// Prepares closure `callee` for the calls a builtin makes to it once per
    /// element (`map`, `filter`, `sortBy` ...): what `call_value` would check
    /// before every call is checked here once, and `Callback::call` skips it.
    /// A closure whose body has a `try` (so a `finally`), one the JIT
    /// compiled, or every closure once `set_slow_callbacks` was called, takes
    /// the full path of `call_value` at each call.
    pub fn callback(&mut self, callee: &Value) -> Callback {
        let clo = match callee {
            Value::Closure(c) => Rc::clone(c),
            v => bad("closure", v),
        };
        // The stack probe and the native depth are the same for every call
        // the builtin makes: it calls from one place, with the same frames
        // below it. The frame bound stays per call.
        let fast = !self.slow_callbacks
            && self.body_is_plain(clo.func)
            && !self.jit.as_ref().is_some_and(|j| j.is_compiled(clo.func))
            && self.native_depth < MAX_NATIVE_DEPTH
            && !crate::stack::left_below(CALL_STACK);
        Callback { clo, fast }
    }

    /// Tests only: `Callback::call` takes the full path of `call_value` for
    /// every closure (the differential test runs the callbacks both ways).
    pub fn set_slow_callbacks(&mut self, slow: bool) {
        self.slow_callbacks = slow;
    }

    /// Whether function `func` has no `try` (so no `finally`) and is not a
    /// generator (whose frame is kept): looked up once per function and kept.
    fn body_is_plain(&mut self, func: u32) -> bool {
        if self.plain_bodies.is_empty() {
            self.plain_bodies = vec![0; self.prog.funcs.len()];
        }
        let kind = &mut self.plain_bodies[func as usize];
        if *kind == 0 {
            let code = &self.prog.funcs[func as usize].code;
            *kind = if code.iter().any(|i| matches!(i, Instr::TryBegin { .. } | Instr::GenStart(_))) { 2 } else { 1 };
        }
        *kind == 1
    }

    /// `call_inner` for a `Callback` that `callback` checked: the stack probe
    /// and the native depth were checked there, and the register window is
    /// not emptied and filled with nulls between two calls (as between the
    /// interpreter's own calls, the callee writes a register before it reads
    /// it). The captures and arguments are bound, the frame is pushed (an
    /// exception's trace reads it) and the time limit is ticked as in every
    /// call.
    fn call_fast(&mut self, func: u32, caps: &[Value], args: &[Value]) -> Result<Value, VmError> {
        if self.frames.len() >= self.limits.max_frames {
            return Err(VmError::Fatal("maximum call stack depth exceeded".into()));
        }
        self.tick()?;
        let base = match self.frames.last() {
            Some(f) => f.base + self.prog.funcs[f.func as usize].num_regs as usize,
            None => 0,
        };
        let nregs = self.prog.funcs[func as usize].num_regs as usize;
        let need = base + nregs.max(caps.len() + args.len());
        if self.regs.len() < need {
            self.grow_regs(need)?;
        }
        let window = &mut self.regs[base..need];
        // The captures again at each call: the body may change its copy
        // (`$count++`), and every call starts from the captured value.
        for (slot, v) in window.iter_mut().zip(caps.iter().chain(args.iter())) {
            *slot = v.clone();
        }
        self.frames.push(Frame { func, pc: 0, base, ret: 0, stop: true, gen: false });
        self.native_depth += 1;
        let r = self.exec();
        self.native_depth -= 1;
        r
    }

    /// `==` of two values: comparing large strings or lists can take long in one
    /// step, so the time limit is checked every 16 MB compared.
    pub fn equal(&self, a: &Value, b: &Value) -> Result<bool, VmError> {
        // Scalars and strings shorter than one timed step are compared at
        // once; lists, maps, sets and longer strings take the metered walk.
        let long = matches!((a, b), (Value::Str(x), Value::Str(y)) | (Value::Html(x), Value::Html(y))
            if x.len().min(y.len()) + 1 >= EQUAL_STEP);
        if !long {
            if let Some(r) = crate::value::flat_equal(a, b) {
                if let (Value::Str(x), Value::Str(y)) | (Value::Html(x), Value::Html(y)) = (a, b) {
                    let since = self.equal_since.get() + x.len().min(y.len());
                    if since >= EQUAL_STEP {
                        self.equal_since.set(0);
                        self.check_limits()?;
                    } else {
                        self.equal_since.set(since);
                    }
                }
                return Ok(r);
            }
        }
        let deadline = self.deadline;
        let mut since = self.equal_since.get();
        let mut over: Option<VmError> = None;
        let r = crate::value::values_equal_metered(a, b, &mut |charge| {
            let bytes = match charge {
                // A list of pairs growing is reserved first, whatever its size.
                Charge::Alloc(bytes) => {
                    if let Err(e) = self.reserve(bytes) {
                        over = Some(e);
                    }
                    return over.is_none();
                }
                Charge::Work(bytes) => bytes,
            };
            // The limits are looked at every `EQUAL_STEP` of work.
            since = since.saturating_add(bytes);
            if since >= EQUAL_STEP {
                since = 0;
                if let Err(e) = self.check_memory() {
                    over = Some(e);
                } else if deadline.is_some_and(|d| Instant::now() > d) {
                    over = self.check_limits().err();
                }
            }
            over.is_none()
        });
        self.equal_since.set(since);
        match (r, over) {
            (Some(v), _) => Ok(v),
            (None, Some(e)) => Err(e),
            (None, None) => self.check_limits().map(|()| false),
        }
    }

    fn call_inner(&mut self, func: u32, caps: &[Value], args: &[Value]) -> Result<Value, VmError> {
        // Each call from Rust nests the interpreter on the thread's stack: the
        // depth is bounded, and so is the stack it may take (a thread with
        // less than the usual room, or larger frames, stops here instead of
        // overflowing).
        if self.native_depth >= MAX_NATIVE_DEPTH
            || self.frames.len() >= self.limits.max_frames
            || crate::stack::left_below(CALL_STACK)
        {
            return Err(VmError::Fatal("maximum call stack depth exceeded".into()));
        }
        // A callback from a builtin (`map`, `sortBy` ...) counts as a call: one
        // without loops would otherwise never reach a time-limit check.
        self.tick()?;
        let base = match self.frames.last() {
            Some(f) => f.base + self.prog.funcs[f.func as usize].num_regs as usize,
            None => 0,
        };
        let nregs = self.prog.funcs[func as usize].num_regs as usize;
        self.regs.truncate(base);
        self.grow_regs(base + nregs.max(caps.len() + args.len()))?;
        for (i, v) in caps.iter().chain(args.iter()).enumerate() {
            self.regs[base + i] = v.clone();
        }
        self.frames.push(Frame { func, pc: 0, base, ret: 0, stop: true, gen: false });
        self.native_depth += 1;
        let r = self.exec();
        self.native_depth -= 1;
        r
    }

    /// Records an object, so that its fields are cleared when the run ends (a
    /// reference cycle through it would otherwise outlive the request).
    pub fn track(&self, o: &Rc<RefCell<Object>>) {
        let mut objects = self.objects.borrow_mut();
        if objects.len() == objects.capacity() && objects.len() >= 1024 {
            // Forget objects that are already gone before the list grows.
            let before = objects.len();
            objects.retain(|w| w.strong_count() > 0);
            self.gc_base.set(self.gc_base.get().saturating_sub(before - objects.len()));
            if objects.len() >= self.gc_next.get() {
                self.gc_due.set(true);
            }
            // The next check comes after as many new objects as are alive now
            // (room grown as well as shrunk: found by the 1.0 audit, a list
            // left nearly full was searched again at every `new`).
            room_for_twice(&mut objects);
        }
        objects.push(Rc::downgrade(o));
        let len = objects.len();
        drop(objects);
        // Memory is looked at every 8 objects (each look reads a thread-local).
        if len.is_multiple_of(8) && self.memory_pressed(len) {
            self.gc_due.set(true);
        }
    }

    /// Objects made since the cycle collector last ran, about (some of them
    /// may be counted although gone).
    fn made_since_collection(&self) -> usize {
        self.objects.borrow().len().saturating_sub(self.gc_base.get())
    }

    /// Memory is running short and objects were made since the collector
    /// last ran: the cycles among them may hold it. At most `GC_WORK` values
    /// are looked at per object made, as when the collector runs by count.
    fn memory_pressed(&self, objects: usize) -> bool {
        let made = objects.saturating_sub(self.gc_base.get());
        made > 0 && made.saturating_mul(GC_WORK) >= self.gc_visited.get() && self.mem().used() > self.gc_pressure
    }

    /// Frees the objects that are reachable only through reference cycles
    /// and returns how many there were. Runs on its own when the number of
    /// live objects has doubled since the last time, or later when the last
    /// walk was long (a large list held by a live object is walked each
    /// time): at most `GC_WORK` values are looked at per object made.
    pub fn collect_cycles(&mut self) -> usize {
        self.collect_within(false)
    }

    /// `collect_cycles`, with the collector's own memory counted against the
    /// script's limit (see `cycles`). When the limit has been reached
    /// (`last`: the script stops unless memory is freed), it may still use an
    /// eighth of the limit, so that a few objects holding much can be freed.
    fn collect_within(&mut self, last: bool) -> usize {
        let budget = match self.limits.max_memory {
            None => usize::MAX,
            Some(max) => {
                let left = max.saturating_sub(self.mem().used().max(0) as usize);
                if last {
                    left.max(max / 8)
                } else {
                    left
                }
            }
        };
        self.gc_due.set(false);
        let done = crate::cycles::collect(self.objects.get_mut(), budget);
        self.gc_visited.set(done.visited);
        let objects = self.objects.get_mut();
        let live = objects.len();
        self.gc_base.set(live);
        room_for_twice(objects);
        self.gc_next.set((live.saturating_mul(2)).max(live + done.visited / GC_WORK).max(GC_FIRST));
        done.freed
    }

    /// The message catalogs of the program (`std.i18n`), if it has them.
    pub fn catalogs(&self) -> Option<&Arc<crate::i18n::Catalogs>> {
        self.code.i18n.as_ref().map(|s| &s.0)
    }

    /// Creates an instance of a built-in exception class such as `ValueError`.
    /// The exception of a `yield` reached while the generator's loop closes it
    /// (kept out of the dispatch loop: it is rare).
    #[cold]
    #[inline(never)]
    fn yield_while_closing(&self) -> VmError {
        self.throw("LogicException", "a generator closed by its loop cannot yield again")
    }

    pub fn throw(&self, class: &str, msg: impl Into<String>) -> VmError {
        let Some(&cid) = self.prog.exc_classes.get(class) else {
            return VmError::Fatal(format!("{class}: {}", msg.into()));
        };
        let cls = &self.prog.classes[cid as usize];
        let mut fields = self.new_fields(cid);
        if let Some(i) = cls.fields.iter().position(|f| f.name == "message") {
            fields[i] = Value::str(msg);
        }
        let obj = Rc::new(RefCell::new(Object { class: cid, fields }));
        self.track(&obj);
        VmError::Exception(Box::new(Exc::new(Value::Object(obj))))
    }

    /// `ClassName: message` for an exception value, then `(caused by ...)` for
    /// each exception it was thrown because of.
    pub fn describe_exception(&self, e: &Exc) -> String {
        let mut out = String::new();
        let mut cur = e.value.clone();
        let mut seen: Vec<*const RefCell<Object>> = Vec::new();
        // A bounded walk: a class could make an exception its own cause.
        for depth in 0..16 {
            let Value::Object(o) = &cur else {
                out.push_str(cur.type_name());
                break;
            };
            if seen.contains(&Rc::as_ptr(o)) {
                out.push_str(" (caused by ...");
                out.push_str(&")".repeat(depth));
                return out;
            }
            seen.push(Rc::as_ptr(o));
            let next = {
                let o = o.borrow();
                let cls = &self.prog.classes[o.class as usize];
                let field = |name: &str| cls.fields.iter().position(|f| f.name == name).map(|i| o.fields[i].clone());
                let msg = field("message").map(|v| v.to_display_string()).unwrap_or_default();
                if depth > 0 {
                    out.push_str(" (caused by ");
                }
                out.push_str(&cls.name);
                if !msg.is_empty() {
                    out.push_str(": ");
                    out.push_str(&msg);
                }
                field("previous")
            };
            match next {
                Some(v @ Value::Object(_)) => cur = v,
                _ => {
                    out.push_str(&")".repeat(depth));
                    return out;
                }
            }
        }
        out.push_str(" ...");
        out.push_str(&")".repeat(15));
        out
    }

    pub fn class_name(&self, class: u32) -> &str {
        &self.prog.classes[class as usize].name
    }

    /// Writes a value to the current output buffer, escaping plain strings
    /// when the buffer is HTML.
    pub fn echo(&mut self, v: &Value) {
        let out = self.out.last_mut().unwrap();
        match v {
            Value::Str(s) if out.html => html_escape_into(s, &mut out.buf),
            other => other.append_to(&mut out.buf),
        }
    }

    /// `Echo` (or `EchoAttr`, with `attr` set) of `v` in frame `fi`, for
    /// native code. The output methods below are what `dispatch` runs for
    /// the same instructions, written there again: called from `dispatch`,
    /// they made the interpreter's other paths 5-13% slower (its code
    /// layout); the differential tests of `phv_driver/tests/jit.rs` run both.
    #[inline(never)]
    pub(crate) fn echo_value(&mut self, fi: usize, v: Value, attr: bool) -> Result<(), VmError> {
        if !self.guards.is_empty() {
            self.output_allowed(fi)?;
        }
        if self.js_output && self.out.len() == 1 && !inert_js(&v) {
            return Err(self.throw(
                "ValueError",
                "only `json_html()` values and numbers can be written to a JavaScript response (a string or template output would be code)",
            ));
        }
        // The output buffer grows with every echo; large pieces
        // (and escaping, up to 6x) are checked before they are added.
        let len = match &v {
            Value::Str(s) => s.len().saturating_mul(6),
            Value::Html(s) => s.len(),
            _ => 0,
        };
        let before = self.out.last().map(|o| o.buf.len()).unwrap_or(0);
        if len > 1 << 12 || before > 1 << 20 && before.next_power_of_two() < before + len + 64 {
            self.reserve(len.max(before))?;
        }
        match &v {
            Value::Html(s) if attr && self.out.last().is_some_and(|o| o.html) => {
                let buf = &mut self.out.last_mut().unwrap().buf;
                if s.contains(['"', '\'', '<', '>']) {
                    self.reserve(s.len().saturating_mul(6))?;
                    let buf = &mut self.out.last_mut().unwrap().buf;
                    attr_quotes_into(s, buf);
                } else {
                    buf.push_str(s);
                }
            }
            _ => self.echo(&v),
        }
        // Escaping a long value takes long; a template's output is
        // dropped after `render()`, so the buffer does not bound it
        // (found by the 1.0.0 audit).
        self.long_op(len);
        self.echoed(len);
        Ok(())
    }

    /// `EchoHtml` of `s` in frame `fi`.
    #[inline(never)]
    pub(crate) fn echo_html(&mut self, fi: usize, s: &str) -> Result<(), VmError> {
        if !self.guards.is_empty() {
            self.output_allowed(fi)?;
        }
        // As for `Echo`: a large piece, or a large buffer about
        // to grow, is counted before it is added.
        let len = s.len();
        let before = self.out.last().map(|o| o.buf.len()).unwrap_or(0);
        if len > 1 << 12 || before > 1 << 20 && before.next_power_of_two() < before + len + 64 {
            self.reserve(len.max(before))?;
        }
        self.out.last_mut().unwrap().buf.push_str(s);
        self.echoed(len);
        Ok(())
    }

    /// `EchoNonce` in frame `fi`.
    #[inline(never)]
    pub(crate) fn echo_nonce(&mut self, fi: usize) -> Result<(), VmError> {
        if !self.guards.is_empty() {
            self.output_allowed(fi)?;
        }
        if let Some(n) = &self.http.csp_nonce {
            let buf = &mut self.out.last_mut().unwrap().buf;
            buf.push_str(" nonce=\"");
            buf.push_str(n);
            buf.push('"');
        }
        Ok(())
    }

    /// `GuardOutput` in frame `fi`.
    #[inline(never)]
    pub(crate) fn guard_output(&mut self, fi: usize) {
        self.guards.push((fi, self.out.len(), self.flushes));
    }

    /// `EndGuard` in frame `fi`.
    #[inline(never)]
    pub(crate) fn end_guard(&mut self, fi: usize) {
        if matches!(self.guards.last(), Some(g) if g.0 == fi) {
            self.guards.pop();
        }
    }

    /// `EchoHtml` of the program's constant `c` (native code).
    pub(crate) fn echo_html_const(&mut self, fi: usize, c: usize) -> Result<(), VmError> {
        let prog = Arc::clone(&self.prog);
        match prog.consts.get(c) {
            Some(Const::Str(s)) => self.echo_html(fi, s),
            _ => Ok(()),
        }
    }

    /// `NewObject`: an object of `class` with its fields' defaults, for the
    /// interpreter and native code alike.
    #[inline(always)]
    pub(crate) fn new_object(&mut self, class: u32) -> Value {
        let fields = self.new_fields(class);
        let o = Rc::new(RefCell::new(Object { class, fields }));
        // An object that can hold no other object is never in a cycle.
        if !self.prog.classes[class as usize].acyclic {
            self.track(&o);
            if self.gc_due.get() {
                // Collect at the next loop step or call.
                self.budget = 1;
            }
        }
        Value::Object(o)
    }

    /// Output from frame `frame` (or deeper) is refused while a statement of a
    /// frame below it runs in HTML other than element text, into the same output.
    fn output_allowed(&self, frame: usize) -> Result<(), VmError> {
        let depth = self.out.len();
        if self.guards.iter().any(|&(f, d, _)| f < frame && d == depth) {
            return Err(VmError::Fatal(GUARDED_OUTPUT.to_string()));
        }
        Ok(())
    }

    /// For builtins that write output (`dump()`): refused anywhere in such HTML.
    pub fn check_output(&self) -> Result<(), VmError> {
        self.output_allowed(self.frames.len())
    }

    pub fn write_raw(&mut self, s: &str) {
        self.out.last_mut().unwrap().buf.push_str(s);
    }

    /// xorshift64*; not suitable for cryptography.
    pub fn next_random(&mut self) -> u64 {
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    /// The fields of a new object of `class`, with their default values.
    pub fn new_fields(&self, class: u32) -> Vec<Value> {
        let mut defaults = self.field_defaults.borrow_mut();
        if defaults.is_empty() {
            defaults.resize(self.prog.classes.len(), None);
        }
        defaults[class as usize]
            .get_or_insert_with(|| {
                self.prog.classes[class as usize].fields.iter().map(|f| const_value(&f.default)).collect()
            })
            .clone()
    }

    /// The singleton object for case `idx` of enum `class`.
    pub fn enum_case(&mut self, class: u32, idx: u32) -> Value {
        let (c, i) = (class as usize, idx as usize);
        if let Some(Some(v)) = self.enum_cache.get(c).and_then(|cases| cases.get(i)) {
            return v.clone();
        }
        let cls = &self.prog.classes[c];
        let (name, value) = &cls.cases[i];
        let mut fields = self.new_fields(class);
        if let Some(i) = cls.fields.iter().position(|f| f.name == "name") {
            fields[i] = Value::str(name.as_str());
        }
        if let Some(i) = cls.fields.iter().position(|f| f.name == "value") {
            fields[i] = const_value(value);
        }
        let v = Value::Object(Rc::new(RefCell::new(Object { class, fields })));
        if self.enum_cache.len() <= c {
            self.enum_cache.resize(self.prog.classes.len().max(c + 1), Vec::new());
        }
        let cases = &mut self.enum_cache[c];
        if cases.len() <= i {
            cases.resize(self.prog.classes[c].cases.len().max(i + 1), None);
        }
        cases[i] = Some(v.clone());
        v
    }

    fn load_const(&mut self, c: u32) -> Value {
        if let Some(v) = &self.consts[c as usize] {
            return v.clone();
        }
        let v = const_value(&self.prog.consts[c as usize]);
        self.consts[c as usize] = Some(v.clone());
        v
    }

    /// The calls on the stack now (see `ThrownAt`). At most the 50 innermost
    /// and 50 outermost are kept: a deep recursion would otherwise build
    /// hundreds of MB for one exception.
    fn thrown_at(&self) -> ThrownAt {
        let n = self.frames.len();
        let call = |f: &Frame| (f.func, f.pc);
        let calls: Rc<[(u32, u32)]> = if n <= 2 * TRACE_ENDS {
            self.frames.iter().rev().map(call).collect()
        } else {
            let (outer, inner) = (&self.frames[..TRACE_ENDS], &self.frames[n - TRACE_ENDS..]);
            inner.iter().rev().chain(outer.iter().rev()).map(call).collect()
        };
        ThrownAt { calls, depth: n }
    }

    /// `thrown_at` for an exception raised in native code: `native` are the
    /// native calls above the interpreter's frames, innermost first.
    fn thrown_at_native(&self, native: &[(u32, u32)]) -> ThrownAt {
        let n = self.frames.len() + native.len();
        let all = native.iter().copied().chain(self.frames.iter().rev().map(|f| (f.func, f.pc)));
        let calls: Rc<[(u32, u32)]> = if n <= 2 * TRACE_ENDS {
            all.collect()
        } else {
            all.enumerate().filter(|(k, _)| *k < TRACE_ENDS || *k >= n - TRACE_ENDS).map(|(_, c)| c).collect()
        };
        ThrownAt { calls, depth: n }
    }

    /// Calls `line` with the function, file and line of each call of `at`
    /// (names of at most 200 characters), and a line for the calls left out.
    fn trace_lines(&self, at: &ThrownAt, mut line: impl FnMut(&str, &str, usize)) {
        fn short(s: &str) -> std::borrow::Cow<'_, str> {
            match s.char_indices().nth(200) {
                Some((i, _)) => format!("{}...", &s[..i]).into(),
                None => s.into(),
            }
        }
        for (k, &(f, pc)) in at.calls.iter().enumerate() {
            if k == TRACE_ENDS && at.depth > 2 * TRACE_ENDS {
                line(&format!("... {} more calls ...", at.depth - 2 * TRACE_ENDS), "", 0);
            }
            let func = &self.prog.funcs[f as usize];
            let file = &self.prog.files[func.file as usize];
            let off = func.offsets.get((pc as usize).saturating_sub(1)).copied().unwrap_or(0);
            let n = match file.line_starts.binary_search(&off) {
                Ok(i) => i,
                Err(i) => i.saturating_sub(1),
            } + 1;
            line(&short(&func.name), &short(&file.name), n);
        }
    }

    /// The trace of an exception that left the script (an uncaught one): one
    /// line per call of where it was thrown, innermost first. Made when it is
    /// read; empty for an exception that was never thrown.
    pub fn exception_trace_lines(&self, e: &Exc) -> Vec<TraceLine> {
        let Some(at) = &e.at else { return Vec::new() };
        let mut lines = Vec::with_capacity(at.calls.len() + 1);
        self.trace_lines(at, |func, file, line| {
            lines.push(TraceLine { func: func.to_string(), file: file.to_string(), line })
        });
        lines
    }

    /// Notes where an exception was first thrown, for `getTraceAsString()`
    /// (`exception_trace`). A rethrown exception keeps the first place. The text
    /// is made only when it is asked for: most exceptions are caught unread.
    fn record_trace(&self, e: &Exc) {
        let (Value::Object(o), Some(at)) = (&e.value, &e.at) else { return };
        let key = Rc::as_ptr(o) as usize;
        let mut traces = self.traces.borrow_mut();
        if let Some((w, _)) = traces.by_object.get(&key) {
            if w.strong_count() > 0 {
                return;
            }
        }
        {
            let ob = o.borrow();
            if traces.trace_slot.is_empty() {
                let slot = |c: &Class| c.fields.iter().position(|f| f.name == "trace").map_or(u32::MAX, |i| i as u32);
                traces.trace_slot = self.prog.classes.iter().map(slot).collect();
            }
            let i = traces.trace_slot.get(ob.class as usize).copied().unwrap_or(u32::MAX);
            if i == u32::MAX || !matches!(&ob.fields[i as usize], Value::Str(s) if s.is_empty()) {
                return;
            }
        }
        // Many exceptions kept in a chain (`previous:`) each hold their place:
        // past the memory limit it is left out (found by the 1.0.0 audit; the
        // next check stops the script).
        if self.check_memory().is_err() {
            return;
        }
        if traces.by_object.len() >= traces.prune_at.max(1024) {
            traces.by_object.retain(|_, (w, _)| w.strong_count() > 0);
            traces.prune_at = traces.by_object.len() * 2;
        }
        traces.by_object.insert(key, (Rc::downgrade(o), at.clone()));
    }

    /// The text of the trace of exception object `o` (`getTraceAsString()`):
    /// one `at function (file:line)` line per call, innermost first, or empty.
    pub fn exception_trace(&self, o: &Rc<RefCell<Object>>) -> String {
        let key = Rc::as_ptr(o) as usize;
        let at = match self.traces.borrow().by_object.get(&key) {
            Some((w, at)) if w.upgrade().is_some_and(|x| Rc::ptr_eq(&x, o)) => at.clone(),
            _ => return String::new(),
        };
        let mut text = String::new();
        self.trace_lines(&at, |func, file, line| {
            if !text.is_empty() {
                text.push('\n');
            }
            let _ = write!(text, "at {func} ({file}:{line})");
        });
        text
    }

    /// After an instruction that went over `bytes` of a string: a long one
    /// makes the next loop step or call check the time limit (one instruction
    /// could otherwise take milliseconds, and thousands run between checks).
    #[inline]
    fn long_op(&mut self, bytes: usize) {
        if bytes >= LONG_OP {
            self.budget = 1;
        }
    }

    /// After output of `bytes`: the limits are checked once `LONG_OP` bytes
    /// were written since the last time, however small each piece (found by
    /// the 1.1 audit: a loop of literal HTML wrote 4 GB under 128 MB).
    fn echoed(&mut self, bytes: usize) {
        self.echoed += bytes;
        if self.echoed >= LONG_OP {
            self.echoed = 0;
            self.budget = 1;
        }
    }

    /// After hashing a map key of `bytes`: a long one is timed at once, since
    /// one step may look it up many times (found by the 1.0 audit).
    fn long_key(&mut self, bytes: usize) -> Result<(), VmError> {
        if bytes >= LONG_OP {
            self.check_limits()?;
        }
        Ok(())
    }

    #[inline]
    fn tick(&mut self) -> Result<(), VmError> {
        self.budget -= 1;
        if self.budget == 0 {
            return self.check_deadline();
        }
        Ok(())
    }

    /// The generator object a register holds (the compiler gives the
    /// generator instructions nothing else).
    fn gen_of(&self, v: &Value) -> Rc<RefCell<Object>> {
        match v {
            Value::Object(o) if o.borrow().class == self.gen_class => Rc::clone(o),
            v => bad("generator", v),
        }
    }

    /// `GenStart` in frame `base` of function `func`: the generator, which
    /// takes the first `n` registers (the captures and arguments) and resumes
    /// at `pc`.
    fn gen_start(&mut self, func: usize, pc: usize, base: usize, n: usize) -> Result<Value, VmError> {
        let nregs = self.prog.funcs[func].num_regs as usize;
        // Counted before it is made: a script keeping many generators stops
        // at its memory limit.
        self.reserve((G_REGS + nregs).saturating_mul(std::mem::size_of::<Value>()) + 64)?;
        let mut fields = Vec::with_capacity(G_REGS + nregs);
        fields.extend([
            Value::Int(func as i64),
            Value::Int(pc as i64),
            Value::Int(-1),
            Value::Int(G_NEW),
            Value::Int(0),
            Value::Null,
        ]);
        for i in 0..nregs {
            // The rest of the window holds what earlier calls left there.
            fields.push(if i < n { std::mem::take(&mut self.regs[base + i]) } else { Value::Null });
        }
        let o = Rc::new(RefCell::new(Object { class: self.gen_class, fields }));
        // Its frame may hold objects (a cycle through `$this`, say).
        self.track(&o);
        if self.gc_due.get() {
            self.budget = 1;
        }
        Ok(Value::Object(o))
    }

    /// Puts the frame of the generator of `link` on top of the stack, above
    /// the loop's frame (the top one now), from instruction `pc`.
    fn gen_resume(&mut self, link: GenLink, pc: u32) -> Result<(), VmError> {
        if self.frames.len() >= self.limits.max_frames {
            return Err(VmError::Fatal("maximum call stack depth exceeded".into()));
        }
        self.tick()?;
        let below = self.frames.last().unwrap();
        let base = below.base + self.prog.funcs[below.func as usize].num_regs as usize;
        let mut ob = link.gen.borrow_mut();
        let func = match ob.fields[G_FUNC] {
            Value::Int(f) => f as u32,
            ref v => bad("generator function", v),
        };
        let nregs = self.prog.funcs[func as usize].num_regs as usize;
        if self.regs.len() < base + nregs {
            drop(ob);
            self.grow_regs(base + nregs)?;
            ob = link.gen.borrow_mut();
        }
        for (r, v) in self.regs[base..base + nregs].iter_mut().zip(ob.fields.drain(G_REGS..)) {
            *r = v;
        }
        let fi = self.frames.len();
        let saved = std::mem::take(&mut ob.fields[G_HANDLERS]);
        if let Value::List(hs) = &saved {
            // Its `try` blocks, as if they began where it resumes.
            let out_depth = self.out.len();
            let out_len = self.out.last().map_or(0, |o| o.buf.len());
            for pair in hs.chunks(2) {
                if let [Value::Int(catch), Value::Int(exc)] = pair {
                    self.handlers.push(Handler {
                        frame: fi,
                        catch_pc: *catch as u32,
                        exc_reg: *exc as Reg,
                        out_depth,
                        out_len,
                        in_guard: false,
                        guards: self.guards.len(),
                        flushes: self.flushes,
                    });
                }
            }
        }
        ob.fields[G_STATE] = Value::Int(G_RUNNING);
        drop(ob);
        self.frames.push(Frame { func, pc, base, ret: 0, stop: false, gen: true });
        self.gen_links.push(link);
        Ok(())
    }

    /// `Yield` in frame `fi` (the generator `g`, from register `base`, with
    /// `nregs` registers): its frame and handlers go into the generator,
    /// which resumes at `resume` and waits at the `Yield` at `at`.
    fn gen_suspend(&mut self, g: &Rc<RefCell<Object>>, fi: usize, base: usize, nregs: usize, resume: u32, at: i64) {
        let mut handlers = Vec::new();
        while matches!(self.handlers.last(), Some(h) if h.frame == fi) {
            let h = self.handlers.pop().unwrap();
            handlers.push(Value::Int(h.exc_reg as i64));
            handlers.push(Value::Int(h.catch_pc as i64));
        }
        while matches!(self.guards.last(), Some(g) if g.0 >= fi) {
            self.guards.pop();
        }
        let mut ob = g.borrow_mut();
        ob.fields[G_HANDLERS] = if handlers.is_empty() {
            Value::Null
        } else {
            handlers.reverse();
            Value::list(handlers)
        };
        ob.fields.truncate(G_REGS);
        ob.fields.extend(self.regs[base..base + nregs].iter_mut().map(std::mem::take));
        ob.fields[G_PC] = Value::Int(resume as i64);
        ob.fields[G_AT] = Value::Int(at);
        ob.fields[G_STATE] = Value::Int(G_WAITING);
    }

    /// The generator is done: its frame is freed.
    fn gen_done(&self, g: &Rc<RefCell<Object>>) {
        let frame = {
            let mut ob = g.borrow_mut();
            ob.fields[G_STATE] = Value::Int(G_DONE);
            ob.fields[G_HANDLERS] = Value::Null;
            let frame = ob.fields.split_off(G_REGS);
            ob.fields.shrink_to_fit();
            frame
        };
        drop(frame);
    }

    /// The generators whose frames an exception (or a fatal error) left are
    /// done: they cannot resume.
    fn gens_left(&mut self) {
        while matches!(self.gen_links.last(), Some(l) if l.frame >= self.frames.len()) {
            let link = self.gen_links.pop().unwrap();
            self.gen_done(&link.gen);
        }
    }

    /// `CallVirtual`'s way into native code (`Call` has its own, as in 1.2.1),
    /// kept out of `dispatch`: inlined there a second time, it slowed the
    /// interpreter's other paths by a few percent. Runs `func` natively with
    /// the `argc` arguments from register `start` when it is compiled and
    /// native calls are not paused; None when the interpreter is to run the
    /// call.
    #[inline(never)]
    fn call_native(&mut self, func: u32, start: usize, argc: usize) -> Result<Option<Value>, VmError> {
        match self.jit_paused {
            Some(n) if self.frames.len() > n => return Ok(None),
            Some(_) => self.jit_paused = None,
            None => {}
        }
        let Some(unit) = self.jit.as_ref().and_then(|j| j.entry(func)).map(|i| i.call()) else {
            return Ok(None);
        };
        let prog = Arc::clone(&self.prog);
        let sig =
            prog.funcs[func as usize].native.as_ref().expect("JIT entry for a function without a native signature");
        let r = self.call_jit(unit.entry, unit.consts, func, sig, start, argc)?;
        if r.is_none() {
            // Out of native stack: the interpreter runs this call, and the
            // calls it makes, with the arguments put back.
            self.jit_paused = Some(self.frames.len());
        }
        Ok(r)
    }

    /// Runs natively compiled function `func` (whose native signature is
    /// `sig`) with the `argc` arguments in the registers from `start`, which
    /// it moves out as an interpreted call does. When the native code runs
    /// out of stack, puts the arguments back and returns None, so that the
    /// interpreter runs the call: a function that changes no object and
    /// writes no output (nor does any function it calls) has no effect other
    /// than its result, so running it again gives the same result. Another
    /// stops with an error instead (see `JitTable::impure`).
    fn call_jit(
        &mut self,
        entry: JitEntry,
        consts: *const u32,
        func: u32,
        sig: &NativeSig,
        start: usize,
        argc: usize,
    ) -> Result<Option<Value>, VmError> {
        // The rest of the unit's tables (looked up here, so that `dispatch`
        // passes what it passed in 1.2.1).
        let unit = match self.jit.as_ref().and_then(|j| j.entry(func)) {
            Some(i) => i.call(),
            None => panic!("phv vm: native call of a function without native code"),
        };
        debug_assert!(unit.entry as usize == entry as usize && unit.consts == consts);
        // The JIT compiles at most 16 parameters.
        let mut raw = [0i64; 16];
        if argc != sig.params.len() {
            panic!("phv vm: native call with {argc} arguments for {} parameters", sig.params.len());
        }
        for (i, (a, k)) in self.regs[start..start + argc].iter().zip(&sig.params).enumerate() {
            raw[i] = match (k, a) {
                (ValKind::Int, Value::Int(n)) => *n,
                (ValKind::Bool, Value::Bool(b)) => *b as i64,
                (ValKind::Float, Value::Float(f)) => f.to_bits() as i64,
                (ValKind::Str, Value::Str(_)) => 0,
                // Passed in the window.
                (ValKind::List(_), Value::List(_))
                | (ValKind::Map(..), Value::Map(_))
                | (ValKind::Opt(_), _)
                | (ValKind::Object(_), Value::Object(_))
                | (ValKind::OptObject(_), Value::Object(_) | Value::Null) => 0,
                (_, v) => bad("an argument of the native signature", v),
            };
        }
        let limits = JitLimits { deadline: self.deadline, max_memory: self.limits.max_memory, mem_base: self.mem_base };
        let mut rt = std::mem::take(&mut self.jit_rt);
        rt.prog.get_or_insert_with(|| Arc::clone(&self.prog));
        rt.consts = std::mem::take(&mut self.consts);
        rt.trace.clear();
        // Strings and lists are moved into the native call's window: kept for
        // a rerun, unless the function changes them in place (found by the 1.0
        // audit: the kept copy made that change copy a whole list), or a rerun
        // would repeat its effects. Numbers are passed in `raw` and stay in
        // their registers.
        let boxed_args = sig.params.iter().any(|p| p.boxed());
        let (writes, impure) = match &self.jit {
            Some(j) => (
                j.writes_args.get(func as usize).copied().unwrap_or(true),
                j.impure.get(func as usize).copied().unwrap_or(true),
            ),
            None => (true, true),
        };
        let kept = boxed_args && !writes && !impure;
        let args = &mut self.regs[start..start + argc];
        if kept {
            self.jit_rerun.clear();
            self.jit_rerun.extend(args.iter().cloned());
        }
        if boxed_args {
            rt.start(args);
        } else {
            rt.start(&mut []);
        }
        // Native code nested in this call (through the interpreter) shares the
        // outermost call's stack limit: `JIT_STACK` below here, but never
        // nearer the end of the thread's stack than `STACK_MARGIN` (found by
        // the 1.0.0 audit: entered deep inside callbacks of builtins, the
        // budget was measured from there, not from what the thread had left).
        let outermost = self.jit_stack_limit == 0;
        if outermost {
            let here = &raw as *const _ as usize;
            let mut limit = here.saturating_sub(crate::jit::JIT_STACK);
            if let Some(low) = crate::stack::stack_low() {
                limit = limit.max(low.saturating_add(crate::stack::STACK_MARGIN));
            }
            self.jit_stack_limit = limit.max(1);
        }
        let depth = self.frames.len();
        let mut ctx = JitCtx {
            error: 0,
            depth: depth as i64,
            max_depth: self.limits.max_frames as i64,
            budget: self.budget as i64,
            limits: &limits,
            detail: 0,
            rt: &mut rt,
            stack_limit: self.jit_stack_limit as i64,
            consts: unit.consts,
            vm: self as *mut Vm,
            funcs: unit.funcs,
            classes: unit.classes,
        };
        // SAFETY: the JIT creates entries only for functions it checked
        // (`phv_jit::well_formed`), the arguments match their native signature
        // (checked above), `consts`, `funcs` and `classes` are the tables of
        // the entry's unit (kept alive by `self.jit`), and `ctx`, `limits`
        // and `rt` outlive the call. Nothing uses `self` but the helpers,
        // through `ctx.vm`, until the call returns.
        let r = unsafe { (unit.entry)(&mut ctx, raw.as_ptr()) };
        if outermost {
            self.jit_stack_limit = 0;
        }
        let ret = rt.finish();
        self.consts = std::mem::take(&mut rt.consts);
        let mut native: Vec<(u32, u32)> = std::mem::take(&mut rt.trace);
        self.jit_rt = rt;
        self.budget = ctx.budget.clamp(1, BUDGET as i64) as u32;
        let error = ctx.error;
        let mut again = error == JIT_ERR_STACK && !impure;
        if error == JIT_ERR_MEMORY && !impure && (kept || !boxed_args) && self.made_since_collection() > 0 {
            // Native code cannot run the cycle collector between two of its
            // checks: when cycles made before this call held the memory, the
            // interpreter runs the call again once they are freed.
            again = self.collect_within(true) > 0 && self.check_memory().is_ok();
        }
        // The kept arguments are dropped now, so that they do not keep the
        // values shared; only a rerun takes them back.
        let rerun = if again && kept {
            for (r, v) in self.regs[start..start + argc].iter_mut().zip(self.jit_rerun.drain(..)) {
                *r = v;
            }
            true
        } else {
            self.jit_rerun.clear();
            false
        };
        // The calls of native code an exception left, as program functions.
        for (f, _) in &mut native {
            // SAFETY: `funcs` has `nfuncs` entries (see `Installed::call`).
            *f = if (*f as usize) < unit.nfuncs { unsafe { *unit.funcs.add(*f as usize) } } else { func };
        }
        let exc = match error {
            0 => {
                self.jit_rerun.clear();
                return Ok(Some(match sig.ret {
                    ValKind::Int => Value::Int(r),
                    ValKind::Bool => Value::Bool(r != 0),
                    ValKind::Float => Value::Float(f64::from_bits(r as u64)),
                    ValKind::Str
                    | ValKind::List(_)
                    | ValKind::Map(..)
                    | ValKind::Opt(_)
                    | ValKind::Object(_)
                    | ValKind::OptObject(_) => ret,
                    ValKind::Void => Value::Null,
                }));
            }
            JIT_ERR_STACK if (boxed_args || impure) && !rerun => {
                return Err(VmError::Fatal("maximum call stack depth exceeded".into()));
            }
            // The arguments are in their registers again (numbers were never moved).
            JIT_ERR_STACK => return Ok(None),
            JIT_ERR_MEMORY if again => return Ok(None),
            JIT_ERR_OVERFLOW => self.throw("ArithmeticError", "integer overflow"),
            JIT_ERR_DIV_ZERO => self.throw("DivisionByZeroError", "modulo by zero"),
            JIT_ERR_FDIV_ZERO => self.throw("DivisionByZeroError", "division by zero"),
            JIT_ERR_SHIFT => self.throw("ArithmeticError", "bit shift by a negative number"),
            JIT_ERR_FLOAT_TO_INT => {
                let f = f64::from_bits(ctx.detail as u64);
                self.throw("ValueError", format!("cannot convert {} to int", format_float(f)))
            }
            JIT_ERR_DEPTH => return Err(VmError::Fatal("maximum call stack depth exceeded".into())),
            JIT_ERR_THROW => {
                let (class, msg) = self.jit_rt.take_pending();
                self.throw(class, msg)
            }
            JIT_ERR_VM => self.jit_rt.take_error(),
            JIT_ERR_MEMORY => return Err(memory_error(self.limits.max_memory.unwrap_or(4 << 30))),
            _ => {
                let secs = self.limits.max_time.map(|t| t.as_secs_f64()).unwrap_or(0.0);
                return Err(VmError::Fatal(format!("maximum execution time of {secs} seconds exceeded")));
            }
        };
        // An exception raised in native code was thrown in its innermost
        // frame: its trace has the native calls above this one, as the
        // interpreter's would.
        Err(match exc {
            VmError::Exception(mut e) if e.at.is_none() => {
                e.at = Some(self.thrown_at_native(&native));
                self.record_trace(&e);
                VmError::Exception(e)
            }
            other => other,
        })
    }

    fn mem(&self) -> MemLimit {
        MemLimit { max: self.limits.max_memory, base: self.mem_base }
    }

    /// Grows the register file to `need` registers, checking the memory limit
    /// first: a call to a function with many registers, deep in a recursion,
    /// would otherwise allocate past the limit before the next check.
    fn grow_regs(&mut self, need: usize) -> Result<(), VmError> {
        if need > self.regs.capacity() {
            let cap = need.max(self.regs.capacity().saturating_mul(2));
            self.reserve(cap.saturating_mul(std::mem::size_of::<Value>()))?;
            self.regs.reserve_exact(cap - self.regs.len());
        }
        self.regs.resize(need, Value::Null);
        Ok(())
    }

    /// Fails before an allocation of `bytes` that would exceed the memory limit
    /// (or 4 GB without one). Builtins call this before building large values,
    /// because a failed allocation would abort the whole process.
    pub fn reserve(&self, bytes: usize) -> Result<(), VmError> {
        self.mem().reserve(bytes).map_err(memory_error)
    }

    /// The per-run memory limit in bytes, if any.
    pub fn memory_limit(&self) -> Option<usize> {
        self.limits.max_memory
    }

    /// Bytes the script may still allocate (4 GB when there is no limit).
    pub fn memory_left(&self) -> usize {
        let limit = self.limits.max_memory.unwrap_or(4 << 30);
        let used = (crate::alloc::thread_allocated() - self.mem_base).max(0) as usize;
        limit.saturating_sub(used)
    }

    /// The time limit of a run, if any.
    pub fn time_limit(&self) -> Option<Duration> {
        self.limits.max_time
    }

    /// The time limit as an instant (for code outside the interpreter loop, e.g. SQLite).
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// Checks the memory and time limits; long-running builtins call it as they go.
    pub fn check_limits(&self) -> Result<(), VmError> {
        self.after_builtin()
    }

    /// Runs after every builtin: builtins can take long or allocate a lot in one call.
    fn after_builtin(&self) -> Result<(), VmError> {
        self.check_memory()?;
        if self.deadline.is_some_and(|d| Instant::now() > d) {
            let secs = self.limits.max_time.map(|t| t.as_secs_f64()).unwrap_or(0.0);
            return Err(VmError::Fatal(format!("maximum execution time of {secs} seconds exceeded")));
        }
        Ok(())
    }

    /// Fails when the script has allocated more than its memory limit.
    pub fn check_memory(&self) -> Result<(), VmError> {
        self.mem().check().map_err(memory_error)
    }

    #[cold]
    pub(crate) fn check_deadline(&mut self) -> Result<(), VmError> {
        self.budget = BUDGET;
        if let Some(p) = &mut self.profile {
            self.budget = PROFILE_BUDGET;
            p.sample(self.frames.iter().map(|f| f.func));
        }
        let objects = self.objects.get_mut().len();
        if self.gc_due.get() || self.memory_pressed(objects) {
            self.collect_cycles();
        }
        if let Err(e) = self.check_memory() {
            // Cycles made since the collector last ran may hold the memory.
            if self.made_since_collection() == 0 {
                return Err(e);
            }
            self.collect_within(true);
            self.check_memory()?;
        }
        {
            if let Some(d) = self.deadline {
                if Instant::now() > d {
                    let secs = self.limits.max_time.map(|t| t.as_secs_f64()).unwrap_or(0.0);
                    return Err(VmError::Fatal(format!("maximum execution time of {secs} seconds exceeded")));
                }
            }
        }
        Ok(())
    }

    /// Runs until the frame on top of the stack (entered from Rust) returns.
    fn exec(&mut self) -> Result<Value, VmError> {
        let stop_at = self.frames.len() - 1;
        loop {
            match self.dispatch() {
                Ok(v) => return Ok(v),
                Err(VmError::Exception(mut e)) => {
                    if e.at.is_none() {
                        e.at = Some(self.thrown_at());
                        self.record_trace(&e);
                    }
                    match self.handlers.last() {
                        Some(h) if h.frame >= stop_at => {
                            let h = self.handlers.pop().unwrap();
                            // The HTML the exception left open (or the `try` began in)
                            // was sent by flush(): the `catch` cannot write where it
                            // began, so the page stops and its response is cut off.
                            let open = &self.guards[h.guards.min(self.guards.len())..];
                            if (h.in_guard && h.out_depth == 1 && h.flushes < self.flushes)
                                || open.iter().any(|&(_, d, at)| d == 1 && at < self.flushes)
                            {
                                self.unwind_to(stop_at);
                                return Err(VmError::Fatal(FLUSHED_GUARD.to_string()));
                            }
                            self.frames.truncate(h.frame + 1);
                            self.gens_left();
                            self.out.truncate(h.out_depth);
                            if h.in_guard || self.guards.len() > h.guards {
                                if let Some(o) = self.out.last_mut() {
                                    if o.buf.len() > h.out_len {
                                        o.buf.truncate(h.out_len);
                                    }
                                }
                            }
                            self.guards.truncate(h.guards);
                            let fr = &mut self.frames[h.frame];
                            fr.pc = h.catch_pc;
                            let base = fr.base;
                            let nregs = self.prog.funcs[fr.func as usize].num_regs as usize;
                            self.regs.truncate(base + nregs);
                            self.regs[base + h.exc_reg as usize] = e.value;
                        }
                        _ => {
                            self.unwind_to(stop_at);
                            return Err(VmError::Exception(e));
                        }
                    }
                }
                Err(fatal) => {
                    self.unwind_to(stop_at);
                    return Err(fatal);
                }
            }
        }
    }

    fn unwind_to(&mut self, stop_at: usize) {
        let base = self.frames[stop_at].base;
        self.frames.truncate(stop_at);
        self.gens_left();
        self.handlers.retain(|h| h.frame < stop_at);
        self.guards.retain(|g| g.0 < stop_at);
        self.regs.truncate(base);
    }

    /// The values of the keys of `p`, in the reused key buffer (give it back
    /// with `put_keys`).
    fn take_keys(&mut self, base: usize, p: &PathSet) -> Vec<Value> {
        let mut keys = std::mem::take(&mut self.keybuf);
        keys.clear();
        keys.extend(p.keys.iter().map(|k| self.regs[base + *k as usize].clone()));
        keys
    }

    fn put_keys(&mut self, mut keys: Vec<Value>) {
        keys.clear();
        self.keybuf = keys;
    }

    fn set_path(&mut self, base: usize, p: &PathSet) -> Result<(), VmError> {
        let keys = self.take_keys(base, p);
        let r = self.set_path_with(base, p, &keys);
        self.put_keys(keys);
        r
    }

    fn append_path(&mut self, base: usize, p: &PathSet, cur: Reg) -> Result<(), VmError> {
        let keys = self.take_keys(base, p);
        let r = self.append_path_with(base, p, cur, &keys);
        self.put_keys(keys);
        r
    }

    /// The bytes copying shared lists and maps on the way to the element at
    /// `keys` of the place of `p` may allocate: accounted (and timed) before
    /// it happens.
    fn reserve_path(&mut self, base: usize, p: &PathSet, keys: &[Value], push: bool) -> Result<(), VmError> {
        let cost = match p.base {
            PathBase::Local(r) => copy_cost(&self.regs[base + r as usize], keys, push),
            PathBase::Prop(o, slot) => match &self.regs[base + o as usize] {
                Value::Object(o) => copy_cost(&o.borrow().fields[slot as usize], keys, push),
                _ => 0,
            },
        };
        if cost > 0 {
            self.reserve(cost)?;
            self.long_op(cost);
        }
        Ok(())
    }

    fn append_path_with(&mut self, base: usize, p: &PathSet, cur: Reg, keys: &[Value]) -> Result<(), VmError> {
        self.reserve_path(base, p, keys, false)?;
        let cur = std::mem::take(&mut self.regs[base + cur as usize]);
        let src = self.regs[base + p.val as usize].clone();
        let mem = self.mem();
        let r = match p.base {
            PathBase::Local(r) => path_append(&mut self.regs[base + r as usize], keys, &p.create, cur, &src, mem),
            PathBase::Prop(o, slot) => {
                let obj = match &self.regs[base + o as usize] {
                    Value::Object(o) => o.clone(),
                    v => bad("object", v),
                };
                let mut ob = obj.borrow_mut();
                path_append(&mut ob.fields[slot as usize], keys, &p.create, cur, &src, mem)
            }
        };
        self.long_key(keys.iter().map(str_len).sum())?;
        match r {
            Ok(work) => {
                self.long_op(work);
                Ok(())
            }
            Err(PathError::Memory(limit)) => Err(memory_error(limit)),
            Err(PathError::Throw(class, msg)) => Err(self.throw(class, msg)),
        }
    }

    fn set_path_with(&mut self, base: usize, p: &PathSet, keys: &[Value]) -> Result<(), VmError> {
        let val = self.regs[base + p.val as usize].clone();
        // A shared list or map on the way is copied whole, and a full list grows.
        self.reserve_path(base, p, keys, p.push)?;
        let r = match p.base {
            PathBase::Local(r) => path_assign(&mut self.regs[base + r as usize], keys, &p.create, p.push, val),
            PathBase::Prop(o, slot) => {
                let obj = match &self.regs[base + o as usize] {
                    Value::Object(o) => o.clone(),
                    v => bad("object", v),
                };
                let mut ob = obj.borrow_mut();
                path_assign(&mut ob.fields[slot as usize], keys, &p.create, p.push, val)
            }
        };
        // Hashing long string keys takes time (found by the 1.0 audit).
        self.long_key(keys.iter().map(str_len).sum())?;
        r.map_err(|(class, msg)| self.throw(class, msg))
    }

    fn dispatch(&mut self) -> Result<Value, VmError> {
        let code = Rc::clone(&self.code);
        let prog: &Program = &code;
        'frame: loop {
            let fi = self.frames.len() - 1;
            let (fid, mut pc, base) = {
                let f = &self.frames[fi];
                (f.func as usize, f.pc as usize, f.base)
            };
            let func = &prog.funcs[fid];
            let code = &func.code[..];

            macro_rules! reg {
                ($r:expr) => {
                    self.regs[base + $r as usize]
                };
            }
            macro_rules! tri {
                ($e:expr) => {
                    match $e {
                        Ok(v) => v,
                        Err(err) => {
                            self.frames[fi].pc = pc as u32;
                            return Err(err);
                        }
                    }
                };
            }
            // A jump; one back (a loop's next pass, also by a test at its end)
            // counts toward the time limit's check.
            macro_rules! branch {
                ($t:expr) => {{
                    let t = $t as usize;
                    if t < pc {
                        tri!(self.tick());
                    }
                    pc = t;
                }};
            }
            macro_rules! int {
                ($r:expr) => {
                    match reg!($r) {
                        Value::Int(i) => i,
                        ref v => bad("int", v),
                    }
                };
            }
            macro_rules! float {
                ($r:expr) => {
                    match reg!($r) {
                        Value::Float(f) => f,
                        ref v => bad("float", v),
                    }
                };
            }
            macro_rules! checked {
                ($d:expr, $a:expr, $b:expr, $op:ident) => {{
                    let (x, y) = (int!($a), int!($b));
                    match x.$op(y) {
                        Some(v) => reg!($d) = Value::Int(v),
                        None => tri!(Err(self.throw("ArithmeticError", "integer overflow"))),
                    }
                }};
            }
            // Enters `callee` with `argc` arguments starting at register `args`.
            macro_rules! call {
                ($callee:expr, $dst:expr, $args:expr, $argc:expr, $caps:expr) => {{
                    let callee: u32 = $callee;
                    if self.frames.len() >= self.limits.max_frames {
                        tri!(Err(VmError::Fatal("maximum call stack depth exceeded".into())));
                    }
                    tri!(self.tick());
                    let caps: &[Value] = $caps;
                    let new_base = base + func.num_regs as usize;
                    let need =
                        new_base + (prog.funcs[callee as usize].num_regs as usize).max(caps.len() + $argc as usize);
                    if self.regs.len() < need {
                        tri!(self.grow_regs(need));
                    }
                    for (i, c) in caps.iter().enumerate() {
                        self.regs[new_base + i] = c.clone();
                    }
                    for i in 0..$argc as usize {
                        let v = std::mem::take(&mut self.regs[base + $args as usize + i]);
                        self.regs[new_base + caps.len() + i] = v;
                    }
                    self.frames[fi].pc = pc as u32;
                    self.frames.push(Frame { func: callee, pc: 0, base: new_base, ret: $dst, stop: false, gen: false });
                    continue 'frame;
                }};
            }
            macro_rules! ret {
                ($v:expr) => {{
                    let v = $v;
                    while matches!(self.handlers.last(), Some(h) if h.frame == fi) {
                        self.handlers.pop();
                    }
                    while matches!(self.guards.last(), Some(g) if g.0 >= fi) {
                        self.guards.pop();
                    }
                    let fr = self.frames.pop().unwrap();
                    if fr.gen {
                        // A generator ended: the loop over it goes to its exit.
                        drop(v);
                        let link = self.gen_links.pop().expect("phv vm: a generator frame without its link");
                        self.gen_done(&link.gen);
                        if link.end != GEN_CLOSING {
                            self.frames.last_mut().unwrap().pc = link.end;
                        }
                        continue 'frame;
                    }
                    // The callee's registers are left as they are: the next
                    // call overwrites them, and the type checker guarantees
                    // registers are written before they are read. (Clearing
                    // them here made `fib(30)` in the interpreter 1.6 times
                    // as slow; the values that matter are emptied where they
                    // are used: arguments are moved, `Concat` empties its
                    // operands and a `foreach` its copy of the collection.)
                    if fr.stop {
                        return Ok(v);
                    }
                    let caller = self.frames.last().unwrap();
                    let idx = caller.base + fr.ret as usize;
                    self.regs[idx] = v;
                    continue 'frame;
                }};
            }

            loop {
                let ins = &code[pc];
                pc += 1;
                match ins {
                    Instr::Const(d, c) => {
                        let v = self.load_const(*c);
                        reg!(*d) = v;
                    }
                    Instr::Null(d) => reg!(*d) = Value::Null,
                    Instr::Bool(d, b) => reg!(*d) = Value::Bool(*b),
                    Instr::Int(d, i) => reg!(*d) = Value::Int(*i),
                    Instr::Move(d, s) => {
                        let v = reg!(*s).clone();
                        reg!(*d) = v;
                    }

                    Instr::AddI(d, a, b) => checked!(*d, *a, *b, checked_add),
                    Instr::SubI(d, a, b) => checked!(*d, *a, *b, checked_sub),
                    Instr::MulI(d, a, b) => checked!(*d, *a, *b, checked_mul),
                    Instr::ModI(d, a, b) => {
                        let (x, y) = (int!(*a), int!(*b));
                        if y == 0 {
                            tri!(Err(self.throw("DivisionByZeroError", "modulo by zero")));
                        }
                        reg!(*d) = Value::Int(x.wrapping_rem(y));
                    }
                    Instr::PowI(d, a, b) => {
                        let (x, y) = (int!(*a), int!(*b));
                        if y < 0 {
                            tri!(Err(self.throw(
                                "ArithmeticError",
                                "negative exponent in int ** int; use a float base, e.g. 2.0 ** -1"
                            )));
                        }
                        match u32::try_from(y).ok().and_then(|y| x.checked_pow(y)) {
                            Some(v) => reg!(*d) = Value::Int(v),
                            None => tri!(Err(self.throw("ArithmeticError", "integer overflow"))),
                        }
                    }
                    Instr::NegI(d, a) => {
                        let x = int!(*a);
                        match x.checked_neg() {
                            Some(v) => reg!(*d) = Value::Int(v),
                            None => tri!(Err(self.throw("ArithmeticError", "integer overflow"))),
                        }
                    }
                    Instr::IncI(r, n) => {
                        let x = int!(*r);
                        match x.checked_add(*n) {
                            Some(v) => reg!(*r) = Value::Int(v),
                            None => tri!(Err(self.throw("ArithmeticError", "integer overflow"))),
                        }
                    }
                    Instr::AddIImm(d, a, n) => {
                        let x = int!(*a);
                        match x.checked_add(*n as i64) {
                            Some(v) => reg!(*d) = Value::Int(v),
                            None => tri!(Err(self.throw("ArithmeticError", "integer overflow"))),
                        }
                    }
                    Instr::SubIImm(d, a, n) => {
                        let x = int!(*a);
                        match x.checked_sub(*n as i64) {
                            Some(v) => reg!(*d) = Value::Int(v),
                            None => tri!(Err(self.throw("ArithmeticError", "integer overflow"))),
                        }
                    }
                    Instr::AddF(d, a, b) => reg!(*d) = Value::Float(float!(*a) + float!(*b)),
                    Instr::SubF(d, a, b) => reg!(*d) = Value::Float(float!(*a) - float!(*b)),
                    Instr::MulF(d, a, b) => reg!(*d) = Value::Float(float!(*a) * float!(*b)),
                    Instr::DivF(d, a, b) => {
                        let (x, y) = (float!(*a), float!(*b));
                        if y == 0.0 {
                            tri!(Err(self.throw("DivisionByZeroError", "division by zero")));
                        }
                        reg!(*d) = Value::Float(x / y);
                    }
                    Instr::ModF(d, a, b) => {
                        let (x, y) = (float!(*a), float!(*b));
                        if y == 0.0 {
                            tri!(Err(self.throw("DivisionByZeroError", "modulo by zero")));
                        }
                        reg!(*d) = Value::Float(x % y);
                    }
                    Instr::PowF(d, a, b) => reg!(*d) = Value::Float(float!(*a).powf(float!(*b))),
                    Instr::NegF(d, a) => reg!(*d) = Value::Float(-float!(*a)),
                    Instr::BitAnd(d, a, b) => reg!(*d) = Value::Int(int!(*a) & int!(*b)),
                    Instr::BitOr(d, a, b) => reg!(*d) = Value::Int(int!(*a) | int!(*b)),
                    Instr::BitXor(d, a, b) => reg!(*d) = Value::Int(int!(*a) ^ int!(*b)),
                    Instr::Shl(d, a, b) | Instr::Shr(d, a, b) => {
                        let (x, y) = (int!(*a), int!(*b));
                        if y < 0 {
                            tri!(Err(self.throw("ArithmeticError", "bit shift by a negative number")));
                        }
                        let left = matches!(ins, Instr::Shl(..));
                        let v = if y >= 64 {
                            if left || x >= 0 {
                                0
                            } else {
                                -1
                            }
                        } else if left {
                            x << y
                        } else {
                            x >> y
                        };
                        reg!(*d) = Value::Int(v);
                    }
                    Instr::BitNot(d, a) => reg!(*d) = Value::Int(!int!(*a)),
                    Instr::Not(d, a) => {
                        let v = match reg!(*a) {
                            Value::Bool(b) => !b,
                            ref v => bad("bool", v),
                        };
                        reg!(*d) = Value::Bool(v);
                    }

                    Instr::EqI(d, a, b) => reg!(*d) = Value::Bool(int!(*a) == int!(*b)),
                    Instr::LtI(d, a, b) => reg!(*d) = Value::Bool(int!(*a) < int!(*b)),
                    Instr::LeI(d, a, b) => reg!(*d) = Value::Bool(int!(*a) <= int!(*b)),
                    Instr::Eq(d, a, b) => {
                        let v = tri!(self.equal(&reg!(*a), &reg!(*b)));
                        self.long_op(str_len(&reg!(*a)));
                        reg!(*d) = Value::Bool(v);
                    }
                    Instr::Ne(d, a, b) => {
                        let v = !tri!(self.equal(&reg!(*a), &reg!(*b)));
                        self.long_op(str_len(&reg!(*a)));
                        reg!(*d) = Value::Bool(v);
                    }
                    Instr::Lt(d, a, b) => {
                        let v = compare(&reg!(*a), &reg!(*b)).is_lt();
                        self.long_op(str_len(&reg!(*a)));
                        reg!(*d) = Value::Bool(v);
                    }
                    Instr::Le(d, a, b) => {
                        let v = compare(&reg!(*a), &reg!(*b)).is_le();
                        self.long_op(str_len(&reg!(*a)));
                        reg!(*d) = Value::Bool(v);
                    }
                    Instr::Cmp(d, a, b) => {
                        let v = compare(&reg!(*a), &reg!(*b)) as i64;
                        self.long_op(str_len(&reg!(*a)));
                        reg!(*d) = Value::Int(v);
                    }
                    Instr::EqF(d, a, b) => reg!(*d) = Value::Bool(float!(*a) == float!(*b)),
                    Instr::NeF(d, a, b) => reg!(*d) = Value::Bool(float!(*a) != float!(*b)),
                    Instr::LtF(d, a, b) => reg!(*d) = Value::Bool(float_order(float!(*a), float!(*b)).is_lt()),
                    Instr::LeF(d, a, b) => reg!(*d) = Value::Bool(float_order(float!(*a), float!(*b)).is_le()),
                    Instr::CmpF(d, a, b) => reg!(*d) = Value::Int(float_order(float!(*a), float!(*b)) as i64),

                    Instr::IntToFloat(d, a) => reg!(*d) = Value::Float(int!(*a) as f64),
                    Instr::FloatToInt(d, a) => {
                        let f = float!(*a);
                        if !(i64::MIN as f64..-(i64::MIN as f64)).contains(&f) {
                            tri!(Err(self.throw("ValueError", format!("cannot convert {} to int", format_float(f)))));
                        }
                        reg!(*d) = Value::Int(f.trunc() as i64);
                    }
                    Instr::ToStr(d, a) => op_to_str(&mut self.regs[base..], *d, *a),
                    Instr::Concat(d, start, n, html) => {
                        let mem = self.mem();
                        tri!(op_concat(&mut self.regs[base..], mem, *d, *start, *n, *html).map_err(memory_error));
                        self.long_op(str_len(&reg!(*d)));
                    }
                    Instr::AppendStr(d, s) => {
                        let mem = self.mem();
                        let work = tri!(op_append(&mut self.regs[base..], mem, *d, *s).map_err(memory_error));
                        self.long_op(work);
                    }

                    Instr::Jump(t) => {
                        branch!(*t);
                    }
                    Instr::JumpIfFalse(r, t) => {
                        if matches!(reg!(*r), Value::Bool(false)) {
                            branch!(*t);
                        }
                    }
                    Instr::JumpIfTrue(r, t) => {
                        if matches!(reg!(*r), Value::Bool(true)) {
                            branch!(*t);
                        }
                    }
                    Instr::JumpIfNull(r, t) => {
                        if matches!(reg!(*r), Value::Null) {
                            branch!(*t);
                        }
                    }
                    Instr::JumpIfNotNull(r, t) => {
                        if !matches!(reg!(*r), Value::Null) {
                            branch!(*t);
                        }
                    }
                    Instr::JumpIfNotLtI(a, b, t) => {
                        if int!(*a) >= int!(*b) {
                            branch!(*t);
                        }
                    }
                    Instr::JumpIfNotLeI(a, b, t) => {
                        if int!(*a) > int!(*b) {
                            branch!(*t);
                        }
                    }
                    Instr::JumpIfNotLtIImm(a, n, t) => {
                        if int!(*a) >= *n as i64 {
                            branch!(*t);
                        }
                    }
                    Instr::JumpIfNotLeIImm(a, n, t) => {
                        if int!(*a) > *n as i64 {
                            branch!(*t);
                        }
                    }
                    Instr::JumpIfLtIImm(a, n, t) => {
                        if int!(*a) < *n as i64 {
                            branch!(*t);
                        }
                    }
                    Instr::JumpIfLeIImm(a, n, t) => {
                        if int!(*a) <= *n as i64 {
                            branch!(*t);
                        }
                    }
                    Instr::JumpIfEq(a, b, t) | Instr::JumpIfNotEq(a, b, t) => {
                        let eq = match (&reg!(*a), &reg!(*b)) {
                            (Value::Int(x), Value::Int(y)) => x == y,
                            _ => {
                                let eq = tri!(self.equal(&reg!(*a), &reg!(*b)));
                                self.long_op(str_len(&reg!(*a)));
                                eq
                            }
                        };
                        if eq == matches!(ins, Instr::JumpIfEq(..)) {
                            branch!(*t);
                        }
                    }
                    Instr::AppendPath { path, cur } => tri!(self.append_path(base, path, *cur)),

                    Instr::Call { dst, func: callee, args, argc } => {
                        let paused = match self.jit_paused {
                            Some(n) if self.frames.len() > n => true,
                            Some(_) => {
                                self.jit_paused = None;
                                false
                            }
                            None => false,
                        };
                        // The function's native code, as its entry and the
                        // program's indices of the constants its unit names.
                        let entry = if paused {
                            None
                        } else {
                            self.jit.as_ref().and_then(|j| j.entry(*callee)).map(|i| (i.entry, i.consts.as_ptr()))
                        };
                        if let Some((entry, consts)) = entry {
                            let sig = prog.funcs[*callee as usize]
                                .native
                                .as_ref()
                                .expect("JIT entry for a function without a native signature");
                            self.frames[fi].pc = pc as u32;
                            let start = base + *args as usize;
                            match tri!(self.call_jit(entry, consts, *callee, sig, start, *argc as usize)) {
                                Some(v) => {
                                    reg!(*dst) = v;
                                    continue;
                                }
                                // Out of native stack: the interpreter runs this call,
                                // and the calls it makes, with the arguments put back.
                                None => self.jit_paused = Some(self.frames.len()),
                            }
                        }
                        call!(*callee, *dst, *args, *argc, &[])
                    }
                    Instr::CallVirtual { dst, slot, args, argc } => {
                        let class = match &reg!(*args) {
                            Value::Object(o) => o.borrow().class,
                            v => bad("object", v),
                        };
                        let callee = prog.classes[class as usize].vtable[*slot as usize];
                        if self.jit.as_ref().is_some_and(|j| j.is_compiled(callee)) {
                            self.frames[fi].pc = pc as u32;
                            if let Some(v) = tri!(self.call_native(callee, base + *args as usize, *argc as usize)) {
                                reg!(*dst) = v;
                                continue;
                            }
                        }
                        call!(callee, *dst, *args, *argc, &[])
                    }
                    Instr::CallClosure { dst, callee, args, argc } => {
                        let clo = match &reg!(*callee) {
                            Value::Closure(c) => c.clone(),
                            v => bad("closure", v),
                        };
                        call!(clo.func, *dst, *args, *argc, &clo.caps)
                    }
                    Instr::CallBuiltin { dst, id, args, argc } => {
                        let mut buf = std::mem::take(&mut self.argbuf);
                        buf.clear();
                        for i in 0..*argc as usize {
                            buf.push(std::mem::take(&mut self.regs[base + *args as usize + i]));
                        }
                        self.frames[fi].pc = pc as u32;
                        let f = self.builtins[*id as usize];
                        let r = f(self, &buf);
                        buf.clear();
                        self.argbuf = buf;
                        let v = tri!(r);
                        reg!(*dst) = v;
                        if prog.builtin_is_cheap(*id) {
                            tri!(self.check_memory());
                        } else {
                            tri!(self.after_builtin());
                        }
                    }
                    Instr::CallNamed { dst, name, args, argc } => {
                        let class = match &reg!(*args) {
                            Value::Object(o) => o.borrow().class,
                            v => bad("object", v),
                        };
                        let callee = match prog.classes[class as usize].method(*name) {
                            Some(f) => f,
                            None => {
                                let msg = format!(
                                    "{} has no method {}",
                                    prog.classes[class as usize].name, prog.method_names[*name as usize]
                                );
                                tri!(Err(VmError::Fatal(msg)))
                            }
                        };
                        call!(callee, *dst, *args, *argc, &[])
                    }
                    Instr::Ret(r) => ret!(std::mem::take(&mut reg!(*r))),
                    Instr::RetNull => ret!(Value::Null),
                    Instr::Enter => {
                        if let Some(p) = &mut self.profile {
                            p.calls[self.frames[fi].func as usize] += 1;
                            if p.last.is_none() {
                                p.last = Some(Instant::now());
                            }
                        }
                        // A step, so that calls from builtins (`map`...) are sampled too.
                        tri!(self.tick());
                    }
                    Instr::Cover(off) => {
                        if let Some(c) = &mut self.coverage {
                            c.insert((func.file, *off));
                        }
                        if let Some(mut hook) = self.debug.take() {
                            self.frames[fi].pc = pc as u32;
                            let r = hook.statement(self, func.file, *off);
                            self.debug = Some(hook);
                            tri!(r);
                        }
                    }

                    Instr::NewObject(d, class) => reg!(*d) = self.new_object(*class),
                    Instr::GetProp(d, o, slot) => {
                        let v = match &reg!(*o) {
                            Value::Object(ob) => ob.borrow().fields[*slot as usize].clone(),
                            v => bad("object", v),
                        };
                        reg!(*d) = v;
                    }
                    Instr::SetProp(o, slot, v) => {
                        let val = reg!(*v).clone();
                        match &reg!(*o) {
                            Value::Object(ob) => ob.borrow_mut().fields[*slot as usize] = val,
                            v => bad("object", v),
                        }
                    }
                    Instr::NewList(d, start, n) => {
                        let items: Vec<Value> = (0..*n as usize)
                            .map(|i| std::mem::take(&mut self.regs[base + *start as usize + i]))
                            .collect();
                        reg!(*d) = Value::list(items);
                    }
                    Instr::NewMap(d, start, n) => {
                        let mut m = IndexMap::with_capacity(*n as usize);
                        for i in 0..*n as usize {
                            let k = std::mem::take(&mut self.regs[base + *start as usize + 2 * i]);
                            let v = std::mem::take(&mut self.regs[base + *start as usize + 2 * i + 1]);
                            // Each long key is hashed: timed as a lookup is (found by
                            // the 1.0.0 audit, a way around the 1.0 fix).
                            tri!(self.long_key(str_len(&k)));
                            m.insert(k.to_key(), v);
                        }
                        reg!(*d) = Value::map(m);
                    }
                    Instr::Range(d, a, b, inclusive) => {
                        let (from, to) = (int!(*a), int!(*b));
                        let to = if *inclusive { to.saturating_add(1) } else { to };
                        let count = to.saturating_sub(from).max(0);
                        if count > 10_000_000 {
                            tri!(Err(self.throw("ValueError", "range is too large (more than 10,000,000 items)")));
                        }
                        tri!(self.reserve(count as usize * std::mem::size_of::<Value>()));
                        reg!(*d) = Value::list((from..to).map(Value::Int).collect());
                        // Up to 10,000,000 items in one step (found by the 1.0.0 audit).
                        self.long_op(count as usize * std::mem::size_of::<Value>());
                    }
                    Instr::Index(d, b, k) => {
                        let v = match (&reg!(*b), &reg!(*k)) {
                            (Value::List(l), Value::Int(i)) => match usize::try_from(*i).ok().and_then(|i| l.get(i)) {
                                Some(v) => v.clone(),
                                None => {
                                    let msg = format!("list index {i} is out of range (length {})", l.len());
                                    tri!(Err(self.throw("IndexError", msg)))
                                }
                            },
                            (Value::Map(m), key) => match m.get(&key.to_key()) {
                                Some(v) => v.clone(),
                                None => {
                                    let msg = format!("undefined map key {}", key_repr(key));
                                    tri!(Err(self.throw("KeyError", msg)))
                                }
                            },
                            (v, _) => bad("list or map", v),
                        };
                        let key_len = str_len(&reg!(*k));
                        reg!(*d) = v;
                        tri!(self.long_key(key_len));
                    }
                    Instr::IndexOrNull(d, b, k) => {
                        let v = match (&reg!(*b), &reg!(*k)) {
                            (Value::List(l), Value::Int(i)) => {
                                usize::try_from(*i).ok().and_then(|i| l.get(i)).cloned().unwrap_or(Value::Null)
                            }
                            (Value::Map(m), key) => m.get(&key.to_key()).cloned().unwrap_or(Value::Null),
                            (v, _) => bad("list or map", v),
                        };
                        let key_len = str_len(&reg!(*k));
                        reg!(*d) = v;
                        tri!(self.long_key(key_len));
                    }
                    Instr::SetPath(p) => tri!(self.set_path(base, p)),

                    Instr::IterInit(it, src) => {
                        if it != src {
                            let v = reg!(*src).clone();
                            reg!(*it) = v;
                        }
                        reg!(*it + 1) = Value::Int(0);
                    }
                    Instr::IterNext { it, key, val, end } => {
                        let idx = int!(*it + 1) as usize;
                        let item = match &reg!(*it) {
                            Value::List(l) => l.get(idx).map(|v| (Value::Int(idx as i64), v.clone())),
                            Value::Map(m) => m.get_index(idx).map(|(k, v)| (k.to_value(), v.clone())),
                            Value::Set(s) => s.get_index(idx).map(|k| (Value::Int(idx as i64), k.to_value())),
                            v => bad("list or map", v),
                        };
                        match item {
                            Some((k, v)) => {
                                reg!(*it + 1) = Value::Int(idx as i64 + 1);
                                if *key != NO_REG {
                                    reg!(*key) = k;
                                }
                                reg!(*val) = v;
                            }
                            None => pc = *end as usize,
                        }
                    }

                    Instr::Throw(r) => {
                        let value = reg!(*r).clone();
                        tri!(Err(VmError::Exception(Box::new(Exc::new(value)))))
                    }
                    Instr::TryBegin { catch, exc } => {
                        let out_depth = self.out.len();
                        let out_len = self.out.last().map_or(0, |o| o.buf.len());
                        let guards = self.guards.len();
                        let in_guard = self.guards.iter().any(|&(f, d, _)| f == fi && d == out_depth);
                        self.handlers.push(Handler {
                            frame: fi,
                            catch_pc: *catch,
                            exc_reg: *exc,
                            out_depth,
                            out_len,
                            in_guard,
                            guards,
                            flushes: self.flushes,
                        });
                    }
                    Instr::TryEnd(n) => {
                        let keep = self.handlers.len().saturating_sub(*n as usize);
                        self.handlers.truncate(keep);
                    }
                    Instr::InstanceOf(d, s, class) => {
                        let v = match &reg!(*s) {
                            Value::Object(o) => prog.is_subclass(o.borrow().class, *class),
                            _ => false,
                        };
                        reg!(*d) = Value::Bool(v);
                    }
                    Instr::IsType(d, s, tag) => {
                        let v = &reg!(*s);
                        let r = matches!(
                            (tag, v),
                            (TypeTag::Null, Value::Null)
                                | (TypeTag::Bool, Value::Bool(_))
                                | (TypeTag::Int, Value::Int(_))
                                | (TypeTag::Float, Value::Float(_))
                                | (TypeTag::Str, Value::Str(_))
                                | (TypeTag::Html, Value::Html(_))
                                | (TypeTag::List, Value::List(_))
                                | (TypeTag::Map, Value::Map(_))
                                | (TypeTag::Set, Value::Set(_))
                                | (TypeTag::Closure, Value::Closure(_))
                                | (TypeTag::Object, Value::Object(_))
                        );
                        reg!(*d) = Value::Bool(r);
                    }
                    Instr::MatchError(r) => {
                        let msg = format!("unhandled match case {}", short_repr(&reg!(*r)));
                        tri!(Err(self.throw("UnhandledMatchError", msg)))
                    }

                    Instr::EnumCase(d, class, idx) => {
                        let v = self.enum_case(*class, *idx);
                        reg!(*d) = v;
                    }
                    Instr::EnumCases(d, class) => {
                        let n = prog.classes[*class as usize].cases.len() as u32;
                        // Counted and timed by its size (found by the 1.1 audit: an
                        // enum of 200,000 cases ran 75 s under a 1 s limit).
                        let bytes = n as usize * std::mem::size_of::<Value>();
                        tri!(self.reserve(bytes));
                        let items = (0..n).map(|i| self.enum_case(*class, i)).collect();
                        reg!(*d) = Value::list(items);
                        self.long_op(bytes.saturating_mul(4));
                    }
                    Instr::EnumFrom { dst, class, val, try_ } => {
                        let wanted = reg!(*val).clone();
                        let cls = &prog.classes[*class as usize];
                        let found = cls.cases.iter().position(|(_, c)| const_equal(c, &wanted));
                        // Timed by the cases looked at, as `cases()` is (found by the
                        // 1.1 audit: 200,000 cases ran 2 s under a 1 s limit).
                        let looked = found.map_or(cls.cases.len(), |i| i + 1);
                        self.long_op(looked.saturating_mul(4 * std::mem::size_of::<Value>()));
                        let v = match found {
                            Some(i) => self.enum_case(*class, i as u32),
                            None if *try_ => Value::Null,
                            None => {
                                let msg = format!("{} is not a valid value for enum {}", short_repr(&wanted), cls.name);
                                tri!(Err(self.throw("ValueError", msg)))
                            }
                        };
                        reg!(*dst) = v;
                    }
                    Instr::MakeClosure { dst, func: f, caps, n } => {
                        let caps = (0..*n as usize).map(|i| self.regs[base + *caps as usize + i].clone()).collect();
                        reg!(*dst) = Value::Closure(Rc::new(Closure { func: *f, caps }));
                    }

                    Instr::GuardOutput => self.guards.push((fi, self.out.len(), self.flushes)),
                    Instr::EndGuard => {
                        if matches!(self.guards.last(), Some(g) if g.0 == fi) {
                            self.guards.pop();
                        }
                    }
                    Instr::Echo(r) | Instr::EchoAttr(r) => {
                        if !self.guards.is_empty() {
                            tri!(self.output_allowed(fi));
                        }
                        let v = reg!(*r).clone();
                        if self.js_output && self.out.len() == 1 && !inert_js(&v) {
                            tri!(Err(self.throw(
                                "ValueError",
                                "only `json_html()` values and numbers can be written to a JavaScript response (a string or template output would be code)",
                            )));
                        }
                        // The output buffer grows with every echo; large pieces
                        // (and escaping, up to 6x) are checked before they are added.
                        let len = match &v {
                            Value::Str(s) => s.len().saturating_mul(6),
                            Value::Html(s) => s.len(),
                            _ => 0,
                        };
                        let before = self.out.last().map(|o| o.buf.len()).unwrap_or(0);
                        if len > 1 << 12 || before > 1 << 20 && before.next_power_of_two() < before + len + 64 {
                            tri!(self.reserve(len.max(before)));
                        }
                        match (ins, &v) {
                            (Instr::EchoAttr(_), Value::Html(s)) if self.out.last().is_some_and(|o| o.html) => {
                                let buf = &mut self.out.last_mut().unwrap().buf;
                                if s.contains(['"', '\'', '<', '>']) {
                                    tri!(self.reserve(s.len().saturating_mul(6)));
                                    let buf = &mut self.out.last_mut().unwrap().buf;
                                    attr_quotes_into(s, buf);
                                } else {
                                    buf.push_str(s);
                                }
                            }
                            _ => self.echo(&v),
                        }
                        // Escaping a long value takes long; a template's output is
                        // dropped after `render()`, so the buffer does not bound it
                        // (found by the 1.0.0 audit).
                        self.long_op(len);
                        self.echoed(len);
                    }
                    Instr::EchoHtml(c) => {
                        if !self.guards.is_empty() {
                            tri!(self.output_allowed(fi));
                        }
                        if let Const::Str(s) = &prog.consts[*c as usize] {
                            // As for `Echo`: a large piece, or a large buffer about
                            // to grow, is counted before it is added.
                            let len = s.len();
                            let before = self.out.last().map(|o| o.buf.len()).unwrap_or(0);
                            if len > 1 << 12 || before > 1 << 20 && before.next_power_of_two() < before + len + 64 {
                                tri!(self.reserve(len.max(before)));
                            }
                            self.out.last_mut().unwrap().buf.push_str(s);
                            self.echoed(len);
                        }
                    }
                    Instr::EchoNonce => {
                        if !self.guards.is_empty() {
                            tri!(self.output_allowed(fi));
                        }
                        if let Some(n) = &self.http.csp_nonce {
                            let buf = &mut self.out.last_mut().unwrap().buf;
                            buf.push_str(" nonce=\"");
                            buf.push_str(n);
                            buf.push('"');
                        }
                    }
                    Instr::PushOut => self.out.push(OutBuf { buf: String::new(), html: true }),

                    Instr::GenStart(n) => {
                        let v = tri!(self.gen_start(fid, pc, base, *n as usize));
                        ret!(v)
                    }
                    Instr::GenInit(it) => {
                        let g = self.gen_of(&reg!(*it));
                        let mut ob = g.borrow_mut();
                        if matches!(ob.fields[G_STATE], Value::Int(G_NEW)) {
                            ob.fields[G_STATE] = Value::Int(G_WAITING);
                        } else {
                            drop(ob);
                            tri!(Err(self.throw("LogicException", "a generator can be iterated only once")));
                        }
                    }
                    Instr::GenNext { it, key, val, end } => {
                        let g = self.gen_of(&reg!(*it));
                        let state = match g.borrow().fields[G_STATE] {
                            Value::Int(s) => s,
                            ref v => bad("generator state", v),
                        };
                        match state {
                            G_DONE => pc = *end as usize,
                            G_WAITING => {
                                let at = match g.borrow().fields[G_PC] {
                                    Value::Int(p) => p as u32,
                                    ref v => bad("generator pc", v),
                                };
                                self.frames[fi].pc = pc as u32;
                                let link = GenLink { frame: fi + 1, gen: g, key: *key, val: *val, end: *end };
                                tri!(self.gen_resume(link, at));
                                continue 'frame;
                            }
                            _ => {
                                tri!(Err(self.throw("LogicException", "a generator cannot be iterated while it runs")))
                            }
                        }
                    }
                    Instr::GenClose(it) => {
                        let Value::Object(_) = &reg!(*it) else { continue };
                        let g = self.gen_of(&reg!(*it));
                        let (state, at, gfunc) = {
                            let ob = g.borrow();
                            match (&ob.fields[G_STATE], &ob.fields[G_AT], &ob.fields[G_FUNC]) {
                                (Value::Int(s), Value::Int(a), Value::Int(f)) => (*s, *a, *f as usize),
                                _ => bad("generator", &Value::Null),
                            }
                        };
                        if state == G_DONE || state == G_RUNNING {
                            continue;
                        }
                        // Waiting at a `yield` whose way out runs code (a `finally`
                        // block, or the closing of a generator it loops over): it
                        // runs from there; otherwise it is done at once.
                        let next = at.checked_add(1).and_then(|n| usize::try_from(n).ok());
                        let runs = state == G_WAITING
                            && next
                                .and_then(|n| prog.funcs[gfunc].code.get(n))
                                .is_some_and(|i| !matches!(i, Instr::RetNull));
                        if !runs {
                            self.gen_done(&g);
                            continue;
                        }
                        self.frames[fi].pc = pc as u32;
                        let link = GenLink { frame: fi + 1, gen: g, key: NO_REG, val: NO_REG, end: GEN_CLOSING };
                        tri!(self.gen_resume(link, next.unwrap_or(0) as u32));
                        continue 'frame;
                    }
                    Instr::Yield { key, val, from, resume } => {
                        let closing = self.gen_links.last().is_some_and(|l| l.frame == fi && l.end == GEN_CLOSING);
                        if closing {
                            // A generator closed by its loop returns from where it
                            // waited, and runs its `finally` blocks (which cannot
                            // hold a `yield`). Their code can still lead back to
                            // one: a `continue` to the generator's own loop, or a
                            // `catch` of what the block threw. There is no loop to
                            // take the value (as in PHP, an error): the exception
                            // is thrown in the generator, which goes on closing.
                            tri!(Err::<(), _>(self.yield_while_closing()));
                        }
                        let link = self.gen_links.pop().expect("phv vm: `yield` outside a running generator");
                        if link.frame != fi {
                            panic!("phv vm: a generator yielded outside its frame (compiler bug)");
                        }
                        let v = std::mem::take(&mut reg!(*val));
                        let k = {
                            let mut ob = link.gen.borrow_mut();
                            let next = match ob.fields[G_KEY] {
                                Value::Int(n) => n,
                                ref v => bad("generator key", v),
                            };
                            if *key == NO_REG {
                                ob.fields[G_KEY] = Value::Int(next.saturating_add(1));
                                Value::Int(next)
                            } else {
                                let k = std::mem::take(&mut reg!(*key));
                                // As in PHP: a key `n` makes the next key without one `n + 1`.
                                if let (false, Value::Int(n)) = (*from, &k) {
                                    if *n >= next {
                                        ob.fields[G_KEY] = Value::Int(n.saturating_add(1));
                                    }
                                }
                                k
                            }
                        };
                        self.gen_suspend(&link.gen, fi, base, func.num_regs as usize, *resume, pc as i64 - 1);
                        self.frames.pop();
                        let consumer = self.frames.last().unwrap().base;
                        if link.key != NO_REG {
                            self.regs[consumer + link.key as usize] = k;
                        }
                        self.regs[consumer + link.val as usize] = v;
                        continue 'frame;
                    }
                    Instr::PopOutHtml(d) => {
                        let buf = self.out.pop().map(|o| o.buf).unwrap_or_default();
                        reg!(*d) = Value::html(buf);
                    }
                }
            }
        }
    }
}

/// Whether a value may be written to a JavaScript response: a number, or text
/// made only of what `json_html()` writes (letters, digits, `"`, `\`, brackets,
/// `:`, `,`, `.`, `-`, `+` and non-ASCII non-space characters). Such text cannot
/// call anything (no parentheses, backticks or `=`), whatever it says.
fn inert_js(v: &Value) -> bool {
    match v {
        Value::Int(_) | Value::Float(_) | Value::Bool(_) | Value::Null => true,
        Value::Html(s) => {
            // The ASCII bytes are checked one by one, the other characters
            // (rare in such text) only when there are any.
            let ascii_ok = |b: u8| {
                b.is_ascii_alphanumeric()
                    || matches!(b, b'"' | b'\\' | b'[' | b']' | b'{' | b'}' | b':' | b',' | b'.' | b'-' | b'+')
            };
            s.bytes().all(|b| !b.is_ascii() || ascii_ok(b))
                && (s.is_ascii() || s.chars().all(|c| c.is_ascii() || (!c.is_whitespace() && c != '\u{feff}')))
        }
        _ => false,
    }
}

/// Bytes a path assignment may allocate: each shared list or map on the way is
/// copied (copy on write), and a list or map that is full grows.
pub(crate) fn copy_cost(target: &Value, keys: &[Value], push: bool) -> usize {
    let val = std::mem::size_of::<Value>();
    let entry = val * 2 + 16;
    let n = if push { keys.len() } else { keys.len().saturating_sub(1) };
    let mut cost = 0usize;
    let mut cur = target;
    for depth in 0..=n {
        let next = match cur {
            Value::List(l) => {
                if Rc::strong_count(l) > 1 {
                    cost = cost.saturating_add(l.len().saturating_mul(val));
                }
                if depth == n && push && l.len() == l.capacity() {
                    cost = cost.saturating_add(l.len().max(4).saturating_mul(val));
                }
                match keys.get(depth) {
                    Some(Value::Int(i)) if depth < n => usize::try_from(*i).ok().and_then(|i| l.get(i)),
                    _ => None,
                }
            }
            Value::Map(m) => {
                if Rc::strong_count(m) > 1 {
                    cost = cost.saturating_add(m.len().saturating_mul(entry));
                }
                if depth == n && m.len() == m.capacity() {
                    cost = cost.saturating_add(m.len().max(4).saturating_mul(entry));
                }
                match keys.get(depth) {
                    Some(k) if depth < n => m.get(&k.to_key()),
                    _ => None,
                }
            }
            // `$set[] = $x`: a set is always the last step.
            Value::Set(s) => {
                let item = std::mem::size_of::<crate::value::Key>() + 8;
                if Rc::strong_count(s) > 1 {
                    cost = cost.saturating_add(s.len().saturating_mul(item));
                }
                if depth == n && s.len() == s.capacity() {
                    cost = cost.saturating_add(s.len().max(4).saturating_mul(item));
                }
                None
            }
            _ => None,
        };
        match next {
            Some(v) => cur = v,
            None => break,
        }
    }
    cost
}

pub(crate) fn path_assign(
    target: &mut Value,
    keys: &[Value],
    create: &[Option<Const>],
    push: bool,
    val: Value,
) -> Result<(), (&'static str, String)> {
    let (inner, last) = if push { (keys, None) } else { (&keys[..keys.len() - 1], keys.last()) };
    let mut cur = target;
    for (i, k) in inner.iter().enumerate() {
        cur = index_mut(cur, k, create.get(i).and_then(|c| c.as_ref()))?;
    }
    match (cur, last) {
        (Value::List(l), None) => Rc::make_mut(l).push(val),
        (Value::List(l), Some(Value::Int(i))) => {
            let len = l.len();
            match usize::try_from(*i).ok().filter(|i| *i < len) {
                Some(i) => Rc::make_mut(l)[i] = val,
                None => return Err(("IndexError", format!("list index {i} is out of range (length {len})"))),
            }
        }
        (Value::Map(m), Some(k)) => {
            Rc::make_mut(m).insert(k.to_key(), val);
        }
        (Value::Set(s), None) => {
            Rc::make_mut(s).insert(val.to_key());
        }
        (v, _) => bad("list or map", v),
    }
    Ok(())
}

fn index_mut<'v>(v: &'v mut Value, k: &Value, create: Option<&Const>) -> Result<&'v mut Value, (&'static str, String)> {
    match v {
        Value::List(l) => {
            let len = l.len();
            let i = match k {
                Value::Int(i) => *i,
                v => bad("int", v),
            };
            match usize::try_from(i).ok().filter(|i| *i < len) {
                Some(i) => Ok(&mut Rc::make_mut(l)[i]),
                None => Err(("IndexError", format!("list index {i} is out of range (length {len})"))),
            }
        }
        Value::Map(m) => match (Rc::make_mut(m).entry(k.to_key()), create) {
            (indexmap::map::Entry::Occupied(e), _) => Ok(e.into_mut()),
            (indexmap::map::Entry::Vacant(e), Some(c)) => Ok(e.insert(const_value(c))),
            (indexmap::map::Entry::Vacant(_), None) => Err(("KeyError", format!("undefined map key {}", key_repr(k)))),
        },
        v => bad("list or map", v),
    }
}

pub(crate) fn key_repr(k: &Value) -> String {
    match k {
        Value::Str(s) => format!("\"{s}\""),
        other => other.to_display_string(),
    }
}

fn short_repr(v: &Value) -> String {
    match v {
        Value::Str(s) => {
            let mut t: String = s.chars().take(40).collect();
            if s.chars().count() > 40 {
                t.push('…');
            }
            format!("\"{t}\"")
        }
        Value::Int(_) | Value::Float(_) | Value::Bool(_) => v.to_display_string(),
        Value::Null => "null".into(),
        other => other.type_name().into(),
    }
}
