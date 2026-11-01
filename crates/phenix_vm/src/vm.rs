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

/// Request data and response state for scripts running under the web server.
pub struct HttpCtx {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub form: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub body: String,
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
}

impl Default for Limits {
    fn default() -> Self {
        Limits { max_time: None, max_frames: 10_000 }
    }
}

const MAX_NATIVE_DEPTH: u32 = 200;
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
    limits: Limits,
    deadline: Option<Instant>,
    budget: u32,
    pub rng: u64,
    native_depth: u32,
    argbuf: Vec<Value>,
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
            limits,
            deadline: limits.max_time.map(|d| Instant::now() + d),
            budget: BUDGET,
            rng: seed,
            native_depth: 0,
            argbuf: Vec::new(),
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

    #[cold]
    fn check_deadline(&mut self) -> Result<(), VmError> {
        {
            self.budget = BUDGET;
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
            PathBase::Local(r) => path_assign(&mut self.regs[base + r as usize], &keys, p.push, val),
            PathBase::Prop(o, slot) => {
                let obj = match &self.regs[base + o as usize] {
                    Value::Object(o) => o.clone(),
                    v => bad("object", v),
                };
                let mut ob = obj.borrow_mut();
                path_assign(&mut ob.fields[slot as usize], &keys, p.push, val)
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
                    let need = new_base + (prog.funcs[callee as usize].num_regs as usize).max(caps.len() + $argc as usize);
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
                        if !f.is_finite() || f >= 9.223372036854775807e18 || f < -9.223372036854775808e18 {
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
                        let mut s = String::new();
                        for i in 0..*n {
                            self.regs[base + (*start + i) as usize].append_to(&mut s);
                        }
                        reg!(*d) = if *html { Value::html(s) } else { Value::str(s) };
                    }
                    Instr::AppendStr(d, s) => {
                        let src = reg!(*s).clone();
                        match &mut reg!(*d) {
                            Value::Str(rc) => src.append_to(Rc::make_mut(rc)),
                            v => bad("string", v),
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

                    Instr::Call { dst, func: callee, args, argc } => call!(*callee, *dst, *args, *argc, &[]),
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
                    }
                    Instr::Ret(r) => ret!(std::mem::take(&mut reg!(*r))),
                    Instr::RetNull => ret!(Value::Null),

                    Instr::NewObject(d, class) => {
                        let fields = prog.classes[*class as usize].fields.iter().map(|f| const_value(&f.default)).collect();
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
                        let items: Vec<Value> =
                            (0..*n as usize).map(|i| std::mem::take(&mut self.regs[base + *start as usize + i])).collect();
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

fn path_assign(target: &mut Value, keys: &[Value], push: bool, val: Value) -> Result<(), (&'static str, String)> {
    let (inner, last) = if push { (keys, None) } else { (&keys[..keys.len() - 1], keys.last()) };
    let mut cur = target;
    for k in inner {
        cur = index_mut(cur, k)?;
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

fn index_mut<'v>(v: &'v mut Value, k: &Value) -> Result<&'v mut Value, (&'static str, String)> {
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
        Value::Map(m) => match Rc::make_mut(m).get_mut(&k.to_key()) {
            Some(v) => Ok(v),
            None => Err(("KeyError", format!("undefined map key {}", key_repr(k)))),
        },
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
