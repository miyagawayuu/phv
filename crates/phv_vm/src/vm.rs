//! The register-based bytecode interpreter.
//!
//! Each `Vm` owns one heap and is used for exactly one script run or HTTP
//! request; the compiled `Program` is shared and immutable.

use crate::bytecode::*;
use crate::jit::*;
pub use crate::jit::{JitCtx, JitEntry, JitLimits, JitTable};
use crate::value::*;
use indexmap::IndexMap;
use std::cell::RefCell;
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
    pub trace: Vec<TraceLine>,
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

pub(crate) const BUDGET: u32 = 10_000;

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
}

pub struct Vm {
    pub prog: Arc<Program>,
    builtins: Arc<[BuiltinFn]>,
    regs: Vec<Value>,
    frames: Vec<Frame>,
    handlers: Vec<Handler>,
    consts: Vec<Option<Value>>,
    pub out: Vec<OutBuf>,
    pub http: HttpCtx,
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
    pub rng: u64,
    native_depth: u32,
    argbuf: Vec<Value>,
    /// Enum cases are singletons within one VM (one request).
    enum_cache: std::collections::HashMap<(u32, u32), Value>,
    /// Objects created by this run. Reference counting cannot free cycles
    /// (`$node->next = $node`), so their fields are cleared when the run ends.
    objects: Vec<std::rc::Weak<RefCell<Object>>>,
}

impl Drop for Vm {
    fn drop(&mut self) {
        for w in std::mem::take(&mut self.objects) {
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
    for p in parts {
        match p {
            // An `Html` result never contains unescaped plain strings.
            Value::Str(x) if html => html_escape_into(x, &mut s),
            v => v.append_to(&mut s),
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

pub(crate) fn op_append(regs: &mut [Value], mem: MemLimit, d: Reg, s: Reg) -> Result<(), usize> {
    let src = regs[s as usize].clone();
    let grow = match &src {
        Value::Str(x) | Value::Html(x) => x.len(),
        _ => 24,
    };
    let cur = match &regs[d as usize] {
        Value::Str(x) => x.capacity(),
        _ => 0,
    };
    if grow > 1 << 20 || (cur > 1 << 20 && cur < grow + cur) {
        // The string may double its buffer.
        mem.reserve(grow.max(cur) * 2)?;
    }
    let len = match &mut regs[d as usize] {
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
    Ok(())
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
            prog,
            builtins,
            regs: Vec::with_capacity(256),
            frames: Vec::new(),
            handlers: Vec::new(),
            consts: vec![None; nconsts],
            out: vec![OutBuf { buf: String::new(), html: html_output }],
            http: HttpCtx::default(),
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
            rng: seed,
            native_depth: 0,
            argbuf: Vec::new(),
            enum_cache: std::collections::HashMap::new(),
            objects: Vec::new(),
        }
    }

    pub fn run_main(&mut self) -> Result<(), VmError> {
        let main = self.prog.main;
        self.call_function(main, &[]).map(|_| ())
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
            Value::Closure(c) => {
                let c = c.clone();
                self.call_inner(c.func, &c.caps, args)
            }
            v => bad("closure", v),
        }
    }

    fn call_inner(&mut self, func: u32, caps: &[Value], args: &[Value]) -> Result<Value, VmError> {
        if self.native_depth >= MAX_NATIVE_DEPTH || self.frames.len() >= self.limits.max_frames {
            return Err(VmError::Fatal("maximum call stack depth exceeded".into()));
        }
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
        let r = self.exec();
        self.native_depth -= 1;
        r
    }

    /// Creates an instance of a built-in exception class such as `ValueError`.
    pub fn throw(&self, class: &str, msg: impl Into<String>) -> VmError {
        let Some(&cid) = self.prog.exc_classes.get(class) else {
            return VmError::Fatal(format!("{class}: {}", msg.into()));
        };
        let cls = &self.prog.classes[cid as usize];
        let mut fields: Vec<Value> = cls.fields.iter().map(|f| const_value(&f.default)).collect();
        if let Some(i) = cls.fields.iter().position(|f| f.name == "message") {
            fields[i] = Value::str(msg);
        }
        let obj = Value::Object(Rc::new(RefCell::new(Object { class: cid, fields })));
        VmError::Exception(Box::new(Exc { value: obj, trace: Vec::new() }))
    }

    /// `ClassName: message` for an exception value.
    pub fn describe_exception(&self, e: &Exc) -> String {
        match &e.value {
            Value::Object(o) => {
                let o = o.borrow();
                let cls = &self.prog.classes[o.class as usize];
                let msg = cls
                    .fields
                    .iter()
                    .position(|f| f.name == "message")
                    .map(|i| o.fields[i].to_display_string())
                    .unwrap_or_default();
                if msg.is_empty() {
                    cls.name.clone()
                } else {
                    format!("{}: {}", cls.name, msg)
                }
            }
            v => v.type_name().to_string(),
        }
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

    /// The singleton object for case `idx` of enum `class`.
    pub fn enum_case(&mut self, class: u32, idx: u32) -> Value {
        if let Some(v) = self.enum_cache.get(&(class, idx)) {
            return v.clone();
        }
        let cls = &self.prog.classes[class as usize];
        let (name, value) = &cls.cases[idx as usize];
        let mut fields: Vec<Value> = cls.fields.iter().map(|f| const_value(&f.default)).collect();
        if let Some(i) = cls.fields.iter().position(|f| f.name == "name") {
            fields[i] = Value::str(name.as_str());
        }
        if let Some(i) = cls.fields.iter().position(|f| f.name == "value") {
            fields[i] = const_value(value);
        }
        let v = Value::Object(Rc::new(RefCell::new(Object { class, fields })));
        self.enum_cache.insert((class, idx), v.clone());
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

    fn trace(&self) -> Vec<TraceLine> {
        self.frames
            .iter()
            .rev()
            .map(|f| {
                let func = &self.prog.funcs[f.func as usize];
                let file = &self.prog.files[func.file as usize];
                let off = func.offsets.get((f.pc as usize).saturating_sub(1)).copied().unwrap_or(0);
                let line = match file.line_starts.binary_search(&off) {
                    Ok(i) => i,
                    Err(i) => i.saturating_sub(1),
                } + 1;
                TraceLine { func: func.name.clone(), file: file.name.clone(), line }
            })
            .collect()
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

    #[inline]
    fn tick(&mut self) -> Result<(), VmError> {
        self.budget -= 1;
        if self.budget == 0 {
            return self.check_deadline();
        }
        Ok(())
    }

    /// Runs natively compiled function `func` with arguments `args`. When the
    /// native code runs out of stack, gives the arguments back (`Err`) so the
    /// interpreter runs the call: compiled functions have no side effects (only
    /// arithmetic, strings and calls to other compiled functions), so running
    /// it again gives the same result.
    fn call_jit(&mut self, entry: JitEntry, func: u32, args: Vec<Value>) -> Result<Result<Value, Vec<Value>>, VmError> {
        let prog = Arc::clone(&self.prog);
        let sig =
            prog.funcs[func as usize].native.as_ref().expect("JIT entry for a function without a native signature");
        // The JIT compiles at most 16 parameters.
        let mut raw = [0i64; 16];
        if args.len() != sig.params.len() {
            panic!("phv vm: native call with {} arguments for {} parameters", args.len(), sig.params.len());
        }
        for (i, (a, k)) in args.iter().zip(&sig.params).enumerate() {
            raw[i] = match (k, a) {
                (ValKind::Int, Value::Int(n)) => *n,
                (ValKind::Bool, Value::Bool(b)) => *b as i64,
                (ValKind::Float, Value::Float(f)) => f.to_bits() as i64,
                (ValKind::Str, Value::Str(_)) => 0,
                (_, v) => bad("an argument of the native signature", v),
            };
        }
        let limits = JitLimits { deadline: self.deadline, max_memory: self.limits.max_memory, mem_base: self.mem_base };
        let mut rt = std::mem::take(&mut self.jit_rt);
        rt.prog.get_or_insert_with(|| Arc::clone(&self.prog));
        rt.consts = std::mem::take(&mut self.consts);
        // Strings are moved into the native call's window: kept for a rerun.
        let strings = sig.params.contains(&ValKind::Str).then(|| args.clone());
        rt.start(args);
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
        match ctx.error {
            0 => {}
            JIT_ERR_STACK => {
                let args = strings.unwrap_or_else(|| {
                    sig.params
                        .iter()
                        .zip(raw)
                        .map(|(k, r)| match k {
                            ValKind::Bool => Value::Bool(r != 0),
                            ValKind::Float => Value::Float(f64::from_bits(r as u64)),
                            _ => Value::Int(r),
                        })
                        .collect()
                });
                return Ok(Err(args));
            }
            JIT_ERR_OVERFLOW => return Err(self.throw("ArithmeticError", "integer overflow")),
            JIT_ERR_DIV_ZERO => return Err(self.throw("DivisionByZeroError", "modulo by zero")),
            JIT_ERR_FDIV_ZERO => return Err(self.throw("DivisionByZeroError", "division by zero")),
            JIT_ERR_SHIFT => return Err(self.throw("ArithmeticError", "bit shift by a negative number")),
            JIT_ERR_FLOAT_TO_INT => {
                let f = f64::from_bits(ctx.detail as u64);
                return Err(self.throw("ValueError", format!("cannot convert {} to int", format_float(f))));
            }
            JIT_ERR_DEPTH => return Err(VmError::Fatal("maximum call stack depth exceeded".into())),
            JIT_ERR_MEMORY => return Err(memory_error(self.limits.max_memory.unwrap_or(4 << 30))),
            _ => {
                let secs = self.limits.max_time.map(|t| t.as_secs_f64()).unwrap_or(0.0);
                return Err(VmError::Fatal(format!("maximum execution time of {secs} seconds exceeded")));
            }
        }
        Ok(Ok(match sig.ret {
            ValKind::Int => Value::Int(r),
            ValKind::Bool => Value::Bool(r != 0),
            ValKind::Float => Value::Float(f64::from_bits(r as u64)),
            ValKind::Str => ret,
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
                    if e.trace.is_empty() {
                        e.trace = self.trace();
                    }
                    match self.handlers.last() {
                        Some(h) if h.frame >= stop_at => {
                            let h = self.handlers.pop().unwrap();
                            self.frames.truncate(h.frame + 1);
                            self.out.truncate(h.out_depth);
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
        self.regs.truncate(base);
    }

    fn set_path(&mut self, base: usize, p: &PathSet) -> Result<(), VmError> {
        let keys: Vec<Value> = p.keys.iter().map(|k| self.regs[base + *k as usize].clone()).collect();
        let val = self.regs[base + p.val as usize].clone();
        let r = match p.base {
            PathBase::Local(r) => path_assign(&mut self.regs[base + r as usize], &keys, &p.create, p.push, val),
            PathBase::Prop(o, slot) => {
                let obj = match &self.regs[base + o as usize] {
                    Value::Object(o) => o.clone(),
                    v => bad("object", v),
                };
                let mut ob = obj.borrow_mut();
                path_assign(&mut ob.fields[slot as usize], &keys, &p.create, p.push, val)
            }
        };
        r.map_err(|(class, msg)| self.throw(class, msg))
    }

    fn dispatch(&mut self) -> Result<Value, VmError> {
        let prog = Arc::clone(&self.prog);
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
                    let fr = self.frames.pop().unwrap();
                    // The callee's registers are left as they are: the next
                    // call overwrites them, and the type checker guarantees
                    // registers are written before they are read.
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
                        let v = values_equal(&reg!(*a), &reg!(*b));
                        self.long_op(str_len(&reg!(*a)));
                        reg!(*d) = Value::Bool(v);
                    }
                    Instr::Ne(d, a, b) => {
                        let v = !values_equal(&reg!(*a), &reg!(*b));
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
                        tri!(op_append(&mut self.regs[base..], mem, *d, *s).map_err(memory_error));
                        self.long_op(str_len(&reg!(*s)));
                    }

                    Instr::Jump(t) => {
                        if (*t as usize) < pc {
                            tri!(self.tick());
                        }
                        pc = *t as usize;
                    }
                    Instr::JumpIfFalse(r, t) => {
                        if matches!(reg!(*r), Value::Bool(false)) {
                            pc = *t as usize;
                        }
                    }
                    Instr::JumpIfTrue(r, t) => {
                        if matches!(reg!(*r), Value::Bool(true)) {
                            pc = *t as usize;
                        }
                    }
                    Instr::JumpIfNull(r, t) => {
                        if matches!(reg!(*r), Value::Null) {
                            pc = *t as usize;
                        }
                    }
                    Instr::JumpIfNotNull(r, t) => {
                        if !matches!(reg!(*r), Value::Null) {
                            pc = *t as usize;
                        }
                    }
                    Instr::JumpIfNotLtI(a, b, t) => {
                        if int!(*a) >= int!(*b) {
                            pc = *t as usize;
                        }
                    }
                    Instr::JumpIfNotLeI(a, b, t) => {
                        if int!(*a) > int!(*b) {
                            pc = *t as usize;
                        }
                    }
                    Instr::JumpIfNotLtIImm(a, n, t) => {
                        if int!(*a) >= *n as i64 {
                            pc = *t as usize;
                        }
                    }
                    Instr::JumpIfNotLeIImm(a, n, t) => {
                        if int!(*a) > *n as i64 {
                            pc = *t as usize;
                        }
                    }

                    Instr::Call { dst, func: callee, args, argc } => {
                        let mut entry =
                            self.jit.as_ref().and_then(|j| j.entries.get(*callee as usize).copied().flatten());
                        match self.jit_paused {
                            Some(n) if self.frames.len() > n => entry = None,
                            Some(_) => self.jit_paused = None,
                            None => {}
                        }
                        if let Some(entry) = entry {
                            // Arguments are moved, as into an interpreted call.
                            let start = base + *args as usize;
                            let argv: Vec<Value> =
                                self.regs[start..start + *argc as usize].iter_mut().map(std::mem::take).collect();
                            self.frames[fi].pc = pc as u32;
                            match tri!(self.call_jit(entry, *callee, argv)) {
                                Ok(v) => {
                                    reg!(*dst) = v;
                                    continue;
                                }
                                // Out of native stack: the interpreter runs this call,
                                // and the calls it makes, with the arguments put back.
                                Err(argv) => {
                                    for (i, v) in argv.into_iter().enumerate() {
                                        self.regs[start + i] = v;
                                    }
                                    self.jit_paused = Some(self.frames.len());
                                }
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
                        tri!(self.after_builtin());
                    }
                    Instr::CallNamed { dst, name, args, argc } => {
                        let class = match &reg!(*args) {
                            Value::Object(o) => o.borrow().class,
                            v => bad("object", v),
                        };
                        let callee = match prog.classes[class as usize].by_name.get(name) {
                            Some(f) => *f,
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

                    Instr::NewObject(d, class) => {
                        let fields =
                            prog.classes[*class as usize].fields.iter().map(|f| const_value(&f.default)).collect();
                        let o = Rc::new(RefCell::new(Object { class: *class, fields }));
                        if self.objects.len() == self.objects.capacity() && self.objects.len() >= 1024 {
                            // Forget objects that are already gone before the list grows.
                            self.objects.retain(|w| w.strong_count() > 0);
                        }
                        self.objects.push(Rc::downgrade(&o));
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
                        reg!(*d) = v;
                    }
                    Instr::IndexOrNull(d, b, k) => {
                        let v = match (&reg!(*b), &reg!(*k)) {
                            (Value::List(l), Value::Int(i)) => {
                                usize::try_from(*i).ok().and_then(|i| l.get(i)).cloned().unwrap_or(Value::Null)
                            }
                            (Value::Map(m), key) => m.get(&key.to_key()).cloned().unwrap_or(Value::Null),
                            (v, _) => bad("list or map", v),
                        };
                        reg!(*d) = v;
                    }
                    Instr::SetPath(p) => tri!(self.set_path(base, p)),

                    Instr::IterInit(it, src) => {
                        let v = reg!(*src).clone();
                        reg!(*it) = v;
                        reg!(*it + 1) = Value::Int(0);
                    }
                    Instr::IterNext { it, key, val, end } => {
                        let idx = int!(*it + 1) as usize;
                        let item = match &reg!(*it) {
                            Value::List(l) => l.get(idx).map(|v| (Value::Int(idx as i64), v.clone())),
                            Value::Map(m) => m.get_index(idx).map(|(k, v)| (k.to_value(), v.clone())),
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
                        tri!(Err(VmError::Exception(Box::new(Exc { value, trace: Vec::new() }))))
                    }
                    Instr::TryBegin { catch, exc } => {
                        let out_depth = self.out.len();
                        self.handlers.push(Handler { frame: fi, catch_pc: *catch, exc_reg: *exc, out_depth });
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
                        let items = (0..n).map(|i| self.enum_case(*class, i)).collect();
                        reg!(*d) = Value::list(items);
                    }
                    Instr::EnumFrom { dst, class, val, try_ } => {
                        let wanted = reg!(*val).clone();
                        let cls = &prog.classes[*class as usize];
                        let found = cls.cases.iter().position(|(_, c)| values_equal(&const_value(c), &wanted));
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

                    Instr::Echo(r) | Instr::EchoAttr(r) => {
                        let v = reg!(*r).clone();
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
                    }
                    Instr::EchoHtml(c) => {
                        if let Const::Str(s) = &prog.consts[*c as usize] {
                            self.out.last_mut().unwrap().buf.push_str(s);
                        }
                    }
                    Instr::EchoNonce => {
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

fn path_assign(
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
        Value::Map(m) => {
            let m = Rc::make_mut(m);
            let key = k.to_key();
            if let (false, Some(c)) = (m.contains_key(&key), create) {
                m.insert(key.clone(), const_value(c));
            }
            match m.get_mut(&key) {
                Some(v) => Ok(v),
                None => Err(("KeyError", format!("undefined map key {}", key_repr(k)))),
            }
        }
        v => bad("list or map", v),
    }
}

fn key_repr(k: &Value) -> String {
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
