//! The register-based bytecode interpreter.
//!
//! Each `Vm` owns one heap and is used for exactly one script run or HTTP
//! request; the compiled `Program` is shared and immutable.

use crate::bytecode::*;
use crate::jit::*;
pub use crate::jit::{JitCtx, JitEntry, JitLimits, JitTable};
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
    /// Where the exception was first thrown, innermost call first. Filled in
    /// when it leaves the outermost call (`Vm::call_function`); until then
    /// only `at` is kept.
    pub trace: Vec<TraceLine>,
    at: Option<Box<ThrownAt>>,
}

impl Exc {
    /// An exception carrying `value`, not thrown yet.
    pub fn new(value: Value) -> Exc {
        Exc { value, trace: Vec::new(), at: None }
    }
}

/// The calls on the stack where an exception was first thrown: `(function,
/// pc)` of each call kept, innermost first, and how many calls there were.
#[derive(Clone)]
struct ThrownAt {
    calls: Vec<(u32, u32)>,
    depth: usize,
}

/// Where the exception objects of a run were first thrown, by object: the
/// text of a trace is made only when `getTraceAsString()` asks for it.
#[derive(Default)]
struct Traces {
    by_object: std::collections::HashMap<usize, (std::rc::Weak<RefCell<Object>>, ThrownAt)>,
    /// Entries of objects that are gone are dropped when there are this many.
    prune_at: usize,
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
    pub values: indexmap::IndexMap<String, String>,
    pub changed: bool,
    /// `regenerate()` was called: issue a new id (prevents session fixation).
    pub regenerate: bool,
    pub destroyed: bool,
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
            csrf_secret: Vec::new(),
            csrf_cookie: None,
            new_csrf_cookie: None,
            csp_nonce: None,
            status: 200,
            resp_headers: Vec::new(),
            content_type: None,
            body_file: None,
            personalized: false,
            params: Vec::new(),
            locals: Vec::new(),
            ws: None,
            publishes: Vec::new(),
            can_publish: false,
            request_id: None,
            traceparent: None,
            job: None,
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
}

/// Why output was refused (see `Vm::guards`).
const GUARDED_OUTPUT: &str = "a function wrote output inside a `<script>`, an attribute, a URL or CSS, where HTML \
    escaping does not protect it; return the value (or `Html`) from the function and insert it with `<?= ?>`";

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
    /// depth): functions they call may not write to that output, where text
    /// escaping would not protect what they write.
    guards: Vec<(usize, usize)>,
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
    budget: u32,
    /// Bytes echoed since the limits were last checked for output.
    echoed: usize,
    /// Bytes compared by `equal` since the limits were last checked: kept
    /// across calls, so a builtin comparing many values is timed as a whole
    /// (found by the 1.1 audit: `contains()` ran 18 s past a 1 s limit).
    equal_since: Cell<usize>,
    pub rng: u64,
    native_depth: u32,
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
pub(crate) fn op_append(regs: &mut [Value], mem: MemLimit, d: Reg, s: Reg) -> Result<usize, usize> {
    let src = regs[s as usize].clone();
    append_value(&mut regs[d as usize], &src, mem)
}

/// `$target .= $src` on a string, in place when it is not shared (see `op_append`).
#[inline]
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
        Vm {
            code: Rc::new(Arc::clone(&prog)),
            prog,
            builtins,
            regs: Vec::with_capacity(256),
            frames: Vec::new(),
            handlers: Vec::new(),
            guards: Vec::new(),
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
            argbuf: Vec::new(),
            keybuf: Vec::new(),
            jit_rerun: Vec::new(),
            enum_cache: Vec::new(),
            field_defaults: RefCell::new(Vec::new()),
            objects: RefCell::new(Vec::new()),
            traces: RefCell::new(Traces::default()),
            gc_next: std::cell::Cell::new(GC_FIRST),
            gc_due: std::cell::Cell::new(false),
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
        let r = crate::value::values_equal_metered(a, b, &mut |bytes| {
            since = since.saturating_add(bytes);
            // A large step (a long list's pairs about to be queued) is reserved
            // first; otherwise the limits are looked at every `EQUAL_STEP`.
            if bytes >= EQUAL_STEP {
                if let Err(e) = self.reserve(bytes) {
                    over = Some(e);
                    return false;
                }
            }
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
        if self.native_depth >= MAX_NATIVE_DEPTH || self.frames.len() >= self.limits.max_frames {
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
        self.frames.push(Frame { func, pc: 0, base, ret: 0, stop: true });
        self.native_depth += 1;
        let mut r = self.exec();
        self.native_depth -= 1;
        if let (Err(VmError::Exception(e)), true) = (&mut r, self.frames.is_empty()) {
            // Leaving the outermost call: the trace is read now.
            self.fill_trace(e);
        }
        r
    }

    /// Records an object, so that its fields are cleared when the run ends (a
    /// reference cycle through it would otherwise outlive the request).
    pub fn track(&self, o: &Rc<RefCell<Object>>) {
        let mut objects = self.objects.borrow_mut();
        if objects.len() == objects.capacity() && objects.len() >= 1024 {
            // Forget objects that are already gone before the list grows.
            objects.retain(|w| w.strong_count() > 0);
            if objects.len() >= self.gc_next.get() {
                self.gc_due.set(true);
            }
            // The next check comes after as many new objects as are alive now
            // (room grown as well as shrunk: found by the 1.0 audit, a list
            // left nearly full was searched again at every `new`).
            room_for_twice(&mut objects);
        }
        objects.push(Rc::downgrade(o));
    }

    /// Frees the objects that are reachable only through reference cycles
    /// and returns how many there were. Runs on its own when the number of
    /// live objects has doubled since the last time, or later when the last
    /// walk was long (a large list held by a live object is walked each
    /// time): at most `GC_WORK` values are looked at per object made.
    pub fn collect_cycles(&mut self) -> usize {
        self.gc_due.set(false);
        let done = crate::cycles::collect(self.objects.get_mut());
        let objects = self.objects.get_mut();
        let live = objects.len();
        room_for_twice(objects);
        self.gc_next.set((live.saturating_mul(2)).max(live + done.visited / GC_WORK).max(GC_FIRST));
        done.freed
    }

    /// Creates an instance of a built-in exception class such as `ValueError`.
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

    /// Output from frame `frame` (or deeper) is refused while a statement of a
    /// frame below it runs in HTML other than element text, into the same output.
    fn output_allowed(&self, frame: usize) -> Result<(), VmError> {
        let depth = self.out.len();
        if self.guards.iter().any(|&(f, d)| f < frame && d == depth) {
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
        let calls = self
            .frames
            .iter()
            .rev()
            .enumerate()
            .filter(|(i, _)| *i < TRACE_ENDS || *i >= n.saturating_sub(TRACE_ENDS))
            .map(|(_, f)| (f.func, f.pc))
            .collect();
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

    /// Fills in `e.trace` from where it was first thrown.
    fn fill_trace(&self, e: &mut Exc) {
        if let Some(at) = &e.at {
            let mut lines = Vec::with_capacity(at.calls.len() + 1);
            self.trace_lines(at, |func, file, line| {
                lines.push(TraceLine { func: func.to_string(), file: file.to_string(), line })
            });
            e.trace = lines;
        }
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
            let cls = &self.prog.classes[ob.class as usize];
            let Some(i) = cls.fields.iter().position(|f| f.name == "trace") else { return };
            if !matches!(&ob.fields[i], Value::Str(s) if s.is_empty()) {
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
        traces.by_object.insert(key, (Rc::downgrade(o), (**at).clone()));
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

    /// Runs natively compiled function `func` (whose native signature is
    /// `sig`) with the `argc` arguments in the registers from `start`, which
    /// it moves out as an interpreted call does. When the native code runs
    /// out of stack, puts the arguments back and returns None, so that the
    /// interpreter runs the call: compiled functions have no side effects
    /// (only arithmetic, strings and calls to other compiled functions), so
    /// running it again gives the same result.
    fn call_jit(
        &mut self,
        entry: JitEntry,
        func: u32,
        sig: &NativeSig,
        start: usize,
        argc: usize,
    ) -> Result<Option<Value>, VmError> {
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
                (ValKind::List(_), Value::List(_)) | (ValKind::Map(..), Value::Map(_)) | (ValKind::Opt(_), _) => 0,
                (_, v) => bad("an argument of the native signature", v),
            };
        }
        let limits = JitLimits { deadline: self.deadline, max_memory: self.limits.max_memory, mem_base: self.mem_base };
        let mut rt = std::mem::take(&mut self.jit_rt);
        rt.prog.get_or_insert_with(|| Arc::clone(&self.prog));
        rt.consts = std::mem::take(&mut self.consts);
        // Strings and lists are moved into the native call's window: kept for
        // a rerun, unless the function changes them in place (found by the 1.0
        // audit: the kept copy made that change copy a whole list). Numbers
        // are passed in `raw` and stay in their registers.
        let boxed_args = sig.params.iter().any(|p| p.boxed());
        let writes = self.jit.as_ref().is_some_and(|j| j.writes_args.get(func as usize).copied().unwrap_or(true));
        let kept = boxed_args && !writes;
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
        // outermost call's stack limit.
        let outermost = self.jit_stack_limit == 0;
        if outermost {
            let here = &raw as *const _ as usize;
            self.jit_stack_limit = here.saturating_sub(crate::jit::JIT_STACK).max(1);
        }
        let mut ctx = JitCtx {
            error: 0,
            depth: self.frames.len() as i64,
            max_depth: self.limits.max_frames as i64,
            budget: self.budget as i64,
            limits: &limits,
            detail: 0,
            rt: &mut rt,
            stack_limit: self.jit_stack_limit as i64,
        };
        // SAFETY: the JIT creates entries only for functions it checked
        // (`phv_jit::well_formed`), the arguments match their native signature
        // (checked above), and `ctx`, `limits` and `rt` outlive the call.
        let r = unsafe { entry(&mut ctx, raw.as_ptr()) };
        if outermost {
            self.jit_stack_limit = 0;
        }
        let ret = rt.finish();
        self.consts = std::mem::take(&mut rt.consts);
        self.jit_rt = rt;
        self.budget = ctx.budget.clamp(1, BUDGET as i64) as u32;
        let error = ctx.error;
        // The kept arguments are dropped now, so that they do not keep the
        // values shared; only a rerun takes them back.
        let rerun = if error == JIT_ERR_STACK && kept {
            for (r, v) in self.regs[start..start + argc].iter_mut().zip(self.jit_rerun.drain(..)) {
                *r = v;
            }
            true
        } else {
            self.jit_rerun.clear();
            false
        };
        match error {
            0 => {}
            JIT_ERR_STACK if boxed_args && !rerun => {
                return Err(VmError::Fatal("maximum call stack depth exceeded".into()));
            }
            // The arguments are in their registers again (numbers were never moved).
            JIT_ERR_STACK => return Ok(None),
            JIT_ERR_OVERFLOW => return Err(self.throw("ArithmeticError", "integer overflow")),
            JIT_ERR_DIV_ZERO => return Err(self.throw("DivisionByZeroError", "modulo by zero")),
            JIT_ERR_FDIV_ZERO => return Err(self.throw("DivisionByZeroError", "division by zero")),
            JIT_ERR_SHIFT => return Err(self.throw("ArithmeticError", "bit shift by a negative number")),
            JIT_ERR_FLOAT_TO_INT => {
                let f = f64::from_bits(ctx.detail as u64);
                return Err(self.throw("ValueError", format!("cannot convert {} to int", format_float(f))));
            }
            JIT_ERR_DEPTH => return Err(VmError::Fatal("maximum call stack depth exceeded".into())),
            JIT_ERR_THROW => {
                let (class, msg) = self.jit_rt.take_pending();
                return Err(self.throw(class, msg));
            }
            JIT_ERR_MEMORY => return Err(memory_error(self.limits.max_memory.unwrap_or(4 << 30))),
            _ => {
                let secs = self.limits.max_time.map(|t| t.as_secs_f64()).unwrap_or(0.0);
                return Err(VmError::Fatal(format!("maximum execution time of {secs} seconds exceeded")));
            }
        }
        Ok(Some(match sig.ret {
            ValKind::Int => Value::Int(r),
            ValKind::Bool => Value::Bool(r != 0),
            ValKind::Float => Value::Float(f64::from_bits(r as u64)),
            ValKind::Str | ValKind::List(_) | ValKind::Map(..) | ValKind::Opt(_) => ret,
            ValKind::Void => Value::Null,
        }))
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
    fn check_deadline(&mut self) -> Result<(), VmError> {
        self.budget = BUDGET;
        if let Some(p) = &mut self.profile {
            self.budget = PROFILE_BUDGET;
            p.sample(self.frames.iter().map(|f| f.func));
        }
        if self.gc_due.get() {
            self.collect_cycles();
        }
        self.check_memory()?;
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
                        e.at = Some(Box::new(self.thrown_at()));
                        self.record_trace(&e);
                    }
                    match self.handlers.last() {
                        Some(h) if h.frame >= stop_at => {
                            let h = self.handlers.pop().unwrap();
                            self.frames.truncate(h.frame + 1);
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
                    self.frames.push(Frame { func: callee, pc: 0, base: new_base, ret: $dst, stop: false });
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
                        let mut entry =
                            self.jit.as_ref().and_then(|j| j.entries.get(*callee as usize).copied().flatten());
                        match self.jit_paused {
                            Some(n) if self.frames.len() > n => entry = None,
                            Some(_) => self.jit_paused = None,
                            None => {}
                        }
                        if let Some(entry) = entry {
                            let sig = prog.funcs[*callee as usize]
                                .native
                                .as_ref()
                                .expect("JIT entry for a function without a native signature");
                            self.frames[fi].pc = pc as u32;
                            let start = base + *args as usize;
                            match tri!(self.call_jit(entry, *callee, sig, start, *argc as usize)) {
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

                    Instr::NewObject(d, class) => {
                        let fields = self.new_fields(*class);
                        let o = Rc::new(RefCell::new(Object { class: *class, fields }));
                        // An object that can hold no other object is never in a cycle.
                        if !self.prog.classes[*class as usize].acyclic {
                            self.track(&o);
                            if self.gc_due.get() {
                                // Collect at the next loop step or call.
                                self.budget = 1;
                            }
                        }
                        reg!(*d) = Value::Object(o);
                    }
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
                        let in_guard = self.guards.iter().any(|&(f, d)| f == fi && d == out_depth);
                        self.handlers.push(Handler {
                            frame: fi,
                            catch_pc: *catch,
                            exc_reg: *exc,
                            out_depth,
                            out_len,
                            in_guard,
                            guards,
                        });
                    }
                    Instr::TryEnd => {
                        self.handlers.pop();
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

                    Instr::GuardOutput => self.guards.push((fi, self.out.len())),
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
                                if s.contains(['"', '\'']) {
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
