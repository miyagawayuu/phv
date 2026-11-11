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
    PathError, Vm, VmError, BUDGET, LONG_OP,
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
    /// The VM that made the call: helpers that create objects, write output
    /// or check the limits run its own code (see `with_vm`).
    pub vm: *mut Vm,
    /// The program's function id of each function of the running unit, by
    /// its index in the unit (see `Installed::funcs`).
    pub funcs: *const u32,
    /// The program's class id of each class the running unit names.
    pub classes: *const u32,
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
/// The VM's own code, run by a helper, failed (`JitRt::error`): an
/// exception, or a limit, with the interpreter's message.
pub const JIT_ERR_VM: i64 = 11;

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
    /// For `JitCtx::funcs`: the program's id of each function of the unit.
    pub funcs: Arc<[u32]>,
    /// For `JitCtx::classes`: the program's id of each class the unit names.
    pub classes: Arc<[u32]>,
    /// Owns the executable memory `entry` points into: the unit lives at
    /// least as long as this.
    pub keep_alive: Arc<dyn std::any::Any + Send + Sync>,
}

impl Installed {
    /// What `Vm::call_jit` needs of it: pointers into it, which live as long
    /// as the table the VM keeps.
    pub(crate) fn call(&self) -> JitCall {
        JitCall {
            entry: self.entry,
            consts: self.consts.as_ptr(),
            funcs: self.funcs.as_ptr(),
            nfuncs: self.funcs.len(),
            classes: self.classes.as_ptr(),
        }
    }
}

/// See `Installed::call`.
#[derive(Clone, Copy)]
pub(crate) struct JitCall {
    pub entry: JitEntry,
    pub consts: *const u32,
    pub funcs: *const u32,
    pub nfuncs: usize,
    pub classes: *const u32,
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
    /// Per function: it, or a function it calls, changes an object or
    /// writes output. Such a call is never run again by the interpreter
    /// (its effects would happen twice): out of native stack it stops, and
    /// out of memory it fails as the interpreter would at that point.
    pub impure: Vec<bool>,
}

impl JitTable {
    pub fn new(entries: Vec<Option<Installed>>, writes_args: Vec<bool>, impure: Vec<bool>) -> JitTable {
        JitTable { entries, writes_args, impure }
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
    /// The error the VM's code returned to a helper (`JIT_ERR_VM`).
    error: Option<VmError>,
    /// When native code stops with an error, each native frame it leaves
    /// notes its function (by index in the unit) and the position after the
    /// instruction that failed, innermost first: the exception's trace has
    /// the calls the interpreter would have had.
    pub(crate) trace: Vec<(u32, u32)>,
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

    /// The error of `JIT_ERR_VM`.
    pub(crate) fn take_error(&mut self) -> VmError {
        self.error.take().unwrap_or_else(|| VmError::Fatal("internal error in native code".into()))
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

/// Runs `f` on the VM that made the native call, with native code's budget
/// (so a long step makes the next loop step or call check the limits, as in
/// the interpreter); an error is kept for the VM (`JIT_ERR_VM`).
///
/// # Safety
/// `ctx.vm` points to the VM running `Vm::call_jit`, which does not touch it
/// until native code returns; `f` must not run PHV code.
unsafe fn with_vm<T>(ctx: *mut JitCtx, f: impl FnOnce(&mut Vm) -> Result<T, VmError>) -> Option<T> {
    let vm = &mut *(*ctx).vm;
    vm.budget = (*ctx).budget.clamp(1, BUDGET as i64) as u32;
    let r = f(vm);
    (*ctx).budget = vm.budget as i64;
    match r {
        Ok(v) => Some(v),
        Err(e) => {
            rt(ctx).error = Some(e);
            (*ctx).error = JIT_ERR_VM;
            None
        }
    }
}

/// Called by native code when its budget runs out (at a loop step or a
/// call): the interpreter's own check, which runs the cycle collector when
/// it is due and checks the memory and time limits.
///
/// # Safety
/// See `with_vm`.
pub unsafe extern "C" fn phv_jit_tick(ctx: *mut JitCtx) -> i64 {
    match with_vm(ctx, |vm| vm.check_deadline()) {
        Some(()) => 0,
        None => JIT_ERR_VM,
    }
}

/// Notes that the native frame of function `func` (its index in the unit)
/// stops with an error after instruction `pc - 1` (see `JitRt::trace`).
///
/// # Safety
/// See `rt`.
pub unsafe extern "C" fn phv_jit_unwind(ctx: *mut JitCtx, func: i64, pc: i64) {
    rt(ctx).trace.push((func as u32, pc as u32));
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

/// Writes `v` to a window slot. Most slots written hold a number or nothing:
/// only a string, collection, object or closure there is dropped (the drop
/// of a `Value` is a call, and these writes are most of what a call that
/// passes an object costs).
#[inline(always)]
fn store(slot: &mut Value, v: Value) {
    let old = std::mem::replace(slot, v);
    if old.is_boxed() {
        drop(old);
    } else {
        std::mem::forget(old);
    }
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
    let rt = rt(ctx);
    let i = rt.used;
    if rt.windows.len() == i {
        rt.windows.push(Vec::new());
    }
    let (below, above) = rt.windows.split_at_mut(i);
    let (caller, w) = (&mut below[i - 1], &mut above[0]);
    // A window keeps its slots, all empty, when its function returns
    // (`phv_jit_leave`): the next call of a function with as many registers
    // at this depth (the same one, in a loop) finds it ready.
    let n = nregs as usize;
    if w.len() != n {
        if w.capacity() < n && mem(ctx).reserve(n * std::mem::size_of::<Value>()).is_err() {
            return JIT_ERR_MEMORY;
        }
        w.truncate(n);
        w.resize_with(n, Value::default);
    }
    let mut left = mask as u64 & 0xffff;
    while left != 0 {
        let k = left.trailing_zeros() as usize;
        left &= left - 1;
        store(&mut w[k], std::mem::take(&mut caller[args as usize + k]));
    }
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
        store(&mut rt.ret, std::mem::take(&mut w[ret as usize]));
    }
    // The values are let go and the slots kept, empty, for the next call
    // at this depth (see `phv_jit_enter`).
    for v in w.iter_mut() {
        if v.is_boxed() {
            *v = Value::Null;
        }
    }
    rt.used -= 1;
}

/// Moves the string the last call returned into slot `d`.
///
/// # Safety
/// See `rt`.
pub unsafe extern "C" fn phv_jit_take_ret(ctx: *mut JitCtx, d: i64) {
    let rt = rt(ctx);
    let v = std::mem::take(&mut rt.ret);
    store(&mut rt.top()[d as usize], v);
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
    // The kinds native code moves, without the call a general clone is.
    let v = match &w[s as usize] {
        Value::Object(o) => Value::Object(Rc::clone(o)),
        Value::Str(s) => Value::Str(Rc::clone(s)),
        Value::List(l) => Value::List(Rc::clone(l)),
        v => v.clone(),
    };
    store(&mut w[d as usize], v);
}

/// Stores a number or boolean (`JIT_PUT_*`) in slot `r`, so a string
/// operation can read it from there like the interpreter does.
///
/// # Safety
/// See `rt`.
pub unsafe extern "C" fn phv_jit_put(ctx: *mut JitCtx, r: i64, kind: i64, bits: i64) {
    let v = match kind {
        JIT_PUT_INT => Value::Int(bits),
        JIT_PUT_FLOAT => Value::Float(f64::from_bits(bits as u64)),
        JIT_PUT_NULL => Value::Null,
        _ => Value::Bool(bits != 0),
    };
    store(&mut rt(ctx).top()[r as usize], v);
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
/// A list, a map or an object, in slot `d` (property reads, see `deliver`).
pub const JIT_ELEM_LIST: i64 = 3;
pub const JIT_ELEM_MAP: i64 = 4;
pub const JIT_ELEM_OBJ: i64 = 5;

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
        Some(v) => v,
        None => {
            let msg = format!("list index {i} is out of range (length {})", list.len());
            throw(ctx, "IndexError", msg);
            return 0;
        }
    };
    match (kind, item) {
        (JIT_ELEM_INT, Value::Int(n)) => *n,
        (JIT_ELEM_INT, Value::Bool(b)) => *b as i64,
        (JIT_ELEM_FLOAT, Value::Float(f)) => f.to_bits() as i64,
        (JIT_ELEM_STR, Value::Str(s)) => {
            w[d as usize] = Value::Str(s.clone());
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
    if !push {
        let Value::List(items) = &mut w[r as usize] else { unreachable!("list checked above") };
        // A unique list can replace an existing slot without copying or growing.
        if let Some(slot) = Rc::get_mut(items).and_then(|items| items.get_mut(usize::try_from(i).ok()?)) {
            *slot = v;
            return;
        }
    }
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
            // Kinds kept in the window get the null there.
            if kind >= JIT_ELEM_STR {
                rt(ctx).top()[d as usize] = Value::Null;
            }
            0
        }
        (JIT_ELEM_INT, Value::Int(n)) => n,
        (JIT_ELEM_INT, Value::Bool(b)) => b as i64,
        (JIT_ELEM_FLOAT, Value::Float(f)) => f.to_bits() as i64,
        (JIT_ELEM_STR, v @ Value::Str(_))
        | (JIT_ELEM_LIST, v @ Value::List(_))
        | (JIT_ELEM_MAP, v @ Value::Map(_))
        | (JIT_ELEM_OBJ, v @ Value::Object(_)) => {
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

// Objects, methods and output. Objects live in the window like strings;
// properties are read and written by slot, with the interpreter's code.

/// `NewObject`: a new object of class `class` (by its index in the unit)
/// into slot `d`, made by the interpreter's own code (field defaults, the
/// cycle collector's list).
///
/// # Safety
/// See `with_vm`; `ctx.classes` has an entry for every class the unit names.
pub unsafe extern "C" fn phv_jit_new(ctx: *mut JitCtx, d: i64, class: i64) {
    let class = *(*ctx).classes.add(class as usize);
    let v = with_vm(ctx, |vm| Ok(((class as usize) < vm.prog.classes.len()).then(|| vm.new_object(class))));
    match v {
        Some(Some(v)) => store(&mut rt(ctx).top()[d as usize], v),
        _ => wrong_value(ctx),
    }
}

/// `GetProp`: property `slot` of the object in slot `o`, into register `d`
/// (see `deliver`; a null is accepted when `nullable` is set).
///
/// # Safety
/// See `rt`.
pub unsafe extern "C" fn phv_jit_get_prop(
    ctx: *mut JitCtx,
    d: i64,
    o: i64,
    slot: i64,
    kind: i64,
    nullable: i64,
) -> i64 {
    let v = match &rt(ctx).top()[o as usize] {
        Value::Object(ob) => match ob.try_borrow() {
            Ok(ob) => ob.fields.get(slot as usize).cloned(),
            Err(_) => None,
        },
        _ => None,
    };
    match v {
        Some(v) => deliver(ctx, d, kind, v, nullable != 0),
        None => {
            wrong_value(ctx);
            0
        }
    }
}

/// Reads a non-null numeric or boolean property without moving a `Value`
/// through the generic result path.
///
/// # Safety
/// See `rt`. The checked bytecode supplies the object register and slot.
pub unsafe extern "C" fn phv_jit_get_prop_scalar(ctx: *mut JitCtx, o: i64, slot: i64, kind: i64) -> i64 {
    if let Value::Object(object) = &rt(ctx).top()[o as usize] {
        if let Ok(object) = object.try_borrow() {
            match (kind, object.fields.get(slot as usize)) {
                (JIT_ELEM_INT, Some(Value::Int(n))) => return *n,
                (JIT_ELEM_INT, Some(Value::Bool(b))) => return i64::from(*b),
                (JIT_ELEM_FLOAT, Some(Value::Float(f))) => return f.to_bits() as i64,
                _ => {}
            }
        }
    }
    wrong_value(ctx);
    0
}

/// `SetProp`: the value in slot `v` into property `slot` of the object in slot `o`.
///
/// # Safety
/// See `rt`.
pub unsafe extern "C" fn phv_jit_set_prop(ctx: *mut JitCtx, o: i64, slot: i64, v: i64) {
    let w = rt(ctx).top();
    let val = w[v as usize].clone();
    let done = match &w[o as usize] {
        Value::Object(ob) => match ob.try_borrow_mut() {
            Ok(mut ob) => match ob.fields.get_mut(slot as usize) {
                Some(f) => {
                    let old = std::mem::replace(f, val);
                    drop(ob);
                    // Dropped after the borrow: the old value may hold the last
                    // reference to objects that point back at this one.
                    drop(old);
                    true
                }
                None => false,
            },
            Err(_) => false,
        },
        _ => false,
    };
    if !done {
        wrong_value(ctx);
    }
}

/// `SetProp` of a number or boolean (`JIT_PUT_*`): one call instead of
/// storing the value in the window first.
///
/// # Safety
/// See `rt`.
pub unsafe extern "C" fn phv_jit_set_prop_scalar(ctx: *mut JitCtx, o: i64, slot: i64, kind: i64, bits: i64) {
    let val = match kind {
        JIT_PUT_INT => Value::Int(bits),
        JIT_PUT_FLOAT => Value::Float(f64::from_bits(bits as u64)),
        JIT_PUT_NULL => Value::Null,
        _ => Value::Bool(bits != 0),
    };
    let done = match &rt(ctx).top()[o as usize] {
        Value::Object(ob) => match ob.try_borrow_mut() {
            Ok(mut ob) => match ob.fields.get_mut(slot as usize) {
                Some(f) => {
                    let old = std::mem::replace(f, val);
                    drop(ob);
                    drop(old);
                    true
                }
                None => false,
            },
            Err(_) => false,
        },
        _ => false,
    };
    if !done {
        wrong_value(ctx);
    }
}

/// `$o->p = $v` through a path: `$o->p[] = $v` (`push` set, no key) or
/// `$o->p[$k] = $v` (key in slot `k`), on property `slot` of the object in
/// slot `o`, with the value in slot `val`: copies accounted first, as the
/// interpreter does (`set_path_with`).
///
/// # Safety
/// See `rt` and `mem`.
pub unsafe extern "C" fn phv_jit_set_prop_path(ctx: *mut JitCtx, o: i64, slot: i64, k: i64, push: i64, val: i64) {
    let mem = mem(ctx);
    let w = rt(ctx).top();
    let Value::Object(ob) = &w[o as usize] else {
        wrong_value(ctx);
        return;
    };
    let ob = ob.clone();
    let v = w[val as usize].clone();
    let key = if k >= 0 { vec![w[k as usize].clone()] } else { Vec::new() };
    let push = push != 0;
    let r = {
        let Ok(mut b) = ob.try_borrow_mut() else {
            wrong_value(ctx);
            return;
        };
        let Some(field) = b.fields.get_mut(slot as usize) else {
            drop(b);
            wrong_value(ctx);
            return;
        };
        let cost = copy_cost(field, &key, push);
        if cost > 0 {
            if mem.reserve(cost).is_err() {
                drop(b);
                (*ctx).error = JIT_ERR_MEMORY;
                return;
            }
            long_op(ctx, cost);
        }
        path_assign(field, &key, &[], push, v)
    };
    long_key(ctx, key.iter().map(str_len).sum());
    if let Err((class, msg)) = r {
        if (*ctx).error == 0 {
            throw(ctx, class, msg);
        }
    }
}

/// `$o->p .= $x` on property `slot` of the object in slot `o`: `cur` holds
/// the value read before the string in slot `val` was made (see
/// `path_append`), as in the interpreter's `append_path_with`.
///
/// # Safety
/// See `rt` and `mem`.
pub unsafe extern "C" fn phv_jit_append_prop(ctx: *mut JitCtx, o: i64, slot: i64, cur: i64, val: i64) {
    let mem = mem(ctx);
    let w = rt(ctx).top();
    let Value::Object(ob) = &w[o as usize] else {
        wrong_value(ctx);
        return;
    };
    let ob = ob.clone();
    let cur = std::mem::take(&mut w[cur as usize]);
    let src = w[val as usize].clone();
    let r = {
        let Ok(mut b) = ob.try_borrow_mut() else {
            wrong_value(ctx);
            return;
        };
        let Some(field) = b.fields.get_mut(slot as usize) else {
            drop(b);
            wrong_value(ctx);
            return;
        };
        let cost = copy_cost(field, &[], false);
        if cost > 0 {
            if mem.reserve(cost).is_err() {
                drop(b);
                (*ctx).error = JIT_ERR_MEMORY;
                return;
            }
            long_op(ctx, cost);
        }
        path_append(field, &[], &[], cur, &src, mem)
    };
    match r {
        Ok(work) => long_op(ctx, work),
        Err(PathError::Memory(_)) => (*ctx).error = JIT_ERR_MEMORY,
        Err(PathError::Throw(class, msg)) => throw(ctx, class, msg),
    }
}

/// `CallVirtual` of vtable slot `slot` on the object in slot `o`: the
/// program's id of the function to call (the caller compares it with the
/// functions of its unit), or -1 with an error.
///
/// # Safety
/// See `rt`.
pub unsafe extern "C" fn phv_jit_vtarget(ctx: *mut JitCtx, o: i64, slot: i64) -> i64 {
    let rt = rt(ctx);
    let class = match &rt.top()[o as usize] {
        Value::Object(ob) => ob.try_borrow().ok().map(|b| b.class),
        _ => None,
    };
    let f = class.and_then(|c| {
        let prog = rt.prog.as_ref()?;
        prog.classes.get(c as usize)?.vtable.get(slot as usize).copied()
    });
    match f {
        Some(f) => f as i64,
        None => {
            wrong_value(ctx);
            -1
        }
    }
}

/// `Echo` (or, with `attr` set, `EchoAttr`) of the value in slot `r`: the
/// interpreter's own code, with the output checks, escaping and limits of
/// the buffer being written; returns an error code.
///
/// # Safety
/// See `with_vm`.
pub unsafe extern "C" fn phv_jit_echo(ctx: *mut JitCtx, r: i64, attr: i64) -> i64 {
    let v = rt(ctx).top()[r as usize].clone();
    // The frame's index as the interpreter would have it, for the guards
    // of the HTML around the call (and those the function set itself).
    let fi = frame_index(ctx);
    match with_vm(ctx, |vm| vm.echo_value(fi, v, attr != 0)) {
        Some(()) => 0,
        None => JIT_ERR_VM,
    }
}

/// `EchoHtml` of constant `c` of the unit; returns an error code.
///
/// # Safety
/// See `with_vm` and `phv_jit_const`.
pub unsafe extern "C" fn phv_jit_echo_html(ctx: *mut JitCtx, c: i64) -> i64 {
    let c = *(*ctx).consts.add(c as usize) as usize;
    let fi = frame_index(ctx);
    match with_vm(ctx, |vm| vm.echo_html_const(fi, c)) {
        Some(()) => 0,
        None => JIT_ERR_VM,
    }
}

/// A `CallVirtual` reached a function its unit does not have (only bytecode
/// that is not what the compiler makes could do that).
///
/// # Safety
/// See `rt`.
pub unsafe extern "C" fn phv_jit_bad(ctx: *mut JitCtx) {
    wrong_value(ctx);
}

/// The interpreter's index of the frame of the native function running: the
/// frames of native code continue those of the interpreter.
///
/// # Safety
/// See `rt`.
unsafe fn frame_index(ctx: *mut JitCtx) -> usize {
    ((*ctx).depth - 1).max(0) as usize
}

/// `GuardOutput` (`end` 0) or `EndGuard` (`end` 1) in the running native
/// frame, as the interpreter notes them.
///
/// # Safety
/// See `with_vm`.
pub unsafe extern "C" fn phv_jit_guard(ctx: *mut JitCtx, end: i64) {
    let fi = frame_index(ctx);
    with_vm(ctx, |vm| {
        if end == 0 {
            vm.guard_output(fi);
        } else {
            vm.end_guard(fi);
        }
        Ok(())
    });
}

/// `EchoNonce`; returns an error code.
///
/// # Safety
/// See `with_vm`.
pub unsafe extern "C" fn phv_jit_echo_nonce(ctx: *mut JitCtx) -> i64 {
    let fi = frame_index(ctx);
    match with_vm(ctx, |vm| vm.echo_nonce(fi)) {
        Some(()) => 0,
        None => JIT_ERR_VM,
    }
}
