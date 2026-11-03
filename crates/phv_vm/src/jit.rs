//! The interface between the interpreter and natively compiled code (built by
//! `phv_jit`): the context native code runs with, and the Rust helpers it
//! calls for everything that needs the runtime.
//!
//! Numbers and booleans live in machine registers. A native function that
//! uses strings also gets a window of `Value` slots, one per bytecode
//! register, owned by `JitRt`; the helpers below work on that window with the
//! same code the interpreter runs, so results, memory checks and error
//! messages are identical.

use crate::bytecode::Program;
use crate::value::{compare, Value};
use crate::vm::{const_value, op_append, op_concat, op_to_str, MemLimit, BUDGET};
use std::sync::Arc;
use std::time::Instant;

/// Shared state between the VM and natively compiled code (layout is fixed:
/// the JIT reads and writes these fields by offset).
#[repr(C)]
pub struct JitCtx {
    /// Set by native code: 0 = ok, see `JIT_ERR_*`.
    pub error: i64,
    pub depth: i64,
    pub max_depth: i64,
    /// Decremented at loop back-edges and calls; at zero `phv_jit_tick` runs.
    pub budget: i64,
    pub limits: *const JitLimits,
    /// More about the error: the bits of the float `JIT_ERR_FLOAT_TO_INT` could not convert.
    pub detail: i64,
    pub rt: *mut JitRt,
}

pub const JIT_ERR_OVERFLOW: i64 = 1;
/// `%` by zero (int or float).
pub const JIT_ERR_DIV_ZERO: i64 = 2;
pub const JIT_ERR_DEPTH: i64 = 3;
pub const JIT_ERR_TIME: i64 = 4;
pub const JIT_ERR_MEMORY: i64 = 5;
pub const JIT_ERR_SHIFT: i64 = 6;
/// `/` by zero.
pub const JIT_ERR_FDIV_ZERO: i64 = 7;
pub const JIT_ERR_FLOAT_TO_INT: i64 = 8;

/// Kinds of the scalar `phv_jit_put` stores in a window slot.
pub const JIT_PUT_INT: i64 = 1;
pub const JIT_PUT_FLOAT: i64 = 2;
pub const JIT_PUT_BOOL: i64 = 3;

pub struct JitLimits {
    pub deadline: Option<Instant>,
    pub max_memory: Option<usize>,
    pub mem_base: isize,
}

impl JitLimits {
    fn mem(&self) -> MemLimit {
        MemLimit { max: self.max_memory, base: self.mem_base }
    }
}

/// Entry point of a compiled function: `(ctx, pointer to the i64 arguments) -> result`.
/// Float arguments and results are passed as their bits; string arguments are
/// in `JitRt`'s first window and a string result is left in `JitRt::ret`.
pub type JitEntry = unsafe extern "C" fn(*mut JitCtx, *const i64) -> i64;

/// Native code for the functions the JIT compiled, indexed by function id.
pub struct JitTable {
    pub entries: Vec<Option<JitEntry>>,
    /// Owns the executable memory the entries point into.
    pub keep_alive: Box<dyn std::any::Any + Send + Sync>,
}

/// The `Value`s of a call into native code.
#[derive(Default)]
pub struct JitRt {
    /// Register windows of the active native frames that use strings, from
    /// the outermost; `windows[0]` holds the arguments from the interpreter.
    /// Windows above `used` are empty and kept for reuse.
    windows: Vec<Vec<Value>>,
    used: usize,
    /// The string a native function returned.
    ret: Value,
    /// The VM's constant cache, lent for the call.
    pub(crate) consts: Vec<Option<Value>>,
    pub(crate) prog: Option<Arc<Program>>,
}

impl JitRt {
    /// Prepares a call from the interpreter with these arguments.
    pub(crate) fn start(&mut self, args: Vec<Value>) {
        match self.windows.first_mut() {
            Some(w) => *w = args,
            None => self.windows.push(args),
        }
        self.used = 1;
    }

    /// After the call: drops what is left in the windows (all of them when
    /// native code stopped with an error) and returns the string result.
    pub(crate) fn finish(&mut self) -> Value {
        for w in &mut self.windows[..self.used] {
            w.clear();
        }
        self.used = 0;
        std::mem::take(&mut self.ret)
    }

    fn top(&mut self) -> &mut [Value] {
        &mut self.windows[self.used - 1]
    }
}

/// Called by native code when its budget runs out: checks the time and memory limits.
///
/// # Safety
/// `ctx` must point to a live `JitCtx` whose `limits` points to live `JitLimits`.
pub unsafe extern "C" fn phv_jit_tick(ctx: *mut JitCtx) -> i64 {
    let ctx = &mut *ctx;
    ctx.budget = BUDGET as i64;
    let limits = &*ctx.limits;
    if limits.deadline.is_some_and(|d| Instant::now() > d) {
        return JIT_ERR_TIME;
    }
    if limits.mem().check().is_err() {
        return JIT_ERR_MEMORY;
    }
    0
}

// The helpers below take register numbers as i64. The JIT only passes numbers
// it checked against the function's register count, which is the size of its
// window; an out-of-range number would panic (and abort), never touch other memory.

/// The `JitRt` of the running call.
///
/// # Safety
/// `ctx` must be the context `Vm::call_jit` gave native code: `rt` points to a
/// live `JitRt` that nothing else uses until native code returns.
unsafe fn rt<'a>(ctx: *mut JitCtx) -> &'a mut JitRt {
    &mut *(*ctx).rt
}

/// The memory limit of the running call.
///
/// # Safety
/// As for `rt`: `limits` points to live `JitLimits`.
unsafe fn mem(ctx: *mut JitCtx) -> MemLimit {
    (*(*ctx).limits).mem()
}

/// Enters a native function that keeps strings: pushes a window of `nregs`
/// slots and moves string argument `k` (bit `k` of `mask`) from slot `args + k`
/// of the caller's window, as the interpreter moves arguments.
///
/// # Safety
/// See `rt`.
pub unsafe extern "C" fn phv_jit_enter(ctx: *mut JitCtx, nregs: i64, args: i64, mask: i64) {
    let rt = rt(ctx);
    let i = rt.used;
    if rt.windows.len() == i {
        rt.windows.push(Vec::new());
    }
    let mut w = std::mem::take(&mut rt.windows[i]);
    w.resize(nregs as usize, Value::Null);
    let caller = &mut rt.windows[i - 1];
    for k in 0..16 {
        if mask >> k & 1 != 0 {
            w[k] = std::mem::take(&mut caller[args as usize + k]);
        }
    }
    rt.windows[i] = w;
    rt.used = i + 1;
}

/// Leaves a native function that keeps strings, returning the string in slot
/// `ret` (none when negative).
///
/// # Safety
/// See `rt`.
pub unsafe extern "C" fn phv_jit_leave(ctx: *mut JitCtx, ret: i64) {
    let rt = rt(ctx);
    let w = &mut rt.windows[rt.used - 1];
    if ret >= 0 {
        rt.ret = std::mem::take(&mut w[ret as usize]);
    }
    w.clear();
    rt.used -= 1;
}

/// Moves the string the last call returned into slot `d`.
///
/// # Safety
/// See `rt`.
pub unsafe extern "C" fn phv_jit_take_ret(ctx: *mut JitCtx, d: i64) {
    let rt = rt(ctx);
    let v = std::mem::take(&mut rt.ret);
    rt.top()[d as usize] = v;
}

/// `Const`: constant `c` (a string) into slot `d`.
///
/// # Safety
/// See `rt`.
pub unsafe extern "C" fn phv_jit_const(ctx: *mut JitCtx, d: i64, c: i64) {
    let rt = rt(ctx);
    let c = c as usize;
    let v = match &rt.consts[c] {
        Some(v) => v.clone(),
        None => {
            let v = const_value(&rt.prog.as_ref().expect("JitRt without a program").consts[c]);
            rt.consts[c] = Some(v.clone());
            v
        }
    };
    rt.top()[d as usize] = v;
}

/// `Move` of a string.
///
/// # Safety
/// See `rt`.
pub unsafe extern "C" fn phv_jit_move(ctx: *mut JitCtx, d: i64, s: i64) {
    let w = rt(ctx).top();
    w[d as usize] = w[s as usize].clone();
}

/// Stores a number or boolean (`JIT_PUT_*`) in slot `r`, so a string
/// operation can read it from there like the interpreter does.
///
/// # Safety
/// See `rt`.
pub unsafe extern "C" fn phv_jit_put(ctx: *mut JitCtx, r: i64, kind: i64, bits: i64) {
    rt(ctx).top()[r as usize] = match kind {
        JIT_PUT_INT => Value::Int(bits),
        JIT_PUT_FLOAT => Value::Float(f64::from_bits(bits as u64)),
        _ => Value::Bool(bits != 0),
    };
}

/// `ToStr`.
///
/// # Safety
/// See `rt`.
pub unsafe extern "C" fn phv_jit_to_str(ctx: *mut JitCtx, d: i64, a: i64) {
    op_to_str(rt(ctx).top(), d as u16, a as u16);
}

/// `Concat` (plain strings); returns an error code.
///
/// # Safety
/// See `rt` and `mem`.
pub unsafe extern "C" fn phv_jit_concat(ctx: *mut JitCtx, d: i64, start: i64, n: i64) -> i64 {
    let mem = mem(ctx);
    match op_concat(rt(ctx).top(), mem, d as u16, start as u16, n as u16, false) {
        Ok(()) => 0,
        Err(_) => JIT_ERR_MEMORY,
    }
}

/// `AppendStr`; returns an error code.
///
/// # Safety
/// See `rt` and `mem`.
pub unsafe extern "C" fn phv_jit_append(ctx: *mut JitCtx, d: i64, s: i64) -> i64 {
    let mem = mem(ctx);
    match op_append(rt(ctx).top(), mem, d as u16, s as u16) {
        Ok(()) => 0,
        Err(_) => JIT_ERR_MEMORY,
    }
}

/// The `strlen` builtin (bytes) or, when `chars` is set, `str_len`
/// (`->len()`, characters) of slot `r`; or minus an error code (the
/// interpreter checks the limits after every builtin).
///
/// # Safety
/// See `rt` and `mem`.
pub unsafe extern "C" fn phv_jit_strlen(ctx: *mut JitCtx, r: i64, chars: i64) -> i64 {
    let n = match &rt(ctx).top()[r as usize] {
        Value::Str(s) | Value::Html(s) if chars != 0 => s.chars().count() as i64,
        Value::Str(s) | Value::Html(s) => s.len() as i64,
        _ => 0,
    };
    if mem(ctx).check().is_err() {
        return -JIT_ERR_MEMORY;
    }
    if (*(*ctx).limits).deadline.is_some_and(|d| Instant::now() > d) {
        return -JIT_ERR_TIME;
    }
    n
}

/// Compares two strings: -1, 0 or 1 (`==` on strings is `== 0`).
///
/// # Safety
/// See `rt`.
pub unsafe extern "C" fn phv_jit_str_cmp(ctx: *mut JitCtx, a: i64, b: i64) -> i64 {
    let w = rt(ctx).top();
    match (&w[a as usize], &w[b as usize]) {
        (Value::Str(x), Value::Str(y)) => x.as_str().cmp(y.as_str()) as i64,
        (x, y) => compare(x, y) as i64,
    }
}

/// `%` on floats, exactly as the interpreter computes it.
pub extern "C" fn phv_jit_fmod(x: f64, y: f64) -> f64 {
    x % y
}

/// `**` on floats, exactly as the interpreter computes it.
pub extern "C" fn phv_jit_pow(x: f64, y: f64) -> f64 {
    x.powf(y)
}
