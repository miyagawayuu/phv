//! A Cranelift JIT for functions that compute with `int`, `bool`, `float`
//! and `string`, and lists of them (the compiler marks them with a
//! `NativeSig`). Everything else stays in the interpreter.
//!
//! Numbers and booleans live in machine registers. A function that uses
//! strings or lists also gets a window of `Value` slots owned by the VM, and
//! every string or list operation calls a Rust helper in `phv_vm::jit` that
//! runs the interpreter's own code on that window.
//!
//! Native code keeps the interpreter's guarantees: integer overflow, division
//! and modulo by zero, negative shifts and out-of-range float to int
//! conversions raise the same exceptions, recursion depth is limited, string
//! operations check the memory limit, and a budget counter at calls and loop
//! back-edges lets the time and memory limits interrupt long-running code.

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::{
    types, AbiParam, Block, FuncRef, InstBuilder, MemFlagsData, Signature, TrapCode, Type, Value,
};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{default_libcall_names, FuncId, Linkage, Module};
use phv_vm::bytecode::{Const, Function, Instr, NativeSig, PathBase, Program, Reg, Scalar, ValKind, NO_REG};
use phv_vm::jit::*;
use phv_vm::{JitEntry, JitTable};
use std::collections::{HashMap, HashSet};

const I64: Type = types::I64;
const F64: Type = types::F64;
// Offsets of the `JitCtx` fields.
const CTX_ERROR: i32 = 0;
const CTX_DEPTH: i32 = 8;
const CTX_MAX_DEPTH: i32 = 16;
const CTX_BUDGET: i32 = 24;
const CTX_DETAIL: i32 = 40;
const CTX_STACK_LIMIT: i32 = 56;
const _: () = {
    use std::mem::offset_of;
    assert!(offset_of!(phv_vm::jit::JitCtx, error) == CTX_ERROR as usize);
    assert!(offset_of!(phv_vm::jit::JitCtx, depth) == CTX_DEPTH as usize);
    assert!(offset_of!(phv_vm::jit::JitCtx, max_depth) == CTX_MAX_DEPTH as usize);
    assert!(offset_of!(phv_vm::jit::JitCtx, budget) == CTX_BUDGET as usize);
    assert!(offset_of!(phv_vm::jit::JitCtx, detail) == CTX_DETAIL as usize);
    assert!(offset_of!(phv_vm::jit::JitCtx, stack_limit) == CTX_STACK_LIMIT as usize);
};

/// Owns the JIT's executable memory.
struct Code(#[allow(dead_code)] JITModule);
// SAFETY: after `finalize_definitions` the module is never used again except
// to be dropped; the code it owns is immutable and safe to call from any thread.
unsafe impl Send for Code {}
unsafe impl Sync for Code {}

/// What a register holds at some point of a function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum K {
    /// Not written yet (or moved out by a call).
    Undef,
    Int,
    Bool,
    Float,
    Str,
    /// A list of elements of one kind (in the window).
    List(E),
    /// A map with keys and values of one kind each (in the window).
    Map(E, E),
    /// `null`.
    Null,
    /// A scalar or `null`: a machine register and a null flag (a string is
    /// in the window).
    Opt(E),
    /// Different kinds on different paths: must not be read.
    Mixed,
}

/// The elements of a list or map, or the scalar of a nullable value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum E {
    Int,
    Bool,
    Float,
    Str,
    /// An empty list or map (`[]`): elements of any kind.
    Empty,
}

impl E {
    fn from(s: Scalar) -> E {
        match s {
            Scalar::Int => E::Int,
            Scalar::Bool => E::Bool,
            Scalar::Float => E::Float,
            Scalar::Str => E::Str,
        }
    }

    /// The element of `JIT_ELEM_*` a helper delivers for this kind.
    fn code(self) -> i64 {
        match self {
            E::Float => JIT_ELEM_FLOAT,
            E::Str => JIT_ELEM_STR,
            _ => JIT_ELEM_INT,
        }
    }

    /// The element kinds `a` and `b` together (an empty container's is any).
    fn join(a: E, b: E) -> Option<E> {
        match (a, b) {
            _ if a == b => Some(a),
            (E::Empty, x) | (x, E::Empty) => Some(x),
            _ => None,
        }
    }

    fn of(k: K) -> Option<E> {
        Some(match k {
            K::Int => E::Int,
            K::Bool => E::Bool,
            K::Float => E::Float,
            K::Str => E::Str,
            _ => return None,
        })
    }

    /// The kind of an element read from the list.
    fn scalar(self) -> Option<K> {
        Some(match self {
            E::Int => K::Int,
            E::Bool => K::Bool,
            E::Float => K::Float,
            E::Str => K::Str,
            E::Empty => return None,
        })
    }
}

impl K {
    fn of(v: ValKind) -> Option<K> {
        match v {
            ValKind::Int => Some(K::Int),
            ValKind::Bool => Some(K::Bool),
            ValKind::Float => Some(K::Float),
            ValKind::Str => Some(K::Str),
            ValKind::List(s) => Some(K::List(E::from(s))),
            ValKind::Map(k, v) => Some(K::Map(E::from(k), E::from(v))),
            ValKind::Opt(s) => Some(K::Opt(E::from(s))),
            ValKind::Void => None,
        }
    }

    /// Kept in the register window, not in a machine register.
    fn boxed(self) -> bool {
        matches!(self, K::Str | K::List(_) | K::Map(..) | K::Opt(E::Str))
    }

    /// A scalar, nullable or not: its element kind and whether it may be null.
    fn base(self) -> Option<(E, bool)> {
        Some(match self {
            K::Int => (E::Int, false),
            K::Bool => (E::Bool, false),
            K::Float => (E::Float, false),
            K::Str => (E::Str, false),
            K::Opt(e) => (e, true),
            _ => return None,
        })
    }

    /// The kind without `null` (a nullable value checked not to be null).
    fn strip(self) -> K {
        match self {
            K::Opt(e) => e.scalar().unwrap_or(self),
            k => k,
        }
    }

    /// The kind at a point two paths reach with `self` and `other`: an empty
    /// list or map holds any kind, and a scalar meets `null` as a nullable
    /// value; other differences cannot be read.
    fn join(self, other: K) -> K {
        let j = match (self, other) {
            _ if self == other => Some(self),
            (K::List(a), K::List(b)) => E::join(a, b).map(K::List),
            (K::Map(k1, v1), K::Map(k2, v2)) => E::join(k1, k2).zip(E::join(v1, v2)).map(|(k, v)| K::Map(k, v)),
            (K::Null, x) | (x, K::Null) => x.base().map(|(e, _)| K::Opt(e)),
            (K::Opt(e), x) | (x, K::Opt(e)) if x.base() == Some((e, false)) => Some(K::Opt(e)),
            _ => None,
        };
        j.unwrap_or(K::Mixed)
    }
}

/// Whether a value of kind `have` may be used where `want` is needed: the
/// same kind, an empty list or map for any, a scalar or `null` for a nullable
/// one, and a nullable value where the scalar is needed (the type checker
/// proved that it is not null there).
fn fits(have: K, want: K) -> bool {
    have == want
        || match (have, want) {
            (K::List(E::Empty), K::List(_)) | (K::Map(E::Empty, E::Empty), K::Map(..)) | (K::Null, K::Opt(_)) => true,
            (K::Map(k1, v1), K::Map(k2, v2)) => E::join(k1, k2) == Some(k2) && E::join(v1, v2) == Some(v2),
            (x, K::Opt(e)) => x.base().is_some_and(|(b, _)| b == e),
            (K::Opt(e), x) => x.base() == Some((e, false)),
            _ => false,
        }
}

/// How `==` / `!=` is computed natively.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EqMode {
    /// Two non-null ints, bools or strings.
    Plain,
    /// `null == null`.
    BothNull,
    /// One side is `null`: whether the other (`a` when true) is null.
    NullTest(bool),
    /// Nullable ints, bools or floats.
    Opt(E),
}

fn eq_mode(ka: K, kb: K) -> Option<EqMode> {
    match (ka, kb) {
        (K::Null, K::Null) => Some(EqMode::BothNull),
        (K::Null, x) => x.base().map(|_| EqMode::NullTest(false)),
        (x, K::Null) => x.base().map(|_| EqMode::NullTest(true)),
        _ => {
            let ((ea, oa), (eb, ob)) = (ka.base()?, kb.base()?);
            if ea != eb {
                return None;
            }
            if !oa && !ob {
                return matches!(ea, E::Int | E::Bool | E::Str).then_some(EqMode::Plain);
            }
            matches!(ea, E::Int | E::Bool | E::Float).then_some(EqMode::Opt(ea))
        }
    }
}

/// The element kind of list register `x`.
fn need_list(st: &[K], x: Reg) -> Option<E> {
    match kind(st, x)? {
        K::List(e) => Some(e),
        _ => None,
    }
}

/// The key and value kinds of map register `x`.
fn need_map(st: &[K], x: Reg) -> Option<(E, E)> {
    match kind(st, x)? {
        K::Map(k, v) => Some((k, v)),
        _ => None,
    }
}

/// Register `x` holds a key for a map whose keys are `ke`.
fn need_key(st: &[K], x: Reg, ke: E) -> Option<()> {
    let k = kind(st, x)?;
    match ke {
        E::Empty => matches!(k, K::Int | K::Str).then_some(()),
        e => need(st, x, e.scalar()?),
    }
}

/// The kind of register `x` when it holds a value.
fn kind(st: &[K], x: Reg) -> Option<K> {
    st.get(x as usize).copied().filter(|k| !matches!(k, K::Undef | K::Mixed))
}

fn need(st: &[K], x: Reg, k: K) -> Option<()> {
    let have = kind(st, x)?;
    (have == k
        || have.strip() == k && have != K::Opt(E::Str) && have.base().is_some()
        || have == K::Opt(E::Str) && k == K::Str)
        .then_some(())
}

fn set(st: &mut [K], x: Reg, k: K) -> Option<()> {
    *st.get_mut(x as usize)? = k;
    Some(())
}

/// Where control goes after an instruction.
enum Flow {
    Next,
    Jump(u32),
    /// To the next instruction or to the target.
    Branch(u32),
    Stop,
}

/// A function that passed `well_formed`.
struct Checked {
    /// Register kinds before each instruction; None where it is unreachable.
    kinds: Vec<Option<Vec<K>>>,
    /// Whether it uses strings or lists, and so needs a register window.
    strings: bool,
}

/// The largest function the JIT compiles, in registers and instructions.
const MAX_REGS: usize = 1024;
const MAX_INSTRUCTIONS: usize = 16_384;

/// Checks `f` for native compilation: every register, jump target, constant
/// and callee is in range, every instruction is supported, and every operand
/// has the kind the instruction needs on every path that reaches it (found by
/// a forward data flow analysis). Callees must be in `ok`. Bytecode can come
/// from the on-disk cache, and the JIT must never trust it blindly.
fn well_formed(prog: &Program, f: &Function, ok: &HashSet<u32>) -> Option<Checked> {
    let sig = f.native.as_ref()?;
    let regs = f.num_regs as usize;
    let n = f.code.len();
    // Compiling a function takes memory and time that grow with its size
    // (not counted against a script's limits): very large ones are left to
    // the interpreter.
    if regs > MAX_REGS || n > MAX_INSTRUCTIONS {
        return None;
    }
    if sig.params.len() > 16 || sig.params.len() > regs || f.num_params as usize != sig.params.len() {
        return None;
    }
    if !matches!(f.code.last(), Some(Instr::Ret(_)) | Some(Instr::RetNull)) {
        return None;
    }
    let mut entry = vec![K::Undef; regs];
    for (i, p) in sig.params.iter().enumerate() {
        entry[i] = K::of(*p)?;
    }
    let mut strings = sig.params.iter().any(|p| p.boxed()) || sig.ret.boxed();
    let mut kinds: Vec<Option<Vec<K>>> = vec![None; n];
    kinds[0] = Some(entry);
    let mut work = vec![0usize];
    while let Some(i) = work.pop() {
        let mut st = kinds[i].clone().unwrap();
        // An instruction may fail with the kinds known so far and pass once
        // they are complete (a list that starts as `[]` gets its element kind
        // later in a loop): every instruction is checked again at the end.
        let Some(flow) = step(prog, sig, ok, &f.code[i], &mut st, &mut strings) else { continue };
        let targets = match flow {
            Flow::Next => [Some(i + 1), None],
            Flow::Jump(t) => [Some(t as usize), None],
            // Only `Jump` checks the clock when it goes back: the compiler's
            // loops branch forward and jump back, and other bytecode could
            // loop without ever checking the time limit.
            Flow::Branch(t) if t as usize <= i => return None,
            Flow::Branch(t) => [Some(i + 1), Some(t as usize)],
            Flow::Stop => [None, None],
        };
        for t in targets.into_iter().flatten() {
            if t >= n {
                return None;
            }
            match &mut kinds[t] {
                None => {
                    kinds[t] = Some(st.clone());
                    work.push(t);
                }
                Some(old) => {
                    let mut changed = false;
                    for (o, k) in old.iter_mut().zip(&st) {
                        let j = o.join(*k);
                        if j != *o {
                            *o = j;
                            changed = true;
                        }
                    }
                    if changed {
                        work.push(t);
                    }
                }
            }
        }
    }
    for (i, st) in kinds.iter().enumerate() {
        if let Some(st) = st {
            if step(prog, sig, ok, &f.code[i], &mut st.clone(), &mut strings).is_none() {
                explain(f, i, st);
                return None;
            }
        }
    }
    Some(Checked { kinds, strings })
}

/// `PHV_JIT_EXPLAIN=1`: says which instruction keeps a function out of the
/// JIT, with the register kinds before it.
fn explain(f: &Function, i: usize, st: &[K]) {
    if std::env::var_os("PHV_JIT_EXPLAIN").is_some() {
        let kinds: Vec<String> = st.iter().enumerate().map(|(r, k)| format!("r{r}={k:?}")).collect();
        eprintln!("phv jit: {} not compiled: instruction {i} {:?} with {}", f.name, f.code[i], kinds.join(" "));
    }
}

/// Checks one instruction against the register kinds before it and updates
/// them; None when it cannot be compiled.
fn step(
    prog: &Program,
    sig: &NativeSig,
    ok: &HashSet<u32>,
    ins: &Instr,
    st: &mut [K],
    strings: &mut bool,
) -> Option<Flow> {
    use Instr::*;
    match *ins {
        Int(d, _) => set(st, d, K::Int)?,
        Bool(d, _) => set(st, d, K::Bool)?,
        Null(d) => set(st, d, K::Null)?,
        Const(d, c) => {
            let k = match prog.consts.get(c as usize)? {
                phv_vm::bytecode::Const::Int(_) => K::Int,
                phv_vm::bytecode::Const::Bool(_) => K::Bool,
                phv_vm::bytecode::Const::Float(_) => K::Float,
                phv_vm::bytecode::Const::Str(_) => K::Str,
                phv_vm::bytecode::Const::EmptyList => K::List(E::Empty),
                phv_vm::bytecode::Const::EmptyMap => K::Map(E::Empty, E::Empty),
                _ => return None,
            };
            set(st, d, k)?;
        }
        Move(d, s) => {
            let k = kind(st, s)?;
            set(st, d, k)?;
        }
        AddI(d, a, b)
        | SubI(d, a, b)
        | MulI(d, a, b)
        | ModI(d, a, b)
        | BitAnd(d, a, b)
        | BitOr(d, a, b)
        | BitXor(d, a, b)
        | Shl(d, a, b)
        | Shr(d, a, b) => {
            need(st, a, K::Int)?;
            need(st, b, K::Int)?;
            set(st, d, K::Int)?;
        }
        AddIImm(d, a, _) | SubIImm(d, a, _) | NegI(d, a) | BitNot(d, a) => {
            need(st, a, K::Int)?;
            set(st, d, K::Int)?;
        }
        IncI(r, _) => need(st, r, K::Int)?,
        Not(d, a) => {
            need(st, a, K::Bool)?;
            set(st, d, K::Bool)?;
        }
        EqI(d, a, b) | LtI(d, a, b) | LeI(d, a, b) => {
            need(st, a, K::Int)?;
            need(st, b, K::Int)?;
            set(st, d, K::Bool)?;
        }
        Eq(d, a, b) | Ne(d, a, b) => {
            eq_mode(kind(st, a)?, kind(st, b)?)?;
            set(st, d, K::Bool)?;
        }
        Lt(d, a, b) | Le(d, a, b) | Cmp(d, a, b) => {
            // Floats use the float forms (`LtF`, ...); lists are not compared natively.
            let k = kind(st, a)?.strip();
            if kind(st, b)?.strip() != k || !matches!(k, K::Int | K::Bool | K::Str) {
                return None;
            }
            set(st, d, if matches!(ins, Cmp(..)) { K::Int } else { K::Bool })?;
        }
        AddF(d, a, b) | SubF(d, a, b) | MulF(d, a, b) | DivF(d, a, b) | ModF(d, a, b) | PowF(d, a, b) => {
            need(st, a, K::Float)?;
            need(st, b, K::Float)?;
            set(st, d, K::Float)?;
        }
        NegF(d, a) => {
            need(st, a, K::Float)?;
            set(st, d, K::Float)?;
        }
        EqF(d, a, b) | NeF(d, a, b) | LtF(d, a, b) | LeF(d, a, b) | CmpF(d, a, b) => {
            need(st, a, K::Float)?;
            need(st, b, K::Float)?;
            set(st, d, if matches!(ins, CmpF(..)) { K::Int } else { K::Bool })?;
        }
        IntToFloat(d, a) => {
            need(st, a, K::Int)?;
            set(st, d, K::Float)?;
        }
        FloatToInt(d, a) => {
            need(st, a, K::Float)?;
            set(st, d, K::Int)?;
        }
        ToStr(d, a) => {
            kind(st, a)?;
            set(st, d, K::Str)?;
        }
        Concat(d, start, n, false) => {
            // The interpreter reads registers `start..start + n` even when `n` is 0.
            if n == 0 {
                return None;
            }
            for i in 0..n {
                kind(st, start.checked_add(i)?)?;
            }
            set(st, d, K::Str)?;
        }
        AppendStr(d, s) => {
            need(st, d, K::Str)?;
            kind(st, s)?;
        }
        CallBuiltin { dst, id, args, argc } => {
            let ret = match (prog.builtin_names.get(id as usize)?.as_str(), argc) {
                ("strlen" | "str_len", 1) => {
                    need(st, args, K::Str)?;
                    K::Int
                }
                // `count($xs)` and `$xs->len()`
                ("list_len", 1) => {
                    need_list(st, args)?;
                    K::Int
                }
                ("map_len", 1) => {
                    need_map(st, args)?;
                    K::Int
                }
                ("map_has", 2) => {
                    let (ke, _) = need_map(st, args)?;
                    need_key(st, args.checked_add(1)?, ke)?;
                    K::Bool
                }
                ("range", 2) => {
                    need(st, args, K::Int)?;
                    need(st, args.checked_add(1)?, K::Int)?;
                    K::List(E::Int)
                }
                _ => return None,
            };
            // The interpreter moves arguments out of their registers.
            for i in 0..argc {
                set(st, args + i, K::Undef)?;
            }
            set(st, dst, ret)?;
        }
        Jump(t) => return Some(Flow::Jump(t)),
        JumpIfFalse(r, t) | JumpIfTrue(r, t) => {
            need(st, r, K::Bool)?;
            return Some(Flow::Branch(t));
        }
        JumpIfNotLtI(a, b, t) | JumpIfNotLeI(a, b, t) => {
            need(st, a, K::Int)?;
            need(st, b, K::Int)?;
            return Some(Flow::Branch(t));
        }
        JumpIfNotLtIImm(a, _, t) | JumpIfNotLeIImm(a, _, t) => {
            need(st, a, K::Int)?;
            return Some(Flow::Branch(t));
        }
        Call { dst, func, args, argc } => {
            if !ok.contains(&func) {
                return None;
            }
            let callee = prog.funcs.get(func as usize)?.native.as_ref()?;
            if callee.params.len() != argc as usize {
                return None;
            }
            for (k, p) in callee.params.iter().enumerate() {
                fits(kind(st, args.checked_add(k as u16)?)?, K::of(*p)?).then_some(())?;
            }
            for k in 0..argc {
                set(st, args + k, K::Undef)?;
            }
            let ret = match callee.ret {
                ValKind::Void => K::Undef,
                v => K::of(v)?,
            };
            // Arguments and results passed in the window (strings, lists,
            // maps and nullable values) need one in the caller too (found by
            // the 1.0 audit: an int function calling a `?int` one wrote into
            // its caller's window).
            if callee.ret.boxed() || callee.params.iter().any(|p| p.boxed()) {
                *strings = true;
            }
            set(st, dst, ret)?;
        }
        Ret(r) => {
            match K::of(sig.ret) {
                None => kind(st, r).map(|_| ())?,
                Some(k) => fits(kind(st, r)?, k).then_some(())?,
            }
            return Some(Flow::Stop);
        }
        // A string, list or map function must return one on every path.
        RetNull if sig.ret.boxed() && !matches!(sig.ret, ValKind::Opt(_)) => return None,
        RetNull => return Some(Flow::Stop),
        PowI(d, a, b) => {
            need(st, a, K::Int)?;
            need(st, b, K::Int)?;
            set(st, d, K::Int)?;
        }
        NewList(d, start, n) => {
            let e = if n == 0 {
                E::Empty
            } else {
                let k = kind(st, start)?;
                for i in 0..n {
                    need(st, start.checked_add(i)?, k)?;
                }
                // The items are moved into the list.
                for i in 0..n {
                    set(st, start + i, K::Undef)?;
                }
                E::of(k)?
            };
            set(st, d, K::List(e))?;
        }
        Index(d, c, i) | IndexOrNull(d, c, i) => {
            let e = match kind(st, c)? {
                K::List(e) => {
                    need(st, i, K::Int)?;
                    e
                }
                K::Map(ke, ve) => {
                    need_key(st, i, ke)?;
                    ve
                }
                _ => return None,
            };
            let k = match (ins, e.scalar()) {
                (IndexOrNull(..), Some(_)) => K::Opt(e),
                // Nothing is ever found in an empty list or map.
                (IndexOrNull(..), None) => K::Null,
                (_, k) => k?,
            };
            set(st, d, k)?;
        }
        SetPath(ref path) if matches!(path.base, PathBase::Local(r) if matches!(kind(st, r), Some(K::Map(..)))) => {
            let PathBase::Local(r) = path.base else { return None };
            let (ke, ve) = need_map(st, r)?;
            let [k] = path.keys.as_slice() else { return None };
            if path.push {
                return None;
            }
            let kk = E::of(kind(st, *k)?)?;
            let v = E::of(kind(st, path.val)?)?;
            let (ke, ve) = (E::join(ke, kk)?, E::join(ve, v)?);
            if !matches!(ke, E::Int | E::Str) {
                return None;
            }
            set(st, r, K::Map(ke, ve))?;
        }
        NewMap(d, start, n) => {
            let k = if n == 0 {
                K::Map(E::Empty, E::Empty)
            } else {
                let (kk, vk) = (kind(st, start)?, kind(st, start.checked_add(1)?)?);
                if !matches!(kk, K::Int | K::Str) {
                    return None;
                }
                for i in 0..n {
                    let at = start.checked_add(i.checked_mul(2)?)?;
                    need(st, at, kk)?;
                    need(st, at.checked_add(1)?, vk)?;
                }
                // The keys and values are moved into the map.
                for i in 0..2 * n {
                    set(st, start + i, K::Undef)?;
                }
                K::Map(E::of(kk)?, E::of(vk)?)
            };
            set(st, d, k)?;
        }
        JumpIfNull(r, t) | JumpIfNotNull(r, t) => {
            kind(st, r)?;
            return Some(Flow::Branch(t));
        }
        SetPath(ref path) => {
            let PathBase::Local(r) = path.base else { return None };
            let e = need_list(st, r)?;
            let v = E::of(kind(st, path.val)?)?;
            if e != E::Empty && e != v {
                return None;
            }
            match (path.push, path.keys.as_slice()) {
                (true, []) => {}
                (false, [k]) => need(st, *k, K::Int)?,
                _ => return None,
            }
            set(st, r, K::List(v))?;
        }
        IterInit(it, src) => {
            let k = kind(st, src)?;
            if !matches!(k, K::List(_) | K::Map(..)) {
                return None;
            }
            set(st, it, k)?;
            set(st, it.checked_add(1)?, K::Int)?;
        }
        IterNext { it, key, val, end } => {
            let (ke, ve) = match kind(st, it)? {
                K::List(e) => (E::Int, e),
                K::Map(k, v) => (k, v),
                _ => return None,
            };
            need(st, it.checked_add(1)?, K::Int)?;
            // A list or map known to be empty has no entries: the loop ends.
            if ve == E::Empty {
                return Some(Flow::Jump(end));
            }
            if key != NO_REG {
                set(st, key, ke.scalar()?)?;
            }
            set(st, val, ve.scalar()?)?;
            if st.iter().any(|k| k.boxed()) {
                *strings = true;
            }
            return Some(Flow::Branch(end));
        }
        Range(d, a, b, _) => {
            need(st, a, K::Int)?;
            need(st, b, K::Int)?;
            set(st, d, K::List(E::Int))?;
        }
        _ => return None,
    }
    if st.iter().any(|k| k.boxed()) {
        *strings = true;
    }
    Some(Flow::Next)
}

/// Functions that can be compiled: well formed, with calls only to other
/// compilable functions.
fn eligible(prog: &Program) -> HashMap<u32, Checked> {
    let mut ok: HashSet<u32> =
        prog.funcs.iter().enumerate().filter(|(_, f)| f.native.is_some()).map(|(i, _)| i as u32).collect();
    loop {
        let checked: HashMap<u32, Checked> =
            ok.iter().filter_map(|&f| well_formed(prog, &prog.funcs[f as usize], &ok).map(|c| (f, c))).collect();
        if checked.len() == ok.len() {
            return checked;
        }
        ok = checked.into_keys().collect();
    }
}

/// The machine type of a value of kind `k` (strings are in the window; their
/// argument slot is an unused i64).
fn abi(k: ValKind) -> Type {
    if k == ValKind::Float {
        F64
    } else {
        I64
    }
}

/// `(ctx, first argument register in the caller's window, params...) -> ret`.
fn inner_sig(module: &JITModule, sig: &NativeSig) -> Signature {
    let mut s = module.make_signature();
    s.params.push(AbiParam::new(module.target_config().pointer_type()));
    s.params.push(AbiParam::new(I64));
    for p in &sig.params {
        s.params.push(AbiParam::new(abi(*p)));
    }
    s.returns.push(AbiParam::new(abi(sig.ret)));
    s
}

/// Rust functions native code calls.
#[derive(Clone, Copy)]
enum H {
    Tick,
    Enter,
    Leave,
    TakeRet,
    Const,
    Move,
    Put,
    ToStr,
    Concat,
    Append,
    Strlen,
    StrCmp,
    Fmod,
    Pow,
    NewList,
    Index,
    ListLen,
    SetIndex,
    Range,
    PowI,
    RangeFn,
    Push,
    IsNull,
    Unbox,
    Get,
    SetKey,
    Entry,
    Has,
    NewMap,
}

/// Name, address, number of i64 arguments after `ctx` (None: `(f64, f64) -> f64`)
/// and whether it returns an i64, in the order of `H`.
fn helpers() -> [(&'static str, *const u8, Option<usize>, bool); 29] {
    [
        ("phv_jit_tick", phv_jit_tick as *const u8, Some(0), true),
        ("phv_jit_enter", phv_jit_enter as *const u8, Some(3), true),
        ("phv_jit_leave", phv_jit_leave as *const u8, Some(1), false),
        ("phv_jit_take_ret", phv_jit_take_ret as *const u8, Some(1), false),
        ("phv_jit_const", phv_jit_const as *const u8, Some(2), false),
        ("phv_jit_move", phv_jit_move as *const u8, Some(2), false),
        ("phv_jit_put", phv_jit_put as *const u8, Some(3), false),
        ("phv_jit_to_str", phv_jit_to_str as *const u8, Some(2), false),
        ("phv_jit_concat", phv_jit_concat as *const u8, Some(3), true),
        ("phv_jit_append", phv_jit_append as *const u8, Some(2), true),
        ("phv_jit_strlen", phv_jit_strlen as *const u8, Some(2), true),
        ("phv_jit_str_cmp", phv_jit_str_cmp as *const u8, Some(2), true),
        ("phv_jit_fmod", phv_jit_fmod as *const u8, None, false),
        ("phv_jit_pow", phv_jit_pow as *const u8, None, false),
        ("phv_jit_new_list", phv_jit_new_list as *const u8, Some(3), false),
        ("phv_jit_index", phv_jit_index as *const u8, Some(4), true),
        ("phv_jit_list_len", phv_jit_list_len as *const u8, Some(1), true),
        ("phv_jit_set_index", phv_jit_set_index as *const u8, Some(4), false),
        ("phv_jit_range", phv_jit_range as *const u8, Some(4), false),
        ("phv_jit_pow_i", phv_jit_pow_i as *const u8, Some(2), true),
        ("phv_jit_range_fn", phv_jit_range_fn as *const u8, Some(3), false),
        ("phv_jit_push", phv_jit_push as *const u8, Some(3), false),
        ("phv_jit_is_null", phv_jit_is_null as *const u8, Some(1), true),
        ("phv_jit_unbox", phv_jit_unbox as *const u8, Some(2), true),
        ("phv_jit_get", phv_jit_get as *const u8, Some(5), true),
        ("phv_jit_set_key", phv_jit_set_key as *const u8, Some(3), false),
        ("phv_jit_entry", phv_jit_entry as *const u8, Some(5), true),
        ("phv_jit_has", phv_jit_has as *const u8, Some(2), true),
        ("phv_jit_new_map", phv_jit_new_map as *const u8, Some(3), false),
    ]
}

/// Whether `f` changes one of its parameters in place: a string, list or
/// map passed in the window (see `JitTable::writes_args`).
fn writes_args(f: &Function) -> bool {
    let param = |r: Reg| (r as usize) < f.num_params as usize;
    f.code.iter().any(|ins| match ins {
        Instr::AppendStr(d, _) => param(*d),
        Instr::SetPath(p) => matches!(p.base, PathBase::Local(r) if param(r)),
        _ => false,
    })
}

/// Compiles every eligible function. Returns `None` if there are none (or
/// the host is not supported by Cranelift).
pub fn build(prog: &Program) -> Option<JitTable> {
    let checked = eligible(prog);
    if checked.is_empty() {
        return None;
    }
    let mut flags = settings::builder();
    flags.set("use_colocated_libcalls", "false").ok()?;
    flags.set("is_pic", "false").ok()?;
    flags.set("opt_level", "speed").ok()?;
    let isa = cranelift_native::builder().ok()?.finish(settings::Flags::new(flags)).ok()?;
    let mut jb = JITBuilder::with_isa(isa, default_libcall_names());
    for (name, addr, ..) in helpers() {
        jb.symbol(name, addr);
    }
    let mut module = JITModule::new(jb);
    let ptr = module.target_config().pointer_type();

    let mut imports = Vec::new();
    for (name, _, args, ret) in helpers() {
        let mut s = module.make_signature();
        match args {
            Some(n) => {
                s.params.push(AbiParam::new(ptr));
                s.params.extend((0..n).map(|_| AbiParam::new(I64)));
                if ret {
                    s.returns.push(AbiParam::new(I64));
                }
            }
            None => {
                s.params.extend([AbiParam::new(F64), AbiParam::new(F64)]);
                s.returns.push(AbiParam::new(F64));
            }
        }
        imports.push(module.declare_function(name, Linkage::Import, &s).ok()?);
    }

    let mut ids: Vec<Option<(FuncId, FuncId)>> = vec![None; prog.funcs.len()];
    let mut order: Vec<u32> = checked.keys().copied().collect();
    order.sort();
    let entry_sig = {
        let mut s = module.make_signature();
        s.params.push(AbiParam::new(ptr));
        s.params.push(AbiParam::new(ptr));
        s.returns.push(AbiParam::new(I64));
        s
    };
    for &f in &order {
        let sig = prog.funcs[f as usize].native.as_ref().unwrap();
        let inner = module.declare_function(&format!("phv{f}"), Linkage::Local, &inner_sig(&module, sig)).ok()?;
        let entry = module.declare_function(&format!("phv{f}_entry"), Linkage::Local, &entry_sig).ok()?;
        ids[f as usize] = Some((inner, entry));
    }

    let mut ctx = module.make_context();
    let mut fctx = FunctionBuilderContext::new();
    for &f in &order {
        let func = &prog.funcs[f as usize];
        let sig = func.native.as_ref().unwrap();
        let (inner, entry) = ids[f as usize].unwrap();
        ctx.func.signature = inner_sig(&module, sig);
        {
            let config = module.target_config();
            let mut b = FunctionBuilder::new(&mut ctx.func, &mut fctx);
            let helpers: Vec<FuncRef> = imports.iter().map(|id| module.declare_func_in_func(*id, b.func)).collect();
            let mut callees = HashMap::new();
            for ins in &func.code {
                if let Instr::Call { func: c, .. } = ins {
                    // (unreachable calls were not checked)
                    if let Some((id, _)) = ids.get(*c as usize).copied().flatten() {
                        callees.entry(*c).or_insert_with(|| module.declare_func_in_func(id, b.func));
                    }
                }
            }
            let t = Translator::new(&mut b, prog, func, &checked[&f], helpers, callees);
            t.translate();
            b.seal_all_blocks();
            b.finalize(config);
        }
        module.define_function(inner, &mut ctx).ok()?;
        module.clear_context(&mut ctx);

        // entry(ctx, args) -> inner(ctx, 0, args[0], args[1], ...): string
        // arguments are in the first window, from register 0.
        ctx.func.signature = entry_sig.clone();
        {
            let mut b = FunctionBuilder::new(&mut ctx.func, &mut fctx);
            let mf = MemFlagsData::trusted();
            let blk = b.create_block();
            b.append_block_params_for_function_params(blk);
            b.switch_to_block(blk);
            let (c, args) = (b.block_params(blk)[0], b.block_params(blk)[1]);
            let mut vals = vec![c, b.ins().iconst(I64, 0)];
            for (i, p) in sig.params.iter().enumerate() {
                vals.push(b.ins().load(abi(*p), mf, args, (i * 8) as i32));
            }
            let callee = module.declare_func_in_func(inner, b.func);
            let call = b.ins().call(callee, &vals);
            let mut r = b.inst_results(call)[0];
            if sig.ret == ValKind::Float {
                r = b.ins().bitcast(I64, MemFlagsData::new(), r);
            }
            b.ins().return_(&[r]);
            b.seal_all_blocks();
            b.finalize(module.target_config());
        }
        module.define_function(entry, &mut ctx).ok()?;
        module.clear_context(&mut ctx);
    }
    module.finalize_definitions().ok()?;

    let mut entries: Vec<Option<JitEntry>> = vec![None; prog.funcs.len()];
    for &f in &order {
        let (_, entry) = ids[f as usize].unwrap();
        let p = module.get_finalized_function(entry);
        // SAFETY: `entry` was defined above with exactly the `JitEntry` signature.
        entries[f as usize] = Some(unsafe { std::mem::transmute::<*const u8, JitEntry>(p) });
    }
    let writes_args = prog.funcs.iter().map(writes_args).collect();
    Some(JitTable { entries, writes_args, keep_alive: Box::new(Code(module)) })
}

struct Translator<'a, 'b> {
    b: &'a mut FunctionBuilder<'b>,
    prog: &'a Program,
    func: &'a Function,
    sig: &'a NativeSig,
    checked: &'a Checked,
    mf: MemFlagsData,
    helpers: Vec<FuncRef>,
    callees: HashMap<u32, FuncRef>,
    /// Per register: an i64 variable (ints and bools) and an f64 one.
    ivars: Vec<Variable>,
    fvars: Vec<Variable>,
    /// Per register: 1 when it holds `null` (a nullable scalar), else 0.
    nvars: Vec<Variable>,
    blocks: Vec<Block>,
    ctx: Value,
}

impl<'a, 'b> Translator<'a, 'b> {
    fn new(
        b: &'a mut FunctionBuilder<'b>,
        prog: &'a Program,
        func: &'a Function,
        checked: &'a Checked,
        helpers: Vec<FuncRef>,
        callees: HashMap<u32, FuncRef>,
    ) -> Translator<'a, 'b> {
        Translator {
            b,
            prog,
            func,
            sig: func.native.as_ref().unwrap(),
            checked,
            mf: MemFlagsData::trusted(),
            helpers,
            callees,
            ivars: Vec::new(),
            fvars: Vec::new(),
            nvars: Vec::new(),
            blocks: Vec::new(),
            ctx: Value::from_u32(0),
        }
    }

    fn get(&mut self, r: u16) -> Value {
        self.b.use_var(self.ivars[r as usize])
    }

    fn set(&mut self, r: u16, v: Value) {
        self.b.def_var(self.ivars[r as usize], v);
        self.setn(r, 0);
    }

    /// Whether register `r` holds `null` (0 or 1).
    fn getn(&mut self, r: u16) -> Value {
        self.b.use_var(self.nvars[r as usize])
    }

    fn setn(&mut self, r: u16, null: i64) {
        let v = self.iconst(null);
        self.b.def_var(self.nvars[r as usize], v);
    }

    /// Whether register `r` (of kind `k`) holds `null`, as 0 or 1.
    fn is_null(&mut self, r: Reg, k: K) -> Value {
        match k {
            K::Null => self.iconst(1),
            K::Opt(E::Str) => self.call_regs(H::IsNull, &[r as i64]).unwrap(),
            K::Opt(_) => self.getn(r),
            _ => self.iconst(0),
        }
    }

    /// Reads a nullable scalar from slot `r` into its machine register and null flag.
    fn unbox(&mut self, r: Reg, e: E) {
        if e == E::Str {
            return;
        }
        let v = self.call_regs(H::Unbox, &[r as i64, e.code()]).unwrap();
        self.check_ctx();
        let null = self.b.ins().load(I64, self.mf, self.ctx, CTX_DETAIL);
        if e == E::Float {
            let f = self.b.ins().bitcast(F64, MemFlagsData::new(), v);
            self.setf(r, f);
        } else {
            self.set(r, v);
        }
        self.b.def_var(self.nvars[r as usize], null);
    }

    fn getf(&mut self, r: u16) -> Value {
        self.b.use_var(self.fvars[r as usize])
    }

    fn setf(&mut self, r: u16, v: Value) {
        self.b.def_var(self.fvars[r as usize], v);
        self.setn(r, 0);
    }

    fn iconst(&mut self, v: i64) -> Value {
        self.b.ins().iconst(I64, v)
    }

    fn bool_val(&mut self, cond: Value) -> Value {
        self.b.ins().uextend(I64, cond)
    }

    /// Calls a helper; `ctx` is passed first except to the float functions.
    fn call(&mut self, h: H, args: &[Value]) -> Option<Value> {
        let mut vals = Vec::with_capacity(args.len() + 1);
        if !matches!(h, H::Fmod | H::Pow) {
            vals.push(self.ctx);
        }
        vals.extend_from_slice(args);
        let call = self.b.ins().call(self.helpers[h as usize], &vals);
        self.b.inst_results(call).first().copied()
    }

    /// Calls a helper with register numbers and constants as arguments.
    fn call_regs(&mut self, h: H, args: &[i64]) -> Option<Value> {
        let vals: Vec<Value> = args.iter().map(|a| self.b.ins().iconst(I64, *a)).collect();
        self.call(h, &vals)
    }

    /// Zero of the function's return type, returned when native code stops with an error.
    fn zero(&mut self) -> Value {
        if self.sig.ret == ValKind::Float {
            self.b.ins().f64const(0.0)
        } else {
            self.iconst(0)
        }
    }

    /// Stores `code` in `ctx.error` and returns. The VM drops the windows.
    fn fail(&mut self, code: Value) {
        self.b.ins().store(self.mf, code, self.ctx, CTX_ERROR);
        let zero = self.zero();
        self.b.ins().return_(&[zero]);
    }

    /// Branches to a fresh error block when `cond` is set; continues in a new block.
    fn fail_if(&mut self, cond: Value, code: i64) {
        let err = self.b.create_block();
        let cont = self.b.create_block();
        self.b.ins().brif(cond, err, &[], cont, &[]);
        self.b.switch_to_block(err);
        let c = self.iconst(code);
        self.fail(c);
        self.b.switch_to_block(cont);
    }

    /// Empties window slot `r` after a builtin call, as the interpreter moves
    /// arguments out of their registers: a copy left there would make the
    /// next change of the value (`$xs[] = ...`, `.=`) copy all of it (found by
    /// the 1.0 audit).
    fn moved_out(&mut self, r: Reg) {
        let (rv, tag, zero) = (self.iconst(r as i64), self.iconst(JIT_PUT_NULL), self.iconst(0));
        self.call(H::Put, &[rv, tag, zero]);
    }

    /// Fails with the error a helper left in `ctx.error`, if any.
    fn check_ctx(&mut self) {
        let e = self.b.ins().load(I64, self.mf, self.ctx, CTX_ERROR);
        self.check(e);
    }

    /// Element `i` (a machine value) of the list in slot `l`, into register
    /// `d` (a machine register, or slot `d` for a string).
    fn element(&mut self, d: Reg, l: Reg, i: Value, e: E) {
        let code = match e {
            E::Float => JIT_ELEM_FLOAT,
            E::Str => JIT_ELEM_STR,
            _ => JIT_ELEM_INT,
        };
        let (dv, lv, cv) = (self.iconst(d as i64), self.iconst(l as i64), self.iconst(code));
        let r = self.call(H::Index, &[dv, lv, i, cv]).unwrap();
        self.check_ctx();
        match e {
            E::Float => {
                let f = self.b.ins().bitcast(F64, MemFlagsData::new(), r);
                self.setf(d, f);
            }
            E::Str | E::Empty => {}
            _ => self.set(d, r),
        }
    }

    /// Fails with the code a helper returned, unless it is 0.
    fn check(&mut self, code: Value) {
        let err = self.b.create_block();
        let cont = self.b.create_block();
        self.b.ins().brif(code, err, &[], cont, &[]);
        self.b.switch_to_block(err);
        self.fail(code);
        self.b.switch_to_block(cont);
    }

    /// Decrements the budget and calls `phv_jit_tick` when it runs out.
    fn tick(&mut self) {
        let budget = self.b.ins().load(I64, self.mf, self.ctx, CTX_BUDGET);
        let next = self.b.ins().iadd_imm_s(budget, -1);
        self.b.ins().store(self.mf, next, self.ctx, CTX_BUDGET);
        let out = self.b.ins().icmp_imm_s(IntCC::SignedLessThanOrEqual, next, 0);
        let slow = self.b.create_block();
        let cont = self.b.create_block();
        self.b.ins().brif(out, slow, &[], cont, &[]);
        self.b.switch_to_block(slow);
        let r = self.call(H::Tick, &[]).unwrap();
        self.check(r);
        self.b.ins().jump(cont, &[]);
        self.b.switch_to_block(cont);
    }

    /// Returns `v`, after moving the string in register `s` to the caller.
    fn ret(&mut self, v: Value, s: Option<Reg>) {
        if self.checked.strings {
            self.call_regs(H::Leave, &[s.map_or(-1, |r| r as i64)]);
        }
        let d = self.b.ins().load(I64, self.mf, self.ctx, CTX_DEPTH);
        let d = self.b.ins().iadd_imm_s(d, -1);
        self.b.ins().store(self.mf, d, self.ctx, CTX_DEPTH);
        self.b.ins().return_(&[v]);
    }

    /// Stores a number or boolean in its window slot, for a string operation.
    fn spill(&mut self, r: Reg, k: K) {
        let (tag, bits) = match k {
            K::Str | K::List(_) | K::Map(..) | K::Opt(E::Str) => return,
            K::Null => (JIT_PUT_NULL, self.iconst(0)),
            K::Opt(e) => {
                let (tag, bits) = match e {
                    E::Float => {
                        let f = self.getf(r);
                        (JIT_PUT_FLOAT, self.b.ins().bitcast(I64, MemFlagsData::new(), f))
                    }
                    E::Bool => (JIT_PUT_BOOL, self.get(r)),
                    _ => (JIT_PUT_INT, self.get(r)),
                };
                let null = self.getn(r);
                let (t, n) = (self.iconst(tag), self.iconst(JIT_PUT_NULL));
                let tag = self.b.ins().select(null, n, t);
                let rv = self.iconst(r as i64);
                self.call(H::Put, &[rv, tag, bits]);
                return;
            }
            K::Float => {
                let f = self.getf(r);
                (JIT_PUT_FLOAT, self.b.ins().bitcast(I64, MemFlagsData::new(), f))
            }
            K::Bool => (JIT_PUT_BOOL, self.get(r)),
            _ => (JIT_PUT_INT, self.get(r)),
        };
        let (r, tag) = (self.iconst(r as i64), self.iconst(tag));
        self.call(H::Put, &[r, tag, bits]);
    }

    /// `a + b` (or `a - b`) with an overflow check.
    fn add_checked(&mut self, a: Value, b: Value, sub: bool) -> Value {
        let (r, of) = if sub {
            let r = self.b.ins().isub(a, b);
            // overflow iff a and b have different signs and r's sign differs from a's
            let x = self.b.ins().bxor(a, b);
            let y = self.b.ins().bxor(a, r);
            (r, self.b.ins().band(x, y))
        } else {
            let r = self.b.ins().iadd(a, b);
            // overflow iff r's sign differs from both a's and b's
            let x = self.b.ins().bxor(a, r);
            let y = self.b.ins().bxor(b, r);
            (r, self.b.ins().band(x, y))
        };
        let neg = self.b.ins().icmp_imm_s(IntCC::SignedLessThan, of, 0);
        self.fail_if(neg, JIT_ERR_OVERFLOW);
        r
    }

    fn mul_checked(&mut self, a: Value, b: Value) -> Value {
        let lo = self.b.ins().imul(a, b);
        let hi = self.b.ins().smulhi(a, b);
        let sign = self.b.ins().sshr_imm_s(lo, 63);
        let of = self.b.ins().icmp(IntCC::NotEqual, hi, sign);
        self.fail_if(of, JIT_ERR_OVERFLOW);
        lo
    }

    /// Fails with `code` when float `y` is zero (or minus zero).
    fn fail_if_zero(&mut self, y: Value, code: i64) {
        let zero = self.b.ins().f64const(0.0);
        let is_zero = self.b.ins().fcmp(FloatCC::Equal, y, zero);
        self.fail_if(is_zero, code);
    }

    fn jump(&mut self, target: u32) {
        let blk = self.blocks[target as usize];
        self.b.ins().jump(blk, &[]);
    }

    fn translate(mut self) {
        let code = &self.func.code;
        let nparams = self.sig.params.len();
        let entry = self.b.create_block();
        self.b.append_block_params_for_function_params(entry);
        self.blocks = (0..code.len()).map(|_| self.b.create_block()).collect();
        let nregs = self.func.num_regs.max(1) as usize;
        self.ivars = (0..nregs).map(|_| self.b.declare_var(I64)).collect();
        self.fvars = (0..nregs).map(|_| self.b.declare_var(F64)).collect();
        self.nvars = (0..nregs).map(|_| self.b.declare_var(I64)).collect();

        self.b.switch_to_block(entry);
        self.ctx = self.b.block_params(entry)[0];
        let args_base = self.b.block_params(entry)[1];
        let zero = self.iconst(0);
        let fzero = self.b.ins().f64const(0.0);
        for r in 0..nregs {
            let (mut i, mut f) = (zero, fzero);
            if r < nparams {
                let p = self.b.block_params(entry)[r + 2];
                match self.sig.params[r] {
                    ValKind::Float => f = p,
                    v if v.boxed() => {}
                    _ => i = p,
                }
            }
            self.b.def_var(self.ivars[r], i);
            self.b.def_var(self.fvars[r], f);
            self.b.def_var(self.nvars[r], zero);
        }
        // depth check, register window and budget
        let d = self.b.ins().load(I64, self.mf, self.ctx, CTX_DEPTH);
        let d = self.b.ins().iadd_imm_s(d, 1);
        self.b.ins().store(self.mf, d, self.ctx, CTX_DEPTH);
        let max = self.b.ins().load(I64, self.mf, self.ctx, CTX_MAX_DEPTH);
        let deep = self.b.ins().icmp(IntCC::SignedGreaterThan, d, max);
        self.fail_if(deep, JIT_ERR_DEPTH);
        // Large frames: the native stack, not only the depth, is bounded.
        let sp = self.b.ins().get_stack_pointer(I64);
        let floor = self.b.ins().load(I64, self.mf, self.ctx, CTX_STACK_LIMIT);
        let low = self.b.ins().icmp(IntCC::UnsignedLessThan, sp, floor);
        self.fail_if(low, JIT_ERR_STACK);
        if self.checked.strings {
            let mask = self.sig.params.iter().enumerate().filter(|(_, p)| p.boxed()).map(|(i, _)| 1 << i).sum();
            let (n, m) = (self.iconst(self.func.num_regs as i64), self.iconst(mask));
            let e = self.call(H::Enter, &[n, args_base, m]).unwrap();
            self.check(e);
            // Nullable scalar arguments come in the window.
            for (r, p) in self.sig.params.iter().enumerate() {
                if let ValKind::Opt(s) = p {
                    self.unbox(r as Reg, E::from(*s));
                }
            }
        }
        self.tick();
        self.jump(0);

        for (i, ins) in code.iter().enumerate() {
            self.b.switch_to_block(self.blocks[i]);
            let Some(st) = &self.checked.kinds[i] else {
                // Never reached (e.g. the `return null` after the last `return`).
                self.b.ins().trap(TrapCode::unwrap_user(1));
                continue;
            };
            let k = |r: Reg| st[r as usize];
            let next = (i + 1) as u32;
            match *ins {
                Instr::Int(d, v) => {
                    let c = self.iconst(v);
                    self.set(d, c);
                }
                Instr::Bool(d, v) => {
                    let c = self.iconst(v as i64);
                    self.set(d, c);
                }
                Instr::Const(d, c) => match self.prog.consts[c as usize] {
                    Const::Int(v) => {
                        let v = self.iconst(v);
                        self.set(d, v);
                    }
                    Const::Bool(v) => {
                        let v = self.iconst(v as i64);
                        self.set(d, v);
                    }
                    Const::Float(v) => {
                        let v = self.b.ins().f64const(v);
                        self.setf(d, v);
                    }
                    _ => {
                        self.call_regs(H::Const, &[d as i64, c as i64]);
                    }
                },
                Instr::Move(d, s) => match k(s) {
                    ks if ks.boxed() => {
                        self.call_regs(H::Move, &[d as i64, s as i64]);
                    }
                    K::Float => {
                        let v = self.getf(s);
                        self.setf(d, v);
                    }
                    K::Null | K::Opt(_) => {
                        // The value, and whether it is null (copied also to
                        // the window, where a string operation may read it).
                        let (i, f, n) = (self.get(s), self.getf(s), self.getn(s));
                        self.b.def_var(self.ivars[d as usize], i);
                        self.b.def_var(self.fvars[d as usize], f);
                        self.b.def_var(self.nvars[d as usize], n);
                        if k(s) == K::Null && self.checked.strings {
                            self.spill(d, K::Null);
                        }
                    }
                    _ => {
                        let v = self.get(s);
                        self.set(d, v);
                    }
                },
                Instr::Null(d) => {
                    let z = self.iconst(0);
                    self.b.def_var(self.ivars[d as usize], z);
                    self.setn(d, 1);
                    // A nullable string is read from the window.
                    if self.checked.strings {
                        self.spill(d, K::Null);
                    }
                }
                Instr::JumpIfNull(r, t) | Instr::JumpIfNotNull(r, t) => {
                    let null = self.is_null(r, k(r));
                    let (yes, no) = if matches!(ins, Instr::JumpIfNull(..)) { (t, next) } else { (next, t) };
                    let (yes, no) = (self.blocks[yes as usize], self.blocks[no as usize]);
                    self.b.ins().brif(null, yes, &[], no, &[]);
                    continue;
                }
                Instr::Eq(d, a, b) | Instr::Ne(d, a, b) if eq_mode(k(a), k(b)) != Some(EqMode::Plain) => {
                    let eq = match eq_mode(k(a), k(b)) {
                        Some(EqMode::BothNull) => self.iconst(1),
                        Some(EqMode::NullTest(true)) => self.is_null(a, k(a)),
                        Some(EqMode::NullTest(false)) => self.is_null(b, k(b)),
                        Some(EqMode::Opt(e)) => {
                            // Both null, or neither and the same value.
                            let (na, nb) = (self.is_null(a, k(a)), self.is_null(b, k(b)));
                            let same_null = self.b.ins().icmp(IntCC::Equal, na, nb);
                            let same_value = if e == E::Float {
                                let (x, y) = (self.getf(a), self.getf(b));
                                self.b.ins().fcmp(FloatCC::Equal, x, y)
                            } else {
                                let (x, y) = (self.get(a), self.get(b));
                                self.b.ins().icmp(IntCC::Equal, x, y)
                            };
                            let same_value = self.bool_val(same_value);
                            let either = self.b.ins().bor(na, same_value);
                            let same_null = self.bool_val(same_null);
                            self.b.ins().band(same_null, either)
                        }
                        _ => unreachable!("checked"),
                    };
                    let r = if matches!(ins, Instr::Ne(..)) { self.b.ins().bxor_imm_s(eq, 1) } else { eq };
                    self.set(d, r);
                }
                Instr::AddI(d, a, b) | Instr::SubI(d, a, b) => {
                    let (x, y) = (self.get(a), self.get(b));
                    let r = self.add_checked(x, y, matches!(ins, Instr::SubI(..)));
                    self.set(d, r);
                }
                Instr::AddIImm(d, a, n) | Instr::SubIImm(d, a, n) => {
                    let x = self.get(a);
                    let y = self.iconst(n as i64);
                    let r = self.add_checked(x, y, matches!(ins, Instr::SubIImm(..)));
                    self.set(d, r);
                }
                Instr::IncI(r, n) => {
                    let x = self.get(r);
                    let y = self.iconst(n);
                    let v = self.add_checked(x, y, false);
                    self.set(r, v);
                }
                Instr::MulI(d, a, b) => {
                    let (x, y) = (self.get(a), self.get(b));
                    let r = self.mul_checked(x, y);
                    self.set(d, r);
                }
                Instr::ModI(d, a, b) => {
                    let (x, y) = (self.get(a), self.get(b));
                    let is_zero = self.b.ins().icmp_imm_s(IntCC::Equal, y, 0);
                    self.fail_if(is_zero, JIT_ERR_DIV_ZERO);
                    // i64::MIN % -1 traps on x86; the answer is 0.
                    let is_m1 = self.b.ins().icmp_imm_s(IntCC::Equal, y, -1);
                    let one = self.iconst(1);
                    let safe = self.b.ins().select(is_m1, one, y);
                    let r = self.b.ins().srem(x, safe);
                    let zero = self.iconst(0);
                    let r = self.b.ins().select(is_m1, zero, r);
                    self.set(d, r);
                }
                Instr::NegI(d, a) => {
                    let x = self.get(a);
                    let is_min = self.b.ins().icmp_imm_s(IntCC::Equal, x, i64::MIN);
                    self.fail_if(is_min, JIT_ERR_OVERFLOW);
                    let r = self.b.ins().ineg(x);
                    self.set(d, r);
                }
                Instr::BitAnd(d, a, b) | Instr::BitOr(d, a, b) | Instr::BitXor(d, a, b) => {
                    let (x, y) = (self.get(a), self.get(b));
                    let r = match ins {
                        Instr::BitAnd(..) => self.b.ins().band(x, y),
                        Instr::BitOr(..) => self.b.ins().bor(x, y),
                        _ => self.b.ins().bxor(x, y),
                    };
                    self.set(d, r);
                }
                Instr::Shl(d, a, b) | Instr::Shr(d, a, b) => {
                    let (x, y) = (self.get(a), self.get(b));
                    let neg = self.b.ins().icmp_imm_s(IntCC::SignedLessThan, y, 0);
                    self.fail_if(neg, JIT_ERR_SHIFT);
                    let big = self.b.ins().icmp_imm_s(IntCC::SignedGreaterThanOrEqual, y, 64);
                    let (shifted, overflowed) = if matches!(ins, Instr::Shl(..)) {
                        (self.b.ins().ishl(x, y), self.iconst(0))
                    } else {
                        (self.b.ins().sshr(x, y), self.b.ins().sshr_imm_s(x, 63))
                    };
                    let r = self.b.ins().select(big, overflowed, shifted);
                    self.set(d, r);
                }
                Instr::BitNot(d, a) => {
                    let x = self.get(a);
                    let r = self.b.ins().bnot(x);
                    self.set(d, r);
                }
                Instr::Not(d, a) => {
                    let x = self.get(a);
                    let r = self.b.ins().bxor_imm_s(x, 1);
                    self.set(d, r);
                }
                Instr::Eq(d, a, b)
                | Instr::Ne(d, a, b)
                | Instr::Lt(d, a, b)
                | Instr::Le(d, a, b)
                | Instr::Cmp(d, a, b)
                    if k(a).strip() == K::Str =>
                {
                    // Compare the strings (-1, 0 or 1), then that with 0.
                    let c = self.call_regs(H::StrCmp, &[a as i64, b as i64]).unwrap();
                    let r = match ins {
                        Instr::Cmp(..) => c,
                        _ => {
                            let cc = match ins {
                                Instr::Eq(..) => IntCC::Equal,
                                Instr::Ne(..) => IntCC::NotEqual,
                                Instr::Lt(..) => IntCC::SignedLessThan,
                                _ => IntCC::SignedLessThanOrEqual,
                            };
                            let c = self.b.ins().icmp_imm_s(cc, c, 0);
                            self.bool_val(c)
                        }
                    };
                    self.set(d, r);
                }
                Instr::EqI(d, a, b)
                | Instr::Eq(d, a, b)
                | Instr::Ne(d, a, b)
                | Instr::LtI(d, a, b)
                | Instr::Lt(d, a, b)
                | Instr::LeI(d, a, b)
                | Instr::Le(d, a, b) => {
                    let cc = match ins {
                        Instr::EqI(..) | Instr::Eq(..) => IntCC::Equal,
                        Instr::Ne(..) => IntCC::NotEqual,
                        Instr::LtI(..) | Instr::Lt(..) => IntCC::SignedLessThan,
                        _ => IntCC::SignedLessThanOrEqual,
                    };
                    let (x, y) = (self.get(a), self.get(b));
                    let c = self.b.ins().icmp(cc, x, y);
                    let r = self.bool_val(c);
                    self.set(d, r);
                }
                Instr::Cmp(d, a, b) => {
                    let (x, y) = (self.get(a), self.get(b));
                    let gt = self.b.ins().icmp(IntCC::SignedGreaterThan, x, y);
                    let lt = self.b.ins().icmp(IntCC::SignedLessThan, x, y);
                    let gt = self.bool_val(gt);
                    let lt = self.bool_val(lt);
                    let r = self.b.ins().isub(gt, lt);
                    self.set(d, r);
                }
                Instr::AddF(d, a, b) | Instr::SubF(d, a, b) | Instr::MulF(d, a, b) | Instr::DivF(d, a, b) => {
                    let (x, y) = (self.getf(a), self.getf(b));
                    let r = match ins {
                        Instr::AddF(..) => self.b.ins().fadd(x, y),
                        Instr::SubF(..) => self.b.ins().fsub(x, y),
                        Instr::MulF(..) => self.b.ins().fmul(x, y),
                        _ => {
                            self.fail_if_zero(y, JIT_ERR_FDIV_ZERO);
                            self.b.ins().fdiv(x, y)
                        }
                    };
                    self.setf(d, r);
                }
                Instr::ModF(d, a, b) | Instr::PowF(d, a, b) => {
                    let (x, y) = (self.getf(a), self.getf(b));
                    let h = if matches!(ins, Instr::ModF(..)) {
                        self.fail_if_zero(y, JIT_ERR_DIV_ZERO);
                        H::Fmod
                    } else {
                        H::Pow
                    };
                    let r = self.call(h, &[x, y]).unwrap();
                    self.setf(d, r);
                }
                Instr::NegF(d, a) => {
                    let x = self.getf(a);
                    let r = self.b.ins().fneg(x);
                    self.setf(d, r);
                }
                Instr::EqF(d, a, b) | Instr::NeF(d, a, b) | Instr::LtF(d, a, b) | Instr::LeF(d, a, b) => {
                    // As in the interpreter, NaN compares equal for `<=` (and `<=>`).
                    let cc = match ins {
                        Instr::EqF(..) => FloatCC::Equal,
                        Instr::NeF(..) => FloatCC::NotEqual,
                        Instr::LtF(..) => FloatCC::LessThan,
                        _ => FloatCC::UnorderedOrLessThanOrEqual,
                    };
                    let (x, y) = (self.getf(a), self.getf(b));
                    let c = self.b.ins().fcmp(cc, x, y);
                    let r = self.bool_val(c);
                    self.set(d, r);
                }
                Instr::CmpF(d, a, b) => {
                    let (x, y) = (self.getf(a), self.getf(b));
                    let gt = self.b.ins().fcmp(FloatCC::GreaterThan, x, y);
                    let lt = self.b.ins().fcmp(FloatCC::LessThan, x, y);
                    let gt = self.bool_val(gt);
                    let lt = self.bool_val(lt);
                    let r = self.b.ins().isub(gt, lt);
                    self.set(d, r);
                }
                Instr::IntToFloat(d, a) => {
                    let x = self.get(a);
                    let r = self.b.ins().fcvt_from_sint(F64, x);
                    self.setf(d, r);
                }
                Instr::FloatToInt(d, a) => {
                    // In range: -2^63 <= x < 2^63 (false for NaN).
                    let x = self.getf(a);
                    let lo = self.b.ins().f64const(i64::MIN as f64);
                    let hi = self.b.ins().f64const(-(i64::MIN as f64));
                    let above = self.b.ins().fcmp(FloatCC::GreaterThanOrEqual, x, lo);
                    let below = self.b.ins().fcmp(FloatCC::LessThan, x, hi);
                    let in_range = self.b.ins().band(above, below);
                    let err = self.b.create_block();
                    let cont = self.b.create_block();
                    self.b.ins().brif(in_range, cont, &[], err, &[]);
                    self.b.switch_to_block(err);
                    let bits = self.b.ins().bitcast(I64, MemFlagsData::new(), x);
                    self.b.ins().store(self.mf, bits, self.ctx, CTX_DETAIL);
                    let c = self.iconst(JIT_ERR_FLOAT_TO_INT);
                    self.fail(c);
                    self.b.switch_to_block(cont);
                    let r = self.b.ins().fcvt_to_sint_sat(I64, x);
                    self.set(d, r);
                }
                Instr::ToStr(d, a) => {
                    self.spill(a, k(a));
                    self.call_regs(H::ToStr, &[d as i64, a as i64]);
                }
                Instr::Concat(d, start, n, _) => {
                    for r in start..start + n {
                        self.spill(r, k(r));
                    }
                    let e = self.call_regs(H::Concat, &[d as i64, start as i64, n as i64]).unwrap();
                    self.check(e);
                }
                Instr::AppendStr(d, s) => {
                    self.spill(s, k(s));
                    let e = self.call_regs(H::Append, &[d as i64, s as i64]).unwrap();
                    self.check(e);
                }
                Instr::CallBuiltin { dst, id, args, .. } if self.prog.builtin_names[id as usize] == "range" => {
                    let (x, y) = (self.get(args), self.get(args + 1));
                    let dv = self.iconst(dst as i64);
                    self.call(H::RangeFn, &[dv, x, y]);
                    self.check_ctx();
                }
                Instr::CallBuiltin { dst, id, args, .. } if self.prog.builtin_names[id as usize] == "map_has" => {
                    self.spill(args + 1, k(args + 1));
                    let r = self.call_regs(H::Has, &[args as i64, args as i64 + 1]).unwrap();
                    self.check_ctx();
                    self.moved_out(args);
                    self.moved_out(args + 1);
                    self.set(dst, r);
                }
                Instr::CallBuiltin { dst, args, .. } if matches!(k(args), K::List(_) | K::Map(..)) => {
                    let lv = self.iconst(args as i64);
                    let n = self.call(H::ListLen, &[lv]).unwrap();
                    self.check_ctx();
                    self.moved_out(args);
                    self.set(dst, n);
                }
                Instr::CallBuiltin { dst, id, args, .. } => {
                    // `strlen` or `str_len`: the length, or minus an error code.
                    let chars = self.prog.builtin_names[id as usize] == "str_len";
                    let n = self.call_regs(H::Strlen, &[args as i64, chars as i64]).unwrap();
                    let failed = self.b.ins().icmp_imm_s(IntCC::SignedLessThan, n, 0);
                    let err = self.b.create_block();
                    let cont = self.b.create_block();
                    self.b.ins().brif(failed, err, &[], cont, &[]);
                    self.b.switch_to_block(err);
                    let code = self.b.ins().ineg(n);
                    self.fail(code);
                    self.b.switch_to_block(cont);
                    self.moved_out(args);
                    self.set(dst, n);
                }
                Instr::Jump(t) => {
                    if t as usize <= i {
                        self.tick();
                    }
                    self.jump(t);
                    continue;
                }
                Instr::JumpIfFalse(r, t) | Instr::JumpIfTrue(r, t) => {
                    let c = self.get(r);
                    let (yes, no) = if matches!(ins, Instr::JumpIfTrue(..)) { (t, next) } else { (next, t) };
                    let (yes, no) = (self.blocks[yes as usize], self.blocks[no as usize]);
                    self.b.ins().brif(c, yes, &[], no, &[]);
                    continue;
                }
                Instr::JumpIfNotLtI(a, b, t) | Instr::JumpIfNotLeI(a, b, t) => {
                    let (x, y) = (self.get(a), self.get(b));
                    let cc = if matches!(ins, Instr::JumpIfNotLtI(..)) {
                        IntCC::SignedLessThan
                    } else {
                        IntCC::SignedLessThanOrEqual
                    };
                    let c = self.b.ins().icmp(cc, x, y);
                    let (yes, no) = (self.blocks[next as usize], self.blocks[t as usize]);
                    self.b.ins().brif(c, yes, &[], no, &[]);
                    continue;
                }
                Instr::JumpIfNotLtIImm(a, n, t) | Instr::JumpIfNotLeIImm(a, n, t) => {
                    let x = self.get(a);
                    let cc = if matches!(ins, Instr::JumpIfNotLtIImm(..)) {
                        IntCC::SignedLessThan
                    } else {
                        IntCC::SignedLessThanOrEqual
                    };
                    let c = self.b.ins().icmp_imm_s(cc, x, n as i64);
                    let (yes, no) = (self.blocks[next as usize], self.blocks[t as usize]);
                    self.b.ins().brif(c, yes, &[], no, &[]);
                    continue;
                }
                Instr::Call { dst, func, args, argc } => {
                    let callee = self.callees[&func];
                    let csig = self.prog.funcs[func as usize].native.as_ref().unwrap();
                    let mut vals = vec![self.ctx, self.iconst(args as i64)];
                    for (i, r) in (args..args + argc).enumerate() {
                        let p = csig.params[i];
                        let v = if p.boxed() {
                            // Moved from the window by the callee.
                            self.spill(r, k(r));
                            self.iconst(0)
                        } else if p == ValKind::Float {
                            self.getf(r)
                        } else {
                            self.get(r)
                        };
                        vals.push(v);
                    }
                    let call = self.b.ins().call(callee, &vals);
                    let r = self.b.inst_results(call)[0];
                    let e = self.b.ins().load(I64, self.mf, self.ctx, CTX_ERROR);
                    self.check(e);
                    match csig.ret {
                        ValKind::Float => self.setf(dst, r),
                        v if v.boxed() => {
                            self.call_regs(H::TakeRet, &[dst as i64]);
                            if let ValKind::Opt(s) = v {
                                self.unbox(dst, E::from(s));
                            }
                        }
                        _ => self.set(dst, r),
                    }
                }
                Instr::Ret(r) => {
                    match self.sig.ret {
                        ValKind::Float => {
                            let v = self.getf(r);
                            self.ret(v, None);
                        }
                        v if v.boxed() => {
                            // A nullable scalar goes back in the window.
                            self.spill(r, k(r));
                            let z = self.iconst(0);
                            self.ret(z, Some(r));
                        }
                        ValKind::Void => {
                            let z = self.iconst(0);
                            self.ret(z, None);
                        }
                        _ => {
                            let v = self.get(r);
                            self.ret(v, None);
                        }
                    }
                    continue;
                }
                Instr::RetNull => {
                    let z = self.zero();
                    self.ret(z, None);
                    continue;
                }
                Instr::PowI(d, a, b) => {
                    let (x, y) = (self.get(a), self.get(b));
                    let r = self.call(H::PowI, &[x, y]).unwrap();
                    self.check_ctx();
                    self.set(d, r);
                }
                Instr::NewList(d, start, n) => {
                    for r in start..start + n {
                        self.spill(r, k(r));
                    }
                    self.call_regs(H::NewList, &[d as i64, start as i64, n as i64]);
                }
                Instr::Index(d, l, i) if matches!(k(l), K::List(_)) => {
                    let K::List(e) = k(l) else { unreachable!("checked") };
                    let iv = self.get(i);
                    self.element(d, l, iv, e);
                }
                Instr::Index(d, c, key) | Instr::IndexOrNull(d, c, key) => {
                    let or_null = matches!(ins, Instr::IndexOrNull(..));
                    let e = match k(c) {
                        K::List(e) => e,
                        K::Map(_, v) => v,
                        _ => unreachable!("checked"),
                    };
                    self.spill(key, k(key));
                    let args = [d as i64, c as i64, key as i64, e.code(), or_null as i64];
                    let v = self.call_regs(H::Get, &args).unwrap();
                    self.check_ctx();
                    match e {
                        E::Float => {
                            let f = self.b.ins().bitcast(F64, MemFlagsData::new(), v);
                            self.setf(d, f);
                        }
                        E::Str => {}
                        _ => self.set(d, v),
                    }
                    if or_null {
                        let null = self.b.ins().load(I64, self.mf, self.ctx, CTX_DETAIL);
                        let null = if e == E::Empty { self.iconst(1) } else { null };
                        self.b.def_var(self.nvars[d as usize], null);
                    }
                }
                Instr::NewMap(d, start, n) => {
                    for r in start..start + 2 * n {
                        self.spill(r, k(r));
                    }
                    self.call_regs(H::NewMap, &[d as i64, start as i64, n as i64]);
                }
                Instr::SetPath(ref path) if matches!(path.base, PathBase::Local(r) if matches!(k(r), K::Map(..))) => {
                    let PathBase::Local(r) = path.base else { unreachable!("checked") };
                    let key = path.keys[0];
                    self.spill(key, k(key));
                    self.spill(path.val, k(path.val));
                    self.call_regs(H::SetKey, &[r as i64, key as i64, path.val as i64]);
                    self.check_ctx();
                }
                Instr::SetPath(ref path) if path.push && k(path.val) != K::Str => {
                    let PathBase::Local(r) = path.base else { unreachable!("checked") };
                    let (tag, bits) = match k(path.val) {
                        K::Float => {
                            let f = self.getf(path.val);
                            (JIT_PUT_FLOAT, self.b.ins().bitcast(I64, MemFlagsData::new(), f))
                        }
                        K::Bool => (JIT_PUT_BOOL, self.get(path.val)),
                        _ => (JIT_PUT_INT, self.get(path.val)),
                    };
                    let (rv, tv) = (self.iconst(r as i64), self.iconst(tag));
                    self.call(H::Push, &[rv, tv, bits]);
                    self.check_ctx();
                }
                Instr::SetPath(ref path) => {
                    let PathBase::Local(r) = path.base else { unreachable!("checked") };
                    self.spill(path.val, k(path.val));
                    let idx = match path.keys.first() {
                        Some(&key) => self.get(key),
                        None => self.iconst(0),
                    };
                    let (rv, pv, vv) =
                        (self.iconst(r as i64), self.iconst(path.push as i64), self.iconst(path.val as i64));
                    self.call(H::SetIndex, &[rv, idx, pv, vv]);
                    self.check_ctx();
                }
                Instr::IterInit(it, src) => {
                    self.call_regs(H::Move, &[it as i64, src as i64]);
                    let z = self.iconst(0);
                    self.set(it + 1, z);
                    // The loop's own copy of the list does not change: its
                    // length is kept in the (otherwise unused) i64 of `it`.
                    let lv = self.iconst(it as i64);
                    let len = self.call(H::ListLen, &[lv]).unwrap();
                    self.check_ctx();
                    self.set(it, len);
                }
                Instr::IterNext { it, key, val, end } if matches!(k(it), K::Map(..)) => {
                    let K::Map(ke, ve) = k(it) else { unreachable!("checked") };
                    let len = self.get(it);
                    let idx = self.get(it + 1);
                    let done = self.b.ins().icmp(IntCC::SignedGreaterThanOrEqual, idx, len);
                    let cont = self.b.create_block();
                    self.b.ins().brif(done, self.blocks[end as usize], &[], cont, &[]);
                    self.b.switch_to_block(cont);
                    for (r, part, e) in [(key, 0, ke), (val, 1, ve)] {
                        if r == NO_REG || e == E::Empty {
                            continue;
                        }
                        let (rv, mv, pv, cv) =
                            (self.iconst(r as i64), self.iconst(it as i64), self.iconst(part), self.iconst(e.code()));
                        let v = self.call(H::Entry, &[rv, mv, idx, pv, cv]).unwrap();
                        self.check_ctx();
                        match e {
                            E::Float => {
                                let f = self.b.ins().bitcast(F64, MemFlagsData::new(), v);
                                self.setf(r, f);
                            }
                            E::Str => {}
                            _ => self.set(r, v),
                        }
                    }
                    let n = self.b.ins().iadd_imm_s(idx, 1);
                    self.set(it + 1, n);
                }
                Instr::IterNext { it, key, val, end } => {
                    let K::List(e) = k(it) else { unreachable!("checked") };
                    let len = self.get(it);
                    let idx = self.get(it + 1);
                    let done = self.b.ins().icmp(IntCC::SignedGreaterThanOrEqual, idx, len);
                    let cont = self.b.create_block();
                    self.b.ins().brif(done, self.blocks[end as usize], &[], cont, &[]);
                    self.b.switch_to_block(cont);
                    if key != NO_REG {
                        self.set(key, idx);
                    }
                    if e != E::Empty {
                        self.element(val, it, idx, e);
                    }
                    let n = self.b.ins().iadd_imm_s(idx, 1);
                    self.set(it + 1, n);
                }
                Instr::Range(d, a, b, inclusive) => {
                    let (x, y) = (self.get(a), self.get(b));
                    let (dv, iv) = (self.iconst(d as i64), self.iconst(inclusive as i64));
                    self.call(H::Range, &[dv, x, y, iv]);
                    self.check_ctx();
                }
                _ => unreachable!("unsupported instruction reached the JIT"),
            }
            // `well_formed` checked that the last instruction returns.
            self.jump(next);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phv_vm::bytecode::Const as C;
    use phv_vm::bytecode::Instr::*;

    fn func(params: Vec<ValKind>, ret: ValKind, num_regs: u16, code: Vec<Instr>) -> Function {
        Function {
            locals: Vec::new(),
            native: Some(NativeSig { params: params.clone(), ret }),
            num_params: params.len() as u16,
            num_regs,
            offsets: vec![0; code.len()],
            code,
            ..Default::default()
        }
    }

    fn accepts(prog: &Program) -> bool {
        let all: HashSet<u32> = (0..prog.funcs.len() as u32).collect();
        well_formed(prog, &prog.funcs[0], &all).is_some()
    }

    fn one(f: Function) -> bool {
        accepts(&Program { funcs: vec![f], ..Default::default() })
    }

    use ValKind::{Float as F, Int as I, Str as S};

    #[test]
    fn accepts_float_and_string_functions() {
        assert!(one(func(vec![F, F], F, 3, vec![AddF(2, 0, 1), DivF(2, 2, 1), Ret(2)])));
        assert!(one(func(vec![F, F], ValKind::Bool, 3, vec![LeF(2, 0, 1), Ret(2)])));
        assert!(one(func(vec![S, I], S, 4, vec![Move(2, 0), Move(3, 1), Concat(2, 2, 2, false), Ret(2)])));
        let prog = Program {
            funcs: vec![func(vec![S], I, 3, vec![Move(1, 0), CallBuiltin { dst: 2, id: 0, args: 1, argc: 1 }, Ret(2)])],
            builtin_names: vec!["strlen".into()],
            ..Default::default()
        };
        assert!(accepts(&prog));
    }

    #[test]
    fn checks_the_kinds_of_lists() {
        use phv_vm::bytecode::PathSet;
        let (li, ls) = (ValKind::List(Scalar::Int), ValKind::List(Scalar::Str));
        let push = |list: u16, val: u16| {
            SetPath(Box::new(PathSet {
                base: PathBase::Local(list),
                keys: Vec::new(),
                create: Vec::new(),
                push: true,
                val,
            }))
        };
        // Reading an element gives the list's element kind.
        assert!(one(func(vec![li, I], I, 3, vec![Index(2, 0, 1), Ret(2)])));
        assert!(!one(func(vec![li, I], F, 3, vec![Index(2, 0, 1), Ret(2)])));
        assert!(!one(func(vec![ls, I], I, 3, vec![Index(2, 0, 1), Ret(2)])));
        // The index is an int; a string is not a list.
        assert!(!one(func(vec![li, F], I, 3, vec![Index(2, 0, 1), Ret(2)])));
        assert!(!one(func(vec![S, I], I, 3, vec![Index(2, 0, 1), Ret(2)])));
        // `[]` takes the kind of what is appended; another kind cannot follow.
        assert!(one(func(vec![I], li, 2, vec![NewList(1, 0, 0), push(1, 0), Ret(1)])));
        assert!(one(func(vec![], li, 1, vec![NewList(0, 0, 0), Ret(0)])));
        assert!(!one(func(vec![li, F], li, 2, vec![push(0, 1), Ret(0)])));
        // Elements of a list known to be empty are never read natively.
        assert!(!one(func(vec![I], I, 3, vec![NewList(1, 0, 0), Index(2, 1, 0), Ret(2)])));
        // Lists are not compared natively.
        assert!(!one(func(vec![li, li], ValKind::Bool, 3, vec![Eq(2, 0, 1), Ret(2)])));
        // A list function returns a list on every path.
        assert!(!one(func(vec![], li, 1, vec![RetNull])));
    }

    #[test]
    fn checks_nullable_values_and_maps() {
        let oi = ValKind::Opt(Scalar::Int);
        let msi = ValKind::Map(Scalar::Str, Scalar::Int);
        // `null` or an int: a nullable int, returned as one.
        let code = vec![JumpIfFalse(0, 3), Int(1, 5), Jump(4), Null(1), Ret(1)];
        assert!(one(func(vec![ValKind::Bool], oi, 2, code)));
        // ... but not returned as an int, nor joined with a float.
        let code = vec![JumpIfFalse(0, 3), Int(1, 5), Jump(4), Null(1), Ret(1)];
        assert!(one(func(vec![ValKind::Bool], I, 2, code)), "a nullable int is used where the checker proved an int");
        let code = vec![JumpIfFalse(0, 3), Int(1, 5), Jump(4), Instr::Const(1, 0), Ret(1)];
        let prog = Program {
            funcs: vec![func(vec![ValKind::Bool], oi, 2, code)],
            consts: vec![C::Float(1.0)],
            ..Default::default()
        };
        assert!(!accepts(&prog));
        // A map's values have its value kind; its keys its key kind.
        assert!(one(func(vec![msi, S], I, 3, vec![Index(2, 0, 1), Ret(2)])));
        assert!(!one(func(vec![msi, I], I, 3, vec![Index(2, 0, 1), Ret(2)])));
        assert!(one(func(vec![msi, S], oi, 3, vec![IndexOrNull(2, 0, 1), Ret(2)])));
        assert!(!one(func(vec![msi, S], S, 3, vec![Index(2, 0, 1), Ret(2)])));
        // Maps are not compared natively.
        assert!(!one(func(vec![msi, msi], ValKind::Bool, 3, vec![Eq(2, 0, 1), Ret(2)])));
    }

    #[test]
    fn rejects_registers_of_the_wrong_kind() {
        // float used as an int, int used as a float
        assert!(!one(func(vec![F, I], I, 3, vec![AddI(2, 0, 1), Ret(2)])));
        assert!(!one(func(vec![F, I], F, 3, vec![AddF(2, 0, 1), Ret(2)])));
        // the generic comparisons are not compiled for floats
        assert!(!one(func(vec![F, F], ValKind::Bool, 3, vec![Lt(2, 0, 1), Ret(2)])));
        // a string as a number, a number appended to
        assert!(!one(func(vec![S], I, 2, vec![NegI(1, 0), Ret(1)])));
        assert!(!one(func(vec![I, S], ValKind::Void, 2, vec![AppendStr(0, 1), RetNull])));
        // returning the wrong kind
        assert!(!one(func(vec![I], S, 1, vec![Ret(0)])));
        assert!(!one(func(vec![S], F, 1, vec![Ret(0)])));
        // float to int of an int
        assert!(!one(func(vec![I], I, 2, vec![FloatToInt(1, 0), Ret(1)])));
    }

    #[test]
    fn rejects_reads_of_unwritten_or_mixed_registers() {
        // register 1 is never written
        assert!(!one(func(vec![F], F, 2, vec![AddF(0, 0, 1), Ret(0)])));
        // register 2 is a float on one path and a string on the other
        let code = vec![JumpIfFalse(0, 3), Instr::Const(2, 0), Jump(4), Instr::Const(2, 1), ToStr(1, 2), Ret(1)];
        let mut prog = Program {
            funcs: vec![func(vec![ValKind::Bool], S, 3, code)],
            consts: vec![C::Float(1.5), C::Str("x".into())],
            ..Default::default()
        };
        assert!(!accepts(&prog));
        // the same with two strings is fine
        prog.consts[0] = C::Str("y".into());
        assert!(accepts(&prog));
    }

    #[test]
    fn rejects_out_of_range_operands() {
        assert!(!one(func(vec![S], S, 2, vec![Concat(1, 0, 3, false), Ret(1)])));
        assert!(!one(func(vec![S], S, 2, vec![Move(5, 0), Ret(0)])));
        // missing constant, and a constant that is not a scalar or string
        assert!(!one(func(vec![], S, 1, vec![Instr::Const(0, 0), Ret(0)])));
        let prog = Program {
            funcs: vec![func(vec![], S, 1, vec![Instr::Const(0, 0), Ret(0)])],
            consts: vec![C::EmptyList],
            ..Default::default()
        };
        assert!(!accepts(&prog));
        // jump past the end
        assert!(!one(func(vec![ValKind::Bool], ValKind::Void, 1, vec![JumpIfTrue(0, 9), RetNull])));
    }

    #[test]
    fn rejects_bad_calls_and_returns() {
        // a string function that can return without a value
        assert!(!one(func(vec![ValKind::Bool, S], S, 2, vec![JumpIfFalse(0, 2), Ret(1), RetNull])));
        // HTML concatenation and builtins other than strlen
        assert!(!one(func(vec![S], S, 2, vec![Concat(1, 0, 1, true), Ret(1)])));
        // `Concat` of no registers, and a branch back (which would not check the clock).
        assert!(!one(func(vec![S], S, 2, vec![Concat(1, 9, 0, false), Ret(1)])));
        assert!(!one(func(vec![I], ValKind::Void, 1, vec![JumpIfNotLtIImm(0, 5, 0), RetNull])));
        let prog = Program {
            funcs: vec![func(vec![S], I, 2, vec![CallBuiltin { dst: 1, id: 0, args: 0, argc: 1 }, Ret(1)])],
            builtin_names: vec!["strtoupper".into()],
            ..Default::default()
        };
        assert!(!accepts(&prog));
        // a float passed for an int parameter; a string argument read after the call moved it
        let callee = func(vec![I], I, 1, vec![Ret(0)]);
        let caller = func(vec![F], I, 2, vec![Call { dst: 1, func: 1, args: 0, argc: 1 }, Ret(1)]);
        assert!(!accepts(&Program { funcs: vec![caller, callee], ..Default::default() }));
        let callee = func(vec![S], I, 1, vec![Int(0, 1), Ret(0)]);
        let caller = func(vec![S], S, 2, vec![Call { dst: 1, func: 1, args: 0, argc: 1 }, Ret(0)]);
        assert!(!accepts(&Program { funcs: vec![caller, callee], ..Default::default() }));
        // a callee that is not compiled
        let callee = func(vec![I], I, 1, vec![Ret(0)]);
        let caller = func(vec![I], I, 2, vec![Call { dst: 1, func: 1, args: 0, argc: 1 }, Ret(1)]);
        let prog = Program { funcs: vec![caller, callee], ..Default::default() };
        assert!(well_formed(&prog, &prog.funcs[0], &HashSet::from([0])).is_none());
    }

    #[test]
    fn rejects_bad_signatures() {
        let mut f = func(vec![S], S, 1, vec![Ret(0)]);
        f.num_params = 2;
        assert!(!one(f));
        assert!(!one(func(vec![ValKind::Void], ValKind::Void, 1, vec![RetNull])));
        assert!(!one(func(vec![I; 17], I, 17, vec![Ret(0)])));
    }
}
