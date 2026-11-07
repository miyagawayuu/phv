//! The interface between the interpreter and natively compiled code (built by
//! `phv_jit`): the context native code runs with, and the Rust helpers it
//! calls for everything that needs the runtime.
//!
//! Numbers and booleans live in machine registers. A native function that
//! uses strings or lists also gets a window of `Value` slots, one per bytecode
//! register, owned by `JitRt`; the helpers below work on that window with the
//! same code the interpreter runs, so results, memory checks and error
//! messages are identical.

use crate::bytecode::Program;
use crate::value::{compare, Value};
use crate::vm::{
    const_value, copy_cost, key_repr, op_append, op_concat, op_to_str, path_append, path_assign, str_len, MemLimit,
    PathError, BUDGET, LONG_OP,
};
use std::rc::Rc;
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
    /// The lowest stack address native code may use: a function entered below
    /// it stops with `JIT_ERR_STACK` (depth alone does not bound large frames).
    pub stack_limit: i64,
    /// The program's constant index of each constant the running unit of
    /// native code names (see `Installed::consts`): compiled code is shared
    /// between programs, so it names constants by their index in the unit.
    pub consts: *const u32,
}

/// Native stack the JIT's code may use below the outermost native call.
/// Scripts run on threads with more (16 MB for server workers, 64 MB for the
/// CLI); where the thread has less left, native code stops `STACK_MARGIN`
/// above its end (see `stack.rs`).
pub const JIT_STACK: usize = 8 << 20;

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
/// Native code ran out of its stack (`JitCtx::stack_limit`): the call is run
/// again by the interpreter, whose frames are not on the native stack.
pub const JIT_ERR_STACK: i64 = 9;
/// A helper raised an exception (`JitRt::pending`), as the interpreter would.
pub const JIT_ERR_THROW: i64 = 10;

/// Kinds of the scalar `phv_jit_put` stores in a window slot.
pub const JIT_PUT_INT: i64 = 1;
pub const JIT_PUT_FLOAT: i64 = 2;
pub const JIT_PUT_BOOL: i64 = 3;
/// `null` (a nullable value that is null).
pub const JIT_PUT_NULL: i64 = 4;

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

/// A function's native code, once it is compiled (see `JitTable::entry`).
pub struct Installed {
    pub entry: JitEntry,
    /// For `JitCtx::consts`: the program's index of each constant the unit
    /// of code `entry` belongs to names (the unit's code is shared between
    /// programs, and names constants by their index in the unit).
    pub consts: Arc<[u32]>,
    /// Owns the executable memory `entry` points into: the unit lives at
    /// least as long as this.
    pub keep_alive: Arc<dyn std::any::Any + Send + Sync>,
}

/// Native code for the functions the JIT compiled, by function id. One table
/// per compiled program, filled when the program is loaded (by `phv_jit`, on
/// the thread that loads it) and then only read, by every run of the program
/// on any thread: the units' memory is made executable, with the instruction
/// cache and pipeline flushed, before the table is published with the
/// program, so what a function runs as, interpreted or compiled, is fixed
/// from the first call.
pub struct JitTable {
    /// Per function: its code, when it is compiled.
    entries: Vec<Option<Installed>>,
    /// Per function: it changes a string, list or map parameter in place
    /// (`$xs[] = ...`, `.=`). Its arguments are not kept for a rerun (an
    /// extra reference would make that change copy the whole value), so when
    /// its native code runs out of stack it stops instead.
    pub writes_args: Vec<bool>,
}

impl JitTable {
    pub fn new(entries: Vec<Option<Installed>>, writes_args: Vec<bool>) -> JitTable {
        JitTable { entries, writes_args }
    }

    /// The native code of function `f`, if it has any.
    #[inline]
    pub fn entry(&self, f: u32) -> Option<&Installed> {
        self.entries.get(f as usize)?.as_ref()
    }

    /// Whether function `f` has native code.
    pub fn is_compiled(&self, f: u32) -> bool {
        self.entry(f).is_some()
    }

    /// The number of functions with native code.
    pub fn compiled(&self) -> usize {
        self.entries.iter().filter(|e| e.is_some()).count()
    }
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
    /// The exception a helper raised (`JIT_ERR_THROW`): its class and message.
    pending: Option<(&'static str, String)>,
}

impl JitRt {
    /// Prepares a call from the interpreter with these arguments, moved out
    /// of the interpreter's registers into the first window (whose memory is
    /// reused from call to call).
    pub(crate) fn start(&mut self, args: &mut [Value]) {
        if self.windows.is_empty() {
            self.windows.push(Vec::new());
        }
        let w = &mut self.windows[0];
        w.clear();
        w.extend(args.iter_mut().map(std::mem::take));
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

    /// The exception of `JIT_ERR_THROW`.
    pub(crate) fn take_pending(&mut self) -> (&'static str, String) {
        self.pending.take().unwrap_or(("RuntimeException", "internal error in native code".into()))
    }
}

/// Makes native code stop with exception `class`.
///
/// # Safety
/// See `rt`.
unsafe fn throw(ctx: *mut JitCtx, class: &'static str, msg: String) {
    rt(ctx).pending = Some((class, msg));
    (*ctx).error = JIT_ERR_THROW;
}

/// A slot did not hold the value the checked bytecode promised (only bytecode
/// that is not what the compiler makes could do that).
///
/// # Safety
/// See `rt`.
unsafe fn wrong_value(ctx: *mut JitCtx) {
    throw(ctx, "RuntimeException", "internal error: a native list operation found another kind of value".into());
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
/// of the caller's window, as the interpreter moves arguments. Returns an
/// error code: the memory limit is checked before a window is allocated.
///
/// # Safety
/// See `rt` and `mem`.
pub unsafe extern "C" fn phv_jit_enter(ctx: *mut JitCtx, nregs: i64, args: i64, mask: i64) -> i64 {
    let mem = mem(ctx);
    let rt = rt(ctx);
    let i = rt.used;
    if rt.windows.len() == i {
        rt.windows.push(Vec::new());
    }
    let mut w = std::mem::take(&mut rt.windows[i]);
    if w.capacity() < nregs as usize && mem.reserve(nregs as usize * std::mem::size_of::<Value>()).is_err() {
        rt.windows[i] = w;
        return JIT_ERR_MEMORY;
    }
    w.resize(nregs as usize, Value::Null);
    let caller = &mut rt.windows[i - 1];
    for k in 0..16 {
        if mask >> k & 1 != 0 {
            w[k] = std::mem::take(&mut caller[args as usize + k]);
        }
    }
    rt.windows[i] = w;
    rt.used = i + 1;
    0
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

/// `Const`: constant `c` of the unit (a string, or an empty list or map) into
/// slot `d`.
///
/// # Safety
/// See `rt`; `ctx.consts` has an entry for every constant the unit names.
pub unsafe extern "C" fn phv_jit_const(ctx: *mut JitCtx, d: i64, c: i64) {
    let c = *(*ctx).consts.add(c as usize) as usize;
    let rt = rt(ctx);
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
        JIT_PUT_NULL => Value::Null,
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
    let w = rt(ctx).top();
    match op_concat(w, mem, d as u16, start as u16, n as u16, false) {
        Ok(()) => {
            long_op(ctx, str_len(&w[d as usize]));
            0
        }
        Err(_) => JIT_ERR_MEMORY,
    }
}

/// As `Vm::long_op`: after a long string operation, the next loop step or
/// call checks the limits.
///
/// # Safety
/// See `rt`.
unsafe fn long_op(ctx: *mut JitCtx, bytes: usize) {
    if bytes >= LONG_OP {
        (*ctx).budget = 1;
    }
}

/// After hashing a map key of `bytes`: a long one is timed at once (one
/// loop step may look it up many times), as in the interpreter.
///
/// # Safety
/// See `rt`.
unsafe fn long_key(ctx: *mut JitCtx, bytes: usize) {
    if bytes >= LONG_OP && (*(*ctx).limits).deadline.is_some_and(|d| Instant::now() > d) && (*ctx).error == 0 {
        (*ctx).error = JIT_ERR_TIME;
    }
}

/// `AppendStr`; returns an error code.
///
/// # Safety
/// See `rt` and `mem`.
pub unsafe extern "C" fn phv_jit_append(ctx: *mut JitCtx, d: i64, s: i64) -> i64 {
    let mem = mem(ctx);
    let w = rt(ctx).top();
    match op_append(w, mem, d as u16, s as u16) {
        Ok(work) => {
            long_op(ctx, work);
            0
        }
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
    // `strlen` takes constant time: like the interpreter (`CHEAP_BUILTINS`),
    // the clock is read at the next loop step or call instead.
    if chars != 0 && (*(*ctx).limits).deadline.is_some_and(|d| Instant::now() > d) {
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
    let r = match (&w[a as usize], &w[b as usize]) {
        (Value::Str(x), Value::Str(y)) => x.as_str().cmp(y.as_str()) as i64,
        (x, y) => compare(x, y) as i64,
    };
    long_op(ctx, str_len(&w[a as usize]));
    r
}

/// `%` on floats, exactly as the interpreter computes it.
pub extern "C" fn phv_jit_fmod(x: f64, y: f64) -> f64 {
    x % y
}

/// `**` on floats, exactly as the interpreter computes it.
pub extern "C" fn phv_jit_pow(x: f64, y: f64) -> f64 {
    x.powf(y)
}

// Lists of numbers, strings or booleans: the interpreter's code on the window.
// Errors are left in `ctx.error`, which native code checks after each call.

/// Element kinds `phv_jit_index` returns: in a machine register as an i64
/// (ints and bools) or as the bits of a float, or in slot `d` (strings).
pub const JIT_ELEM_INT: i64 = 0;
pub const JIT_ELEM_FLOAT: i64 = 1;
pub const JIT_ELEM_STR: i64 = 2;

/// `NewList`: the `n` slots from `start` (moved out) as a list in slot `d`.
///
/// # Safety
/// See `rt`.
pub unsafe extern "C" fn phv_jit_new_list(ctx: *mut JitCtx, d: i64, start: i64, n: i64) {
    let w = rt(ctx).top();
    let items: Vec<Value> = (0..n as usize).map(|i| std::mem::take(&mut w[start as usize + i])).collect();
    w[d as usize] = Value::list(items);
}

/// `Index` of the list in slot `l` at `i`: the element (see `JIT_ELEM_*`).
///
/// # Safety
/// See `rt`.
pub unsafe extern "C" fn phv_jit_index(ctx: *mut JitCtx, d: i64, l: i64, i: i64, kind: i64) -> i64 {
    let w = rt(ctx).top();
    let Value::List(list) = &w[l as usize] else {
        wrong_value(ctx);
        return 0;
    };
    let item = match usize::try_from(i).ok().and_then(|x| list.get(x)) {
        Some(v) => v.clone(),
        None => {
            let msg = format!("list index {i} is out of range (length {})", list.len());
            throw(ctx, "IndexError", msg);
            return 0;
        }
    };
    match (kind, item) {
        (JIT_ELEM_INT, Value::Int(n)) => n,
        (JIT_ELEM_INT, Value::Bool(b)) => b as i64,
        (JIT_ELEM_FLOAT, Value::Float(f)) => f.to_bits() as i64,
        (JIT_ELEM_STR, v @ Value::Str(_)) => {
            w[d as usize] = v;
            0
        }
        _ => {
            wrong_value(ctx);
            0
        }
    }
}

/// The length of the list or map in slot `l`.
///
/// # Safety
/// See `rt`.
pub unsafe extern "C" fn phv_jit_list_len(ctx: *mut JitCtx, l: i64) -> i64 {
    match &rt(ctx).top()[l as usize] {
        Value::List(x) => x.len() as i64,
        Value::Map(m) => m.len() as i64,
        _ => {
            wrong_value(ctx);
            0
        }
    }
}

/// `$xs[$i] = $v` (or `$xs[] = $v` when `push` is set) on the list in slot
/// `r`, with the value in slot `val`: copied first when shared, as the
/// interpreter does, after the copy is accounted.
///
/// # Safety
/// See `rt` and `mem`.
pub unsafe extern "C" fn phv_jit_set_index(ctx: *mut JitCtx, r: i64, i: i64, push: i64, val: i64) {
    let mem = mem(ctx);
    let w = rt(ctx).top();
    if !matches!(w[r as usize], Value::List(_)) {
        wrong_value(ctx);
        return;
    }
    let v = w[val as usize].clone();
    let push = push != 0;
    let key = [Value::Int(i)];
    let keys: &[Value] = if push { &[] } else { &key };
    let cost = copy_cost(&w[r as usize], keys, push);
    if cost > 0 {
        if mem.reserve(cost).is_err() {
            (*ctx).error = JIT_ERR_MEMORY;
            return;
        }
        long_op(ctx, cost);
    }
    if let Err((class, msg)) = path_assign(&mut w[r as usize], keys, &[], push, v) {
        throw(ctx, class, msg);
    }
}

/// `Range`: the ints from `from` to `to` (included when `inclusive` is set) in slot `d`.
///
/// # Safety
/// See `rt` and `mem`.
pub unsafe extern "C" fn phv_jit_range(ctx: *mut JitCtx, d: i64, from: i64, to: i64, inclusive: i64) {
    let to = if inclusive != 0 { to.saturating_add(1) } else { to };
    let count = to.saturating_sub(from).max(0);
    if count > 10_000_000 {
        throw(ctx, "ValueError", "range is too large (more than 10,000,000 items)".into());
        return;
    }
    if mem(ctx).reserve(count as usize * std::mem::size_of::<Value>()).is_err() {
        (*ctx).error = JIT_ERR_MEMORY;
        return;
    }
    rt(ctx).top()[d as usize] = Value::list((from..to).map(Value::Int).collect());
    // Up to 10,000,000 items in one step (found by the 1.0.0 audit).
    long_op(ctx, count as usize * std::mem::size_of::<Value>());
}

/// `**` on ints, as the interpreter computes it.
///
/// # Safety
/// See `rt`.
pub unsafe extern "C" fn phv_jit_pow_i(ctx: *mut JitCtx, x: i64, y: i64) -> i64 {
    if y < 0 {
        let msg = "negative exponent in int ** int; use a float base, e.g. 2.0 ** -1";
        throw(ctx, "ArithmeticError", msg.into());
        return 0;
    }
    match u32::try_from(y).ok().and_then(|y| x.checked_pow(y)) {
        Some(v) => v,
        None => {
            (*ctx).error = JIT_ERR_OVERFLOW;
            0
        }
    }
}

/// The `range($from, $to)` builtin into slot `d`.
///
/// # Safety
/// See `rt` and `mem`.
pub unsafe extern "C" fn phv_jit_range_fn(ctx: *mut JitCtx, d: i64, from: i64, to: i64) {
    let count = crate::value::range_len(from, to);
    if count > crate::value::MAX_RANGE {
        throw(ctx, "ValueError", "range is too large (more than 10,000,000 items)".into());
        return;
    }
    if mem(ctx).reserve(count as usize * std::mem::size_of::<Value>()).is_err() {
        (*ctx).error = JIT_ERR_MEMORY;
        return;
    }
    rt(ctx).top()[d as usize] = Value::list(crate::value::range_items(from, to));
    // The interpreter checks the time after this builtin (found by the 1.0.0 audit).
    long_op(ctx, count as usize * std::mem::size_of::<Value>());
}

/// `$xs[] = $v` with a number or boolean (`JIT_PUT_*`) on the list in slot
/// `r`: one call instead of storing the value in the window first.
///
/// # Safety
/// See `rt` and `mem`.
pub unsafe extern "C" fn phv_jit_push(ctx: *mut JitCtx, r: i64, kind: i64, bits: i64) {
    let mem = mem(ctx);
    let w = rt(ctx).top();
    if !matches!(w[r as usize], Value::List(_)) {
        wrong_value(ctx);
        return;
    }
    let v = match kind {
        JIT_PUT_INT => Value::Int(bits),
        JIT_PUT_FLOAT => Value::Float(f64::from_bits(bits as u64)),
        _ => Value::Bool(bits != 0),
    };
    let cost = copy_cost(&w[r as usize], &[], true);
    if cost > 0 {
        if mem.reserve(cost).is_err() {
            (*ctx).error = JIT_ERR_MEMORY;
            return;
        }
        long_op(ctx, cost);
    }
    if let Err((class, msg)) = path_assign(&mut w[r as usize], &[], &[], true, v) {
        throw(ctx, class, msg);
    }
}

// Nullable values and maps.

/// Hands value `v` to native code as an element of kind `kind` (`JIT_ELEM_*`):
/// returned in a machine register, or put in slot `d` (strings). A null is
/// accepted when `nullable` is set, and reported in `ctx.detail` (1 = null).
///
/// # Safety
/// See `rt`.
unsafe fn deliver(ctx: *mut JitCtx, d: i64, kind: i64, v: Value, nullable: bool) -> i64 {
    (*ctx).detail = 0;
    match (kind, v) {
        (_, Value::Null) if nullable => {
            (*ctx).detail = 1;
            if kind == JIT_ELEM_STR {
                rt(ctx).top()[d as usize] = Value::Null;
            }
            0
        }
        (JIT_ELEM_INT, Value::Int(n)) => n,
        (JIT_ELEM_INT, Value::Bool(b)) => b as i64,
        (JIT_ELEM_FLOAT, Value::Float(f)) => f.to_bits() as i64,
        (JIT_ELEM_STR, v @ Value::Str(_)) => {
            rt(ctx).top()[d as usize] = v;
            0
        }
        _ => {
            wrong_value(ctx);
            0
        }
    }
}

/// Whether the value in slot `r` is null.
///
/// # Safety
/// See `rt`.
pub unsafe extern "C" fn phv_jit_is_null(ctx: *mut JitCtx, r: i64) -> i64 {
    matches!(rt(ctx).top()[r as usize], Value::Null) as i64
}

/// The nullable scalar in slot `r` for a machine register (`JIT_ELEM_INT` or
/// `JIT_ELEM_FLOAT`); `ctx.detail` is 1 when it is null.
///
/// # Safety
/// See `rt`.
pub unsafe extern "C" fn phv_jit_unbox(ctx: *mut JitCtx, r: i64, kind: i64) -> i64 {
    let v = rt(ctx).top()[r as usize].clone();
    deliver(ctx, r, kind, v, true)
}

/// `Index` (or, with `or_null`, `IndexOrNull`) of the list or map in slot `c`
/// at the key in slot `k`, into register `d` (see `deliver`).
///
/// # Safety
/// See `rt`.
pub unsafe extern "C" fn phv_jit_get(ctx: *mut JitCtx, d: i64, c: i64, k: i64, kind: i64, or_null: i64) -> i64 {
    let w = rt(ctx).top();
    let found = match (&w[c as usize], &w[k as usize]) {
        (Value::List(l), Value::Int(i)) => match usize::try_from(*i).ok().and_then(|x| l.get(x)) {
            Some(v) => Ok(v.clone()),
            None => Err(("IndexError", format!("list index {i} is out of range (length {})", l.len()))),
        },
        (Value::Map(m), key) => match m.get(&key.to_key()) {
            Some(v) => Ok(v.clone()),
            None => Err(("KeyError", format!("undefined map key {}", key_repr(key)))),
        },
        _ => {
            wrong_value(ctx);
            return 0;
        }
    };
    long_key(ctx, str_len(&w[k as usize]));
    match found {
        Ok(v) => deliver(ctx, d, kind, v, false),
        Err(_) if or_null != 0 => deliver(ctx, d, kind, Value::Null, true),
        Err((class, msg)) => {
            throw(ctx, class, msg);
            0
        }
    }
}

/// `$m[$k] = $v` (or `$xs[$i] = $v`) with the key in slot `k` and the value in slot `val`.
///
/// # Safety
/// See `rt` and `mem`.
pub unsafe extern "C" fn phv_jit_set_key(ctx: *mut JitCtx, r: i64, k: i64, val: i64) {
    let mem = mem(ctx);
    let w = rt(ctx).top();
    if !matches!(w[r as usize], Value::List(_) | Value::Map(_)) {
        wrong_value(ctx);
        return;
    }
    let v = w[val as usize].clone();
    let key = [w[k as usize].clone()];
    long_key(ctx, str_len(&key[0]));
    let cost = copy_cost(&w[r as usize], &key, false);
    if cost > 0 {
        if mem.reserve(cost).is_err() {
            (*ctx).error = JIT_ERR_MEMORY;
            return;
        }
        long_op(ctx, cost);
    }
    if let Err((class, msg)) = path_assign(&mut w[r as usize], &key, &[], false, v) {
        throw(ctx, class, msg);
    }
}

/// `AppendPath` on the list or map in slot `r` with the key in slot `k`:
/// `cur` holds the element read before the string in slot `val` was made
/// (see `path_append`). Copies are accounted first, as for `phv_jit_set_key`.
///
/// # Safety
/// See `rt` and `mem`.
pub unsafe extern "C" fn phv_jit_append_key(ctx: *mut JitCtx, r: i64, k: i64, cur: i64, val: i64) {
    let mem = mem(ctx);
    let w = rt(ctx).top();
    if !matches!(w[r as usize], Value::List(_) | Value::Map(_)) {
        wrong_value(ctx);
        return;
    }
    let key = [w[k as usize].clone()];
    long_key(ctx, str_len(&key[0]));
    let cost = copy_cost(&w[r as usize], &key, false);
    if cost > 0 {
        if mem.reserve(cost).is_err() {
            (*ctx).error = JIT_ERR_MEMORY;
            return;
        }
        long_op(ctx, cost);
    }
    let cur = std::mem::take(&mut w[cur as usize]);
    let src = w[val as usize].clone();
    match path_append(&mut w[r as usize], &key, &[], cur, &src, mem) {
        Ok(work) => long_op(ctx, work),
        Err(PathError::Memory(_)) => (*ctx).error = JIT_ERR_MEMORY,
        Err(PathError::Throw(class, msg)) => throw(ctx, class, msg),
    }
}

/// `==` of the values in slots `a` and `b` (strings), as 0 or 1.
///
/// # Safety
/// See `rt`.
pub unsafe extern "C" fn phv_jit_str_eq(ctx: *mut JitCtx, a: i64, b: i64) -> i64 {
    let w = rt(ctx).top();
    let r = match (&w[a as usize], &w[b as usize]) {
        (Value::Str(x), Value::Str(y)) => Rc::ptr_eq(x, y) || x == y,
        // Only strings (and nullable ones) reach here; a list or map would not
        // be compared without the limits of `Vm::equal`.
        (x, y) => crate::value::flat_equal(x, y).unwrap_or(false),
    };
    long_op(ctx, str_len(&w[a as usize]));
    r as i64
}

/// Entry `i` of the map in slot `m` (a `foreach` step): its key (`part` 0) or
/// its value (1), into register `d` (see `deliver`).
///
/// # Safety
/// See `rt`.
pub unsafe extern "C" fn phv_jit_entry(ctx: *mut JitCtx, d: i64, m: i64, i: i64, part: i64, kind: i64) -> i64 {
    let w = rt(ctx).top();
    let Value::Map(map) = &w[m as usize] else {
        wrong_value(ctx);
        return 0;
    };
    let Some((k, v)) = usize::try_from(i).ok().and_then(|x| map.get_index(x)) else {
        wrong_value(ctx);
        return 0;
    };
    let item = if part == 0 { k.to_value() } else { v.clone() };
    deliver(ctx, d, kind, item, false)
}

/// `$m->has($k)` on the map in slot `m` with the key in slot `k`.
///
/// # Safety
/// See `rt`.
pub unsafe extern "C" fn phv_jit_has(ctx: *mut JitCtx, m: i64, k: i64) -> i64 {
    let w = rt(ctx).top();
    match &w[m as usize] {
        Value::Map(map) => {
            let found = map.contains_key(&w[k as usize].to_key()) as i64;
            long_key(ctx, str_len(&w[k as usize]));
            found
        }
        _ => {
            wrong_value(ctx);
            0
        }
    }
}

/// `NewMap`: `n` keys and values from slot `start` (moved out), as a map in slot `d`.
///
/// # Safety
/// See `rt`.
pub unsafe extern "C" fn phv_jit_new_map(ctx: *mut JitCtx, d: i64, start: i64, n: i64) {
    let w = rt(ctx).top();
    let mut m = indexmap::IndexMap::with_capacity(n as usize);
    for i in 0..n as usize {
        let k = std::mem::take(&mut w[start as usize + 2 * i]);
        let v = std::mem::take(&mut w[start as usize + 2 * i + 1]);
        // Each long key is hashed: timed as in the interpreter, and the literal
        // stops once the time is up (found by the 1.0.0 audit).
        long_key(ctx, crate::vm::str_len(&k));
        if (*ctx).error != 0 {
            return;
        }
        m.insert(k.to_key(), v);
    }
    w[d as usize] = Value::map(m);
}
