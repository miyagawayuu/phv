//! The register-based bytecode interpreter.
//!
//! Each `Vm` owns one heap and is used for exactly one script run or HTTP
//! request; the compiled `Program` is shared and immutable.

use crate::bytecode::*;
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
    /// A Phenix exception; can be caught with `try`/`catch`.
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
    pub status: u16,
    pub resp_headers: Vec<(String, String)>,
    pub content_type: Option<String>,
}

impl Default for HttpCtx {
    fn default() -> Self {
        HttpCtx {
            method: "GET".into(),
            path: "/".into(),
            query: Vec::new(),
            form: Vec::new(),
            headers: Vec::new(),
            body: String::new(),
            files: Vec::new(),
            session: SessionData::default(),
            csrf_secret: Vec::new(),
            csrf_cookie: None,
            new_csrf_cookie: None,
            status: 200,
            resp_headers: Vec::new(),
            content_type: None,
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

/// What a script may access outside its own heap (from `phenix.toml`).
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
}

const MAX_NATIVE_DEPTH: u32 = 200;

// ----- JIT interface -------------------------------------------------------

/// Shared state between the VM and natively compiled code (layout is fixed:
/// the JIT reads and writes these fields by offset).
#[repr(C)]
pub struct JitCtx {
    /// Set by native code: 0 = ok, see `JIT_ERR_*`.
    pub error: i64,
    pub depth: i64,
    pub max_depth: i64,
    /// Decremented at loop back-edges and calls; at zero `phenix_jit_tick` runs.
    pub budget: i64,
    pub limits: *const JitLimits,
}

pub const JIT_ERR_OVERFLOW: i64 = 1;
pub const JIT_ERR_DIV_ZERO: i64 = 2;
pub const JIT_ERR_DEPTH: i64 = 3;
pub const JIT_ERR_TIME: i64 = 4;
pub const JIT_ERR_MEMORY: i64 = 5;
pub const JIT_ERR_SHIFT: i64 = 6;

pub struct JitLimits {
    pub deadline: Option<Instant>,
    pub max_memory: Option<usize>,
    pub mem_base: isize,
}

/// Called by native code when its budget runs out: checks the time and memory limits.
///
/// # Safety
/// `ctx` must point to a live `JitCtx` whose `limits` points to live `JitLimits`.
pub unsafe extern "C" fn phenix_jit_tick(ctx: *mut JitCtx) -> i64 {
    let ctx = &mut *ctx;
    ctx.budget = BUDGET as i64;
    let limits = &*ctx.limits;
    if limits.deadline.is_some_and(|d| Instant::now() > d) {
        return JIT_ERR_TIME;
    }
    if let Some(max) = limits.max_memory {
        if crate::alloc::thread_allocated() - limits.mem_base > max as isize {
            return JIT_ERR_MEMORY;
        }
    }
    0
}

/// Entry point of a compiled function: `(ctx, pointer to the i64 arguments) -> result`.
pub type JitEntry = unsafe extern "C" fn(*mut JitCtx, *const i64) -> i64;

/// Native code for the functions the JIT compiled, indexed by function id.
pub struct JitTable {
    pub entries: Vec<Option<JitEntry>>,
    /// Owns the executable memory the entries point into.
    pub keep_alive: Box<dyn std::any::Any + Send + Sync>,
}
const BUDGET: u32 = 10_000;

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
    limits: Limits,
    mem_base: isize,
    deadline: Option<Instant>,
    budget: u32,
    pub rng: u64,
    native_depth: u32,
    argbuf: Vec<Value>,
    /// Enum cases are singletons within one VM (one request).
    enum_cache: std::collections::HashMap<(u32, u32), Value>,
}

#[cold]
#[inline(never)]
fn bad(expected: &str, v: &Value) -> ! {
    panic!("phenix vm: expected {expected}, found {} (compiler bug)", v.type_name())
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
            limits,
            mem_base: crate::alloc::thread_allocated(),
            deadline: limits.max_time.map(|d| Instant::now() + d),
            budget: BUDGET,
            rng: seed,
            native_depth: 0,
            argbuf: Vec::new(),
            enum_cache: std::collections::HashMap::new(),
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

    /// Calls a Phenix function from Rust (also used by builtins such as `map`).
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
        self.regs.resize(base + nregs.max(caps.len() + args.len()), Value::Null);
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

    #[inline]
    fn tick(&mut self) -> Result<(), VmError> {
        self.budget -= 1;
        if self.budget == 0 {
            return self.check_deadline();
        }
        Ok(())
    }

    /// Runs natively compiled function `func` with arguments from `args`.
    fn call_jit(&mut self, entry: JitEntry, func: u32, args: &[Value]) -> Result<Value, VmError> {
        let mut raw = vec![0i64; args.len().max(16)];
        for (i, a) in args.iter().enumerate() {
            raw[i] = match a {
                Value::Int(n) => *n,
                Value::Bool(b) => *b as i64,
                v => bad("int or bool", v),
            };
        }
        let limits = JitLimits { deadline: self.deadline, max_memory: self.limits.max_memory, mem_base: self.mem_base };
        let mut ctx = JitCtx {
            error: 0,
            depth: self.frames.len() as i64,
            max_depth: self.limits.max_frames as i64,
            budget: self.budget as i64,
            limits: &limits,
        };
        // SAFETY: the JIT only creates entries for functions whose arguments
        // are all ints/bools (checked above), and `ctx`/`limits` outlive the call.
        let r = unsafe { entry(&mut ctx, raw.as_ptr()) };
        self.budget = ctx.budget.clamp(1, BUDGET as i64) as u32;
        match ctx.error {
            0 => {}
            JIT_ERR_OVERFLOW => return Err(self.throw("ArithmeticError", "integer overflow")),
            JIT_ERR_DIV_ZERO => return Err(self.throw("DivisionByZeroError", "modulo by zero")),
            JIT_ERR_SHIFT => return Err(self.throw("ArithmeticError", "bit shift by a negative number")),
            JIT_ERR_DEPTH => return Err(VmError::Fatal("maximum call stack depth exceeded".into())),
            JIT_ERR_MEMORY => {
                return Err(VmError::Fatal(format!(
                    "allowed memory size of {} MB exhausted",
                    self.limits.max_memory.unwrap_or(0) / (1024 * 1024)
                )))
            }
            _ => {
                let secs = self.limits.max_time.map(|t| t.as_secs_f64()).unwrap_or(0.0);
                return Err(VmError::Fatal(format!("maximum execution time of {secs} seconds exceeded")));
            }
        }
        Ok(match self.prog.funcs[func as usize].native.as_ref().map(|s| s.ret) {
            Some(ValKind::Int) => Value::Int(r),
            Some(ValKind::Bool) => Value::Bool(r != 0),
            _ => Value::Null,
        })
    }

    /// Fails before an allocation of `bytes` that would exceed the memory limit
    /// (or 4 GB without one). Builtins call this before building large values,
    /// because a failed allocation would abort the whole process.
    pub fn reserve(&self, bytes: usize) -> Result<(), VmError> {
        let limit = self.limits.max_memory.unwrap_or(4 << 30);
        let used = (crate::alloc::thread_allocated() - self.mem_base).max(0) as usize;
        if bytes > limit || used.saturating_add(bytes) > limit {
            return Err(VmError::Fatal(format!("allowed memory size of {} MB exhausted", limit / (1024 * 1024))));
        }
        Ok(())
    }

    /// The time limit as an instant (for code outside the interpreter loop, e.g. SQLite).
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
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
        if let Some(max) = self.limits.max_memory {
            let used = crate::alloc::thread_allocated() - self.mem_base;
            if used > max as isize {
                return Err(VmError::Fatal(format!("allowed memory size of {} MB exhausted", max / (1024 * 1024))));
            }
        }
        Ok(())
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
                        self.regs.resize(need, Value::Null);
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
                        reg!(*d) = Value::Bool(v);
                    }
                    Instr::Ne(d, a, b) => {
                        let v = !values_equal(&reg!(*a), &reg!(*b));
                        reg!(*d) = Value::Bool(v);
                    }
                    Instr::Lt(d, a, b) => {
                        let v = compare(&reg!(*a), &reg!(*b)).is_lt();
                        reg!(*d) = Value::Bool(v);
                    }
                    Instr::Le(d, a, b) => {
                        let v = compare(&reg!(*a), &reg!(*b)).is_le();
                        reg!(*d) = Value::Bool(v);
                    }
                    Instr::Cmp(d, a, b) => {
                        let v = compare(&reg!(*a), &reg!(*b)) as i64;
                        reg!(*d) = Value::Int(v);
                    }

                    Instr::IntToFloat(d, a) => reg!(*d) = Value::Float(int!(*a) as f64),
                    Instr::FloatToInt(d, a) => {
                        let f = float!(*a);
                        if !(i64::MIN as f64..-(i64::MIN as f64)).contains(&f) {
                            tri!(Err(self.throw("ValueError", format!("cannot convert {} to int", format_float(f)))));
                        }
                        reg!(*d) = Value::Int(f.trunc() as i64);
                    }
                    Instr::ToStr(d, a) => {
                        let v = match &reg!(*a) {
                            Value::Str(s) => Value::Str(s.clone()),
                            other => Value::str(other.to_display_string()),
                        };
                        reg!(*d) = v;
                    }
                    Instr::Concat(d, start, n, html) => {
                        let mut size = 0usize;
                        for i in 0..*n {
                            size += match &self.regs[base + (*start + i) as usize] {
                                Value::Str(s) | Value::Html(s) => s.len(),
                                _ => 24,
                            };
                        }
                        if size > 1 << 20 {
                            tri!(self.reserve(size));
                        }
                        let mut s = String::with_capacity(size);
                        for i in 0..*n {
                            match &self.regs[base + (*start + i) as usize] {
                                // An `Html` result never contains unescaped plain strings.
                                Value::Str(x) if *html => html_escape_into(x, &mut s),
                                v => v.append_to(&mut s),
                            }
                        }
                        let big = s.len() > 1 << 20;
                        reg!(*d) = if *html { Value::html(s) } else { Value::str(s) };
                        if big {
                            tri!(self.check_memory());
                        }
                    }
                    Instr::AppendStr(d, s) => {
                        let src = reg!(*s).clone();
                        let grow = match &src {
                            Value::Str(x) | Value::Html(x) => x.len(),
                            _ => 24,
                        };
                        let cur = match &reg!(*d) {
                            Value::Str(x) => x.capacity(),
                            _ => 0,
                        };
                        if grow > 1 << 20 || (cur > 1 << 20 && cur < grow + cur) {
                            // The string may double its buffer.
                            tri!(self.reserve(grow.max(cur) * 2));
                        }
                        let len = match &mut reg!(*d) {
                            Value::Str(rc) => {
                                let s = Rc::make_mut(rc);
                                src.append_to(s);
                                s.len()
                            }
                            v => bad("string", v),
                        };
                        if len > 1 << 20 && len.is_power_of_two() {
                            tri!(self.check_memory());
                        }
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
                        let entry = self.jit.as_ref().and_then(|j| j.entries.get(*callee as usize).copied().flatten());
                        if let Some(entry) = entry {
                            let start = base + *args as usize;
                            let argv: Vec<Value> = self.regs[start..start + *argc as usize].to_vec();
                            self.frames[fi].pc = pc as u32;
                            let v = tri!(self.call_jit(entry, *callee, &argv));
                            reg!(*dst) = v;
                            continue;
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
                        reg!(*d) = Value::Object(Rc::new(RefCell::new(Object { class: *class, fields })));
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

                    Instr::Echo(r) => {
                        let v = reg!(*r).clone();
                        self.echo(&v);
                    }
                    Instr::EchoHtml(c) => {
                        if let Const::Str(s) = &prog.consts[*c as usize] {
                            self.out.last_mut().unwrap().buf.push_str(s);
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
