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
use phv_vm::bytecode::{
    Class, Const, Field, Function, Instr, NativeSig, PathBase, Program, Reg, Scalar, ValKind, NO_REG,
};
use phv_vm::jit::*;
use phv_vm::{Installed, JitEntry, JitTable};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const I64: Type = types::I64;
const F64: Type = types::F64;
// Offsets of the `JitCtx` fields.
const CTX_ERROR: i32 = 0;
const CTX_DEPTH: i32 = 8;
const CTX_MAX_DEPTH: i32 = 16;
const CTX_BUDGET: i32 = 24;
const CTX_DETAIL: i32 = 40;
const CTX_STACK_LIMIT: i32 = 56;
const CTX_FUNCS: i32 = 80;
const _: () = {
    use std::mem::offset_of;
    assert!(offset_of!(phv_vm::jit::JitCtx, error) == CTX_ERROR as usize);
    assert!(offset_of!(phv_vm::jit::JitCtx, depth) == CTX_DEPTH as usize);
    assert!(offset_of!(phv_vm::jit::JitCtx, max_depth) == CTX_MAX_DEPTH as usize);
    assert!(offset_of!(phv_vm::jit::JitCtx, budget) == CTX_BUDGET as usize);
    assert!(offset_of!(phv_vm::jit::JitCtx, detail) == CTX_DETAIL as usize);
    assert!(offset_of!(phv_vm::jit::JitCtx, stack_limit) == CTX_STACK_LIMIT as usize);
    assert!(offset_of!(phv_vm::jit::JitCtx, funcs) == CTX_FUNCS as usize);
};

/// Owns a unit's executable memory, freed when the unit is dropped.
struct Code(Option<JITModule>);
// SAFETY: after `finalize_definitions` the module is never used again except
// to be dropped; the code it owns is immutable and safe to call from any thread.
unsafe impl Send for Code {}
unsafe impl Sync for Code {}

impl Drop for Code {
    fn drop(&mut self) {
        if let Some(module) = self.0.take() {
            // SAFETY: every entry into this code is an `Installed` holding
            // the unit (see `compile_root`), and the unit is dropped only
            // when the last of them is: nothing can run or call it any more.
            unsafe { module.free_memory() };
        }
    }
}

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
    /// An object of the class (by id in the program being compiled) or of a
    /// subclass (in the window).
    Obj(u32),
    /// The same, or `null`.
    OptObj(u32),
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
            ValKind::Object(c) => Some(K::Obj(c)),
            ValKind::OptObject(c) => Some(K::OptObj(c)),
            ValKind::Void => None,
        }
    }

    /// Kept in the register window, not in a machine register.
    fn boxed(self) -> bool {
        matches!(self, K::Str | K::List(_) | K::Map(..) | K::Opt(E::Str) | K::Obj(_) | K::OptObj(_))
    }

    /// The class of an object, nullable or not (the type checker proved
    /// that it is not null where it is used as an object).
    fn class(self) -> Option<u32> {
        match self {
            K::Obj(c) | K::OptObj(c) => Some(c),
            _ => None,
        }
    }

    /// The `JIT_ELEM_*` a helper delivers a value of this kind as, and
    /// whether it may be null.
    fn elem(self) -> Option<(i64, bool)> {
        Some(match self {
            K::Int | K::Bool => (JIT_ELEM_INT, false),
            K::Float => (JIT_ELEM_FLOAT, false),
            K::Str => (JIT_ELEM_STR, false),
            K::List(_) => (JIT_ELEM_LIST, false),
            K::Map(..) => (JIT_ELEM_MAP, false),
            K::Obj(_) => (JIT_ELEM_OBJ, false),
            K::OptObj(_) => (JIT_ELEM_OBJ, true),
            K::Opt(E::Empty) => return None,
            K::Opt(e) => (e.code(), true),
            _ => return None,
        })
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
            K::OptObj(c) => K::Obj(c),
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
            // An object meets `null` (or the nullable form of its class).
            (K::Null | K::OptObj(_), K::Obj(c) | K::OptObj(c)) | (K::Obj(c) | K::OptObj(c), K::Null | K::OptObj(_))
                if self.class().is_none_or(|a| a == c) && other.class().is_none_or(|b| b == c) =>
            {
                Some(K::OptObj(c))
            }
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
/// proved that it is not null there); an object of a class where one of a
/// class it extends (or implements) is needed.
fn fits(prog: &Program, have: K, want: K) -> bool {
    have == want
        || match (have, want) {
            (K::Null, K::OptObj(_)) => true,
            (K::Obj(a) | K::OptObj(a), K::Obj(b) | K::OptObj(b)) => extends(prog, a, b),
            (K::List(E::Empty), K::List(_)) | (K::Map(E::Empty, E::Empty), K::Map(..)) | (K::Null, K::Opt(_)) => true,
            (K::Map(k1, v1), K::Map(k2, v2)) => E::join(k1, k2) == Some(k2) && E::join(v1, v2) == Some(v2),
            (x, K::Opt(e)) => x.base().is_some_and(|(b, _)| b == e),
            (K::Opt(e), x) => x.base() == Some((e, false)),
            _ => false,
        }
}

/// `K::join`, where objects of two classes meet as the nearest class both
/// extend (`$p = $c ? new A() : new B()`, the type checker's type of `$p`).
fn join(prog: &Program, a: K, b: K) -> K {
    let common = match (a.class(), b.class()) {
        (Some(x), Some(y)) if x != y => {
            let mut c = Some(x);
            // The parents of `x`, nearest first, until one `y` extends.
            for _ in 0..prog.classes.len() {
                match c {
                    Some(p) if extends(prog, y, p) => break,
                    Some(p) => c = prog.classes.get(p as usize).and_then(|k| k.parent),
                    None => break,
                }
            }
            c.filter(|&p| extends(prog, x, p) && extends(prog, y, p))
        }
        _ => None,
    };
    match common {
        Some(c) => {
            let nullable = matches!(a, K::OptObj(_)) || matches!(b, K::OptObj(_));
            if nullable {
                K::OptObj(c)
            } else {
                K::Obj(c)
            }
        }
        None => a.join(b),
    }
}

/// Whether class `a` is class `b`, extends it or implements it (both in range).
fn extends(prog: &Program, a: u32, b: u32) -> bool {
    match (prog.classes.get(a as usize), prog.classes.get(b as usize)) {
        (Some(c), Some(_)) => a == b || c.supers.binary_search(&b).is_ok(),
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
        (K::Null, x) => (x.base().is_some() || x.class().is_some()).then_some(EqMode::NullTest(false)),
        (x, K::Null) => (x.base().is_some() || x.class().is_some()).then_some(EqMode::NullTest(true)),
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
    /// By instruction: the registers it makes a number or a boolean whose
    /// window slot may still hold a string, list, map or object (see
    /// `stale_slots`). Native code empties those slots after it.
    stale: Vec<Vec<Reg>>,
    /// By instruction: the registers of `stale` whose slot it fills (`true`:
    /// with a value that may be shared; `false`: with a constant, which does
    /// not count). Native code keeps, per such register, whether its slot
    /// holds a value to let go, and empties a stale slot only when it does:
    /// in a loop the slot is stale at the first pass and empty afterwards.
    fills: Vec<Vec<(Reg, bool)>>,
}

/// The largest function the JIT compiles, in registers and instructions.
const MAX_REGS: usize = 1024;
const MAX_INSTRUCTIONS: usize = 16_384;

/// Checks `f` for native compilation: every register, jump target, constant
/// and callee is in range, every instruction is supported, and every operand
/// has the kind the instruction needs on every path that reaches it (found by
/// a forward data flow analysis). Callees must be `ok` (indexed by function
/// id). Bytecode can come from the on-disk cache, and the JIT must never
/// trust it blindly.
fn well_formed(prog: &Program, f: &Function, ok: &[bool]) -> Option<Checked> {
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
            // A branch back (a loop's test at its end) counts toward the
            // time limit's check like `Jump` (see `Translator::edge`).
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
                        let j = join(prog, *o, *k);
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
    let (stale, fills) =
        if strings { stale_slots(prog, sig, ok, f, &kinds) } else { (vec![Vec::new(); n], vec![Vec::new(); n]) };
    Some(Checked { kinds, strings, stale, fills })
}

/// The registers each instruction makes a machine number or boolean (in a
/// machine register, not the window) while their window slot may still hold
/// a string, list, map or object from earlier. The interpreter's write drops
/// the old value; native code only defines its machine register, so the
/// stale copy stayed in the slot and kept the value shared: the next change
/// (`$o->xs[] = ...`, an element write) copied all of it, and a loop doing
/// that took quadratic time (audit 20). Found by a forward analysis of
/// whether each slot may hold such a value (`kinds` gives what each
/// instruction leaves), so registers that only ever hold numbers cost
/// nothing. A constant does not count: the unit's cache shares it anyway, so
/// letting it go gains nothing (a string constant kept in a register before a
/// loop would otherwise be let go at every pass).
///
/// Also returns, by instruction, the slots of those registers it fills
/// (`Checked::fills`), by the rule the analysis itself uses.
#[allow(clippy::type_complexity)]
fn stale_slots(
    prog: &Program,
    sig: &NativeSig,
    ok: &[bool],
    f: &Function,
    kinds: &[Option<Vec<K>>],
) -> (Vec<Vec<Reg>>, Vec<Vec<(Reg, bool)>>) {
    let (n, regs) = (f.code.len(), f.num_regs as usize);
    let words = regs.div_ceil(64);
    let scalar = |k: K| matches!(k, K::Int | K::Bool | K::Float);
    // The kinds after instruction `i` and where control goes next.
    let after = |i: usize, st: &[K]| -> Option<(Vec<K>, Flow)> {
        let mut st = st.to_vec();
        let flow = step(prog, sig, ok, &f.code[i], &mut st, &mut true)?;
        Some((st, flow))
    };
    // Whether the slot may hold a boxed value, before each instruction (a bit per register).
    let mut held: Vec<Option<Vec<u64>>> = vec![None; n];
    let mut entry = vec![0u64; words];
    for (r, p) in sig.params.iter().enumerate() {
        if p.boxed() {
            entry[r / 64] |= 1 << (r % 64);
        }
    }
    held[0] = Some(entry);
    let mut work = vec![0usize];
    while let Some(i) = work.pop() {
        let (Some(before), Some(mut out)) = (&kinds[i], held[i].clone()) else { continue };
        let Some((st, flow)) = after(i, before) else { continue };
        let next = matches!(flow, Flow::Next);
        let written = writes(&f.code[i]);
        for (r, k) in st.iter().enumerate() {
            if k.boxed() {
                // A new value (a register it writes, or one whose kind it
                // changed): a constant, or one that may be shared.
                if written.contains(&Some(r as Reg)) || *k != before[r] {
                    if matches!(f.code[i], Instr::Const(..)) {
                        out[r / 64] &= !(1 << (r % 64));
                    } else {
                        out[r / 64] |= 1 << (r % 64);
                    }
                }
            } else if next && scalar(*k) {
                // Emptied after the instruction (see below), or already empty.
                out[r / 64] &= !(1 << (r % 64));
            }
        }
        let targets = match flow {
            Flow::Next => [Some(i + 1), None],
            Flow::Jump(t) => [Some(t as usize), None],
            Flow::Branch(t) => [Some(i + 1), Some(t as usize)],
            Flow::Stop => [None, None],
        };
        for t in targets.into_iter().flatten().filter(|t| *t < n) {
            match &mut held[t] {
                None => {
                    held[t] = Some(out.clone());
                    work.push(t);
                }
                Some(old) => {
                    let mut changed = false;
                    for (o, w) in old.iter_mut().zip(&out) {
                        if *o | w != *o {
                            *o |= w;
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
    let mut stale = vec![Vec::new(); n];
    let mut any = vec![false; regs];
    for i in 0..n {
        let (Some(st), Some(before)) = (&kinds[i], &held[i]) else { continue };
        let Some((st, Flow::Next)) = after(i, st) else { continue };
        for (r, k) in st.iter().enumerate() {
            if scalar(*k) && before[r / 64] >> (r % 64) & 1 != 0 {
                stale[i].push(r as Reg);
                any[r] = true;
            }
        }
    }
    // What the analysis above counts as filling a slot, for those registers.
    let mut fills = vec![Vec::new(); n];
    for i in 0..n {
        let Some(before) = &kinds[i] else { continue };
        let Some((st, _)) = after(i, before) else { continue };
        let written = writes(&f.code[i]);
        for (r, k) in st.iter().enumerate() {
            if any[r] && k.boxed() && (written.contains(&Some(r as Reg)) || *k != before[r]) {
                fills[i].push((r as Reg, !matches!(f.code[i], Instr::Const(..))));
            }
        }
    }
    (stale, fills)
}

/// The registers an instruction puts a new string, list, map or object in,
/// as far as `stale_slots` needs them (others are seen by their kind).
fn writes(ins: &Instr) -> [Option<Reg>; 2] {
    use Instr::*;
    match *ins {
        Const(d, _)
        | Move(d, _)
        | ToStr(d, _)
        | Concat(d, ..)
        | AppendStr(d, _)
        | Index(d, ..)
        | IndexOrNull(d, ..)
        | GetProp(d, ..)
        | NewList(d, ..)
        | NewMap(d, ..)
        | Range(d, ..)
        | NewObject(d, _)
        | IterInit(d, _) => [Some(d), None],
        Call { dst, .. } | CallVirtual { dst, .. } | CallBuiltin { dst, .. } => [Some(dst), None],
        IterNext { key, val, .. } => [Some(key), Some(val)],
        SetPath(ref p) | AppendPath { path: ref p, .. } => match p.base {
            PathBase::Local(r) => [Some(r), None],
            PathBase::Prop(..) => [None, None],
        },
        _ => [None, None],
    }
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
fn step(prog: &Program, sig: &NativeSig, ok: &[bool], ins: &Instr, st: &mut [K], strings: &mut bool) -> Option<Flow> {
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
            // The operands are moved out (they are temporaries).
            for i in 0..n {
                set(st, start + i, K::Undef)?;
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
        JumpIfNotLtIImm(a, _, t) | JumpIfNotLeIImm(a, _, t) | JumpIfLtIImm(a, _, t) | JumpIfLeIImm(a, _, t) => {
            need(st, a, K::Int)?;
            return Some(Flow::Branch(t));
        }
        JumpIfEq(a, b, t) | JumpIfNotEq(a, b, t) => {
            eq_mode(kind(st, a)?, kind(st, b)?)?;
            return Some(Flow::Branch(t));
        }
        // `$o->p .= $x` on a string property.
        AppendPath { ref path, cur } if matches!(path.base, PathBase::Prop(..)) => {
            let PathBase::Prop(o, slot) = path.base else { return None };
            if !path.keys.is_empty() || path.push || field_kind(prog, kind(st, o)?, slot)? != K::Str {
                return None;
            }
            need(st, cur, K::Str)?;
            kind(st, path.val)?;
            set(st, cur, K::Undef)?;
        }
        // `$o->p[] = $v` and `$o->p[$k] = $v` on a list or map property.
        SetPath(ref path) if matches!(path.base, PathBase::Prop(..)) => {
            let PathBase::Prop(o, slot) = path.base else { return None };
            let v = E::of(kind(st, path.val)?)?;
            match (field_kind(prog, kind(st, o)?, slot)?, path.push, path.keys.as_slice()) {
                (K::List(e), true, []) if e == v => {}
                (K::List(e), false, [k]) if e == v => need(st, *k, K::Int)?,
                (K::Map(ke, ve), false, [k]) if ve == v && matches!(ke, E::Int | E::Str) => need(st, *k, ke.scalar()?)?,
                _ => return None,
            }
        }
        // `.=` on an element of a string list or map in a variable.
        AppendPath { ref path, cur } => {
            let PathBase::Local(r) = path.base else { return None };
            let [k] = path.keys.as_slice() else { return None };
            match kind(st, r)? {
                K::List(E::Str) => need(st, *k, K::Int)?,
                K::Map(ke, E::Str) => need_key(st, *k, ke)?,
                _ => return None,
            }
            need(st, cur, K::Str)?;
            kind(st, path.val)?;
            // The element read is taken back.
            set(st, cur, K::Undef)?;
        }
        Call { dst, func, args, argc } => {
            let ret = call_kind(prog, ok, st, func, args, argc, false)?;
            // Arguments and results passed in the window (strings, lists,
            // maps and nullable values) need one in the caller too (found by
            // the 1.0 audit: an int function calling a `?int` one wrote into
            // its caller's window).
            let callee = prog.funcs.get(func as usize)?.native.as_ref()?;
            if callee.ret.boxed() || callee.params.iter().any(|p| p.boxed()) {
                *strings = true;
            }
            for k in 0..argc {
                set(st, args + k, K::Undef)?;
            }
            set(st, dst, ret)?;
        }
        // A method call through the class's method table: every function
        // the object's class (the receiver's or a subclass) may have in that
        // slot is compiled, takes these arguments and returns the same kind.
        CallVirtual { dst, slot, args, argc } => {
            let targets = virtual_targets(prog, kind(st, args)?.class()?, slot)?;
            let mut ret = None;
            for &t in &targets {
                let k = call_kind(prog, ok, st, t, args, argc, true)?;
                let r = prog.funcs.get(t as usize)?.native.as_ref()?.ret;
                if ret.is_some_and(|(_, old)| old != r) {
                    return None;
                }
                ret = Some((k, r));
            }
            let (ret, _) = ret?;
            *strings = true;
            for k in 0..argc {
                set(st, args + k, K::Undef)?;
            }
            set(st, dst, ret)?;
        }
        Ret(r) => {
            match K::of(sig.ret) {
                None => kind(st, r).map(|_| ())?,
                Some(k) => fits(prog, kind(st, r)?, k).then_some(())?,
            }
            return Some(Flow::Stop);
        }
        // A string, list, map or object function must return one on every path.
        RetNull if sig.ret.boxed() && !matches!(sig.ret, ValKind::Opt(_) | ValKind::OptObject(_)) => return None,
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
        NewObject(d, c) => {
            prog.classes.get(c as usize)?;
            set(st, d, K::Obj(c))?;
        }
        // Properties by slot, of the kind the class declares.
        GetProp(d, o, slot) => {
            let k = field_kind(prog, kind(st, o)?, slot)?;
            k.elem()?;
            set(st, d, k)?;
        }
        SetProp(o, slot, v) => {
            let k = field_kind(prog, kind(st, o)?, slot)?;
            fits(prog, kind(st, v)?, k).then_some(())?;
        }
        // Output: what `echo` writes of a number, a boolean, a string or null.
        Echo(r) | EchoAttr(r) => {
            let k = kind(st, r)?;
            if k != K::Null && k.base().is_none() {
                return None;
            }
            *strings = true;
        }
        EchoHtml(c) => {
            prog.consts.get(c as usize)?;
        }
        // Statements in HTML other than element text (see `Vm::guard_output`).
        GuardOutput | EndGuard | EchoNonce => {}
        _ => return None,
    }
    if st.iter().any(|k| k.boxed()) {
        *strings = true;
    }
    Some(Flow::Next)
}

/// The kind a call of `func` with the `argc` arguments from register `args`
/// returns (`Undef` for none), when `func` can be compiled (`ok`) and takes
/// them. For a method called through the method table (`virt`), the object
/// is of the class the function belongs to, so the first one is not checked.
fn call_kind(prog: &Program, ok: &[bool], st: &[K], func: u32, args: Reg, argc: u16, virt: bool) -> Option<K> {
    if !ok.get(func as usize).copied().unwrap_or(false) {
        return None;
    }
    let callee = prog.funcs.get(func as usize)?.native.as_ref()?;
    if callee.params.len() != argc as usize {
        return None;
    }
    for (k, p) in callee.params.iter().enumerate() {
        let have = kind(st, args.checked_add(k as u16)?)?;
        if !(virt && k == 0) && !fits(prog, have, K::of(*p)?) {
            return None;
        }
    }
    Some(match callee.ret {
        ValKind::Void => K::Undef,
        v => K::of(v)?,
    })
}

/// The functions a `CallVirtual` of `slot` on an object of class `c` may
/// call: what each class that is `c` or extends it has in that slot, sorted.
/// None when a class has nothing there, or there is no such class.
fn virtual_targets(prog: &Program, c: u32, slot: u32) -> Option<Vec<u32>> {
    let mut targets = Vec::new();
    for (x, class) in prog.classes.iter().enumerate() {
        if extends(prog, x as u32, c) {
            targets.push(*class.vtable.get(slot as usize)?);
        }
    }
    targets.sort_unstable();
    targets.dedup();
    (!targets.is_empty()).then_some(targets)
}

/// The kind property `slot` of an object of kind `k` holds, as its class
/// declares it.
fn field_kind(prog: &Program, k: K, slot: u16) -> Option<K> {
    let field = prog.classes.get(k.class()? as usize)?.fields.get(slot as usize)?;
    K::of(field.kind?)
}

/// The functions instruction `ins` may call: a `Call`'s, or for a
/// `CallVirtual` what any class has in its slot.
fn callees_of(prog: &Program, ins: &Instr) -> Vec<u32> {
    match *ins {
        Instr::Call { func, .. } => vec![func],
        Instr::CallVirtual { slot, .. } => {
            let mut v: Vec<u32> = prog.classes.iter().filter_map(|c| c.vtable.get(slot as usize).copied()).collect();
            v.sort_unstable();
            v.dedup();
            v
        }
        _ => Vec::new(),
    }
}

/// Functions that can be compiled: well formed, with calls only to other
/// compilable functions. Each function is checked once, then again only
/// when a function it calls is found not to be compilable (removing one can
/// only make its callers fail), so the work grows with the size of the
/// program, not with its size times the length of its call chains.
fn eligible(prog: &Program) -> HashMap<u32, Checked> {
    let n = prog.funcs.len();
    let mut ok: Vec<bool> = prog.funcs.iter().map(|f| f.native.is_some()).collect();
    // Who calls each function (each caller once per callee).
    let mut callers: Vec<Vec<u32>> = vec![Vec::new(); n];
    for (i, f) in prog.funcs.iter().enumerate().filter(|(i, _)| ok[*i]) {
        for ins in &f.code {
            for func in callees_of(prog, ins) {
                if let Some(c) = callers.get_mut(func as usize) {
                    if c.last() != Some(&(i as u32)) {
                        c.push(i as u32);
                    }
                }
            }
        }
    }
    let mut queued = ok.clone();
    let mut work: Vec<u32> = (0..n as u32).filter(|&i| ok[i as usize]).rev().collect();
    let mut checked: HashMap<u32, Checked> = HashMap::new();
    while let Some(f) = work.pop() {
        queued[f as usize] = false;
        if !ok[f as usize] {
            continue;
        }
        match well_formed(prog, &prog.funcs[f as usize], &ok) {
            Some(c) => {
                checked.insert(f, c);
            }
            None => {
                ok[f as usize] = false;
                checked.remove(&f);
                for &c in &callers[f as usize] {
                    if ok[c as usize] && !queued[c as usize] {
                        queued[c as usize] = true;
                        work.push(c);
                    }
                }
            }
        }
    }
    checked
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
    AppendKey,
    StrEq,
    Unwind,
    New,
    GetProp,
    SetProp,
    SetPropPath,
    AppendProp,
    VTarget,
    Echo,
    EchoHtml,
    Bad,
    Guard,
    EchoNonce,
    SetPropScalar,
    GetPropScalar,
}

/// Name, address, number of i64 arguments after `ctx` (None: `(f64, f64) -> f64`)
/// and whether it returns an i64, in the order of `H`.
fn helpers() -> [(&'static str, *const u8, Option<usize>, bool); 45] {
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
        ("phv_jit_append_key", phv_jit_append_key as *const u8, Some(4), false),
        ("phv_jit_str_eq", phv_jit_str_eq as *const u8, Some(2), true),
        ("phv_jit_unwind", phv_jit_unwind as *const u8, Some(2), false),
        ("phv_jit_new", phv_jit_new as *const u8, Some(2), false),
        ("phv_jit_get_prop", phv_jit_get_prop as *const u8, Some(5), true),
        ("phv_jit_set_prop", phv_jit_set_prop as *const u8, Some(3), false),
        ("phv_jit_set_prop_path", phv_jit_set_prop_path as *const u8, Some(5), false),
        ("phv_jit_append_prop", phv_jit_append_prop as *const u8, Some(4), false),
        ("phv_jit_vtarget", phv_jit_vtarget as *const u8, Some(2), true),
        ("phv_jit_echo", phv_jit_echo as *const u8, Some(2), true),
        ("phv_jit_echo_html", phv_jit_echo_html as *const u8, Some(1), true),
        ("phv_jit_bad", phv_jit_bad as *const u8, Some(0), false),
        ("phv_jit_guard", phv_jit_guard as *const u8, Some(1), false),
        ("phv_jit_echo_nonce", phv_jit_echo_nonce as *const u8, Some(0), true),
        ("phv_jit_set_prop_scalar", phv_jit_set_prop_scalar as *const u8, Some(4), false),
        ("phv_jit_get_prop_scalar", phv_jit_get_prop_scalar as *const u8, Some(3), true),
    ]
}

/// Whether `f` changes one of its parameters in place: a string, list or
/// map passed in the window (see `JitTable::writes_args`).
fn writes_args(f: &Function) -> bool {
    let param = |r: Reg| (r as usize) < f.num_params as usize;
    f.code.iter().any(|ins| match ins {
        Instr::AppendStr(d, _) => param(*d),
        Instr::SetPath(p) | Instr::AppendPath { path: p, .. } => matches!(p.base, PathBase::Local(r) if param(r)),
        _ => false,
    })
}

/// Whether `f` has a loop: a branch to an earlier instruction (the compiler
/// branches back only for loops).
fn has_loop(f: &Function) -> bool {
    f.code.iter().enumerate().any(|(i, ins)| back_target(i, ins).is_some())
}

fn back_target(i: usize, ins: &Instr) -> Option<usize> {
    use Instr::*;
    let t = match *ins {
        Jump(t)
        | JumpIfFalse(_, t)
        | JumpIfTrue(_, t)
        | JumpIfNull(_, t)
        | JumpIfNotNull(_, t)
        | JumpIfNotLtI(_, _, t)
        | JumpIfNotLeI(_, _, t)
        | JumpIfEq(_, _, t)
        | JumpIfNotEq(_, _, t)
        | JumpIfNotLtIImm(_, _, t)
        | JumpIfNotLeIImm(_, _, t)
        | JumpIfLtIImm(_, _, t)
        | JumpIfLeIImm(_, _, t) => t as usize,
        _ => return None,
    };
    (t <= i).then_some(t)
}

/// Scalar reads of object parameters inside a single loop. Restrict the
/// entire function to operations that cannot replace those parameters or
/// mutate their objects, even through aliases.
fn loop_prop_reads(prog: &Program, f: &Function, checked: &Checked) -> Vec<usize> {
    use phv_vm::bytecode::Const as Literal;
    use Instr::*;
    let params = &f.native.as_ref().expect("checked").params;
    let object_param = |r: Reg| matches!(params.get(r as usize), Some(ValKind::Object(_)));
    if !params.iter().any(|p| matches!(p, ValKind::Object(_))) {
        return Vec::new();
    }
    let mut back_edges = f.code.iter().enumerate().filter_map(|(i, ins)| {
        checked.kinds[i].as_ref()?;
        back_target(i, ins).map(|start| start..=i)
    });
    let Some(body) = back_edges.next() else { return Vec::new() };
    if back_edges.next().is_some() {
        return Vec::new();
    }
    let mut reads = Vec::new();
    for (i, (ins, state)) in f.code.iter().zip(&checked.kinds).enumerate() {
        let Some(state) = state else { continue };
        let dst = match *ins {
            GetProp(d, o, slot)
                if matches!(field_kind(prog, state[o as usize], slot), Some(K::Int | K::Bool | K::Float)) =>
            {
                if object_param(o) && body.contains(&i) {
                    reads.push(i);
                }
                d
            }
            Const(d, c)
                if matches!(prog.consts[c as usize], Literal::Int(_) | Literal::Bool(_) | Literal::Float(_)) =>
            {
                d
            }
            Move(d, s) if matches!(state[s as usize], K::Int | K::Bool | K::Float) => d,
            Int(d, _)
            | Bool(d, _)
            | AddI(d, _, _)
            | AddIImm(d, _, _)
            | SubI(d, _, _)
            | SubIImm(d, _, _)
            | MulI(d, _, _)
            | ModI(d, _, _)
            | IncI(d, _)
            | AddF(d, _, _)
            | SubF(d, _, _)
            | MulF(d, _, _)
            | DivF(d, _, _)
            | IntToFloat(d, _)
            | LtI(d, _, _)
            | LeI(d, _, _)
            | EqI(d, _, _)
            | Not(d, _) => d,
            Jump(_) | JumpIfFalse(..) | JumpIfTrue(..) | JumpIfNotLtI(..) | JumpIfNotLeI(..) | JumpIfNotLtIImm(..)
            | JumpIfNotLeIImm(..) | JumpIfLtIImm(..) | JumpIfLeIImm(..) | Ret(_) | RetNull => continue,
            _ => return Vec::new(),
        };
        if object_param(dst) {
            return Vec::new();
        }
    }
    // Bound the extra live native values in the loop.
    reads.truncate(4);
    reads
}

/// Per function: whether it may call itself again, directly or through other
/// functions (it is on a cycle of the call graph). Tarjan's algorithm, with
/// an explicit stack (call chains can be long).
fn recursive(prog: &Program) -> Vec<bool> {
    let n = prog.funcs.len();
    let callees: Vec<Vec<usize>> = prog
        .funcs
        .iter()
        .map(|f| {
            f.code
                .iter()
                .flat_map(|i| callees_of(prog, i))
                .filter(|&func| (func as usize) < n)
                .map(|func| func as usize)
                .collect()
        })
        .collect();
    let mut index = vec![usize::MAX; n];
    let mut low = vec![0usize; n];
    let mut on_stack = vec![false; n];
    let mut stack = Vec::new();
    let mut next = 0;
    let mut rec = vec![false; n];
    for root in 0..n {
        if index[root] != usize::MAX {
            continue;
        }
        let mut work: Vec<(usize, usize)> = vec![(root, 0)];
        index[root] = next;
        low[root] = next;
        next += 1;
        stack.push(root);
        on_stack[root] = true;
        while let Some(top) = work.last_mut() {
            let v = top.0;
            if top.1 < callees[v].len() {
                let w = callees[v][top.1];
                top.1 += 1;
                if index[w] == usize::MAX {
                    index[w] = next;
                    low[w] = next;
                    next += 1;
                    stack.push(w);
                    on_stack[w] = true;
                    work.push((w, 0));
                } else if on_stack[w] {
                    low[v] = low[v].min(index[w]);
                }
                continue;
            }
            work.pop();
            if let Some(&(u, _)) = work.last() {
                low[u] = low[u].min(low[v]);
            }
            if low[v] == index[v] {
                let mut members = Vec::new();
                loop {
                    let w = stack.pop().unwrap();
                    on_stack[w] = false;
                    members.push(w);
                    if w == v {
                        break;
                    }
                }
                if members.len() > 1 || callees[v].contains(&v) {
                    for w in members {
                        rec[w] = true;
                    }
                }
            }
        }
    }
    rec
}

/// The native code of a program: the eligible functions with a loop or
/// recursion, each compiled as a unit with the functions it calls (see
/// `canonical`). Returns `None` if nothing can be compiled (or the host is not
/// supported by Cranelift).
///
/// Why these functions: one call of a loop or a recursion may run for the
/// whole script, and native code is entered only at a call, so they must be
/// compiled before they run. A straight-line function called from the
/// interpreter gains nothing: entering native code costs more than its body
/// (measured on the 1.2.1 cycle: a body of one addition, 2 million calls,
/// 172 ms compiled against 131 interpreted; one that joins strings, 710
/// against 425; a body of 30 integer operations saves 0.3 µs a call, and
/// compiling it costs 0.5–1.5 ms). It is compiled only inside a caller's
/// unit, where the call is direct. So what a function runs as is fixed when
/// the program is loaded, and the same in every run.
pub fn table(prog: &Program) -> Option<JitTable> {
    let rec = recursive(prog);
    if std::env::var_os("PHV_JIT_EXPLAIN").is_some() {
        for (i, f) in prog.funcs.iter().enumerate() {
            if f.native.is_none() && (rec[i] || has_loop(f)) {
                eprintln!(
                    "phv jit: {} not compiled: a script, a template or a closure, or a value of a type native code does not hold",
                    f.name
                );
            }
        }
    }
    let roots = prog.funcs.iter().enumerate().filter(|(i, f)| {
        if f.native.is_none() {
            return false;
        }
        if has_loop(f) {
            return true;
        }
        if !rec[*i] {
            return false;
        }
        let (inline, helpers) = work(prog, f);
        let pays = inline >= 2 * helpers;
        if !pays && std::env::var_os("PHV_JIT_EXPLAIN").is_some() {
            eprintln!(
                "phv jit: {} not compiled on its own: recursive, with {inline} instructions run inline against {helpers} calls into the runtime",
                f.name
            );
        }
        pays
    });
    build_table(prog, roots.map(|(i, _)| i as u32))
}

/// For a recursive function without a loop: the instructions of its body
/// native code runs inline (arithmetic, comparisons, branches, moves, calls
/// of functions that take and return only numbers) and those that are calls
/// into the runtime (properties, `new`, strings, lists and maps, output,
/// builtins, and calls that pass or return a string, list, map or object).
/// Such a function is compiled on its own only when the first are at least
/// twice the second. A runtime call costs about what the interpreter's
/// instruction costs, and a native call that passes an object costs about
/// what an interpreted call costs, so a recursion whose work is object
/// operations gains nothing natively and pays for entering native code
/// (measured on the 1.3 cycle: a recursive sum over a tree of objects, 8
/// against 7, ran 15% slower natively; `fib`, 9 against 0, 12 times faster).
/// A function with a loop is always compiled: each pass saves the
/// interpreter's dispatch even when its body is runtime calls (a loop of
/// `.=` on a property, 2 against 3, ran 30% faster).
fn work(prog: &Program, f: &Function) -> (usize, usize) {
    use Instr::*;
    let boxed_call = |func: u32| {
        prog.funcs
            .get(func as usize)
            .and_then(|c| c.native.as_ref())
            .is_none_or(|sig| sig.ret.boxed() || sig.params.iter().any(|p| p.boxed()))
    };
    let (mut inline, mut helpers) = (0, 0);
    for ins in &f.code {
        let helper = match ins {
            Const(_, c) => !matches!(
                prog.consts.get(*c as usize),
                Some(
                    phv_vm::bytecode::Const::Int(_)
                        | phv_vm::bytecode::Const::Float(_)
                        | phv_vm::bytecode::Const::Bool(_)
                )
            ),
            Call { func, .. } => boxed_call(*func),
            GetProp(..)
            | SetProp(..)
            | NewObject(..)
            | SetPath(_)
            | AppendPath { .. }
            | AppendStr(..)
            | Concat(..)
            | ToStr(..)
            | NewList(..)
            | NewMap(..)
            | Index(..)
            | IndexOrNull(..)
            | IterInit(..)
            | IterNext { .. }
            | Range(..)
            | CallBuiltin { .. }
            | CallVirtual { .. }
            | CallNamed { .. }
            | CallClosure { .. }
            | Echo(_)
            | EchoAttr(_)
            | EchoHtml(_)
            | EchoNonce
            | GuardOutput
            | EndGuard
            | PowI(..)
            | ModF(..)
            | PowF(..) => true,
            _ => false,
        };
        if helper {
            helpers += 1;
        } else {
            inline += 1;
        }
    }
    (inline, helpers)
}

/// A table with every eligible function compiled, the straight-line ones too
/// (tests of the JIT; `table` is what scripts run with).
pub fn table_all(prog: &Program) -> Option<JitTable> {
    let roots = prog.funcs.iter().enumerate().filter(|(_, f)| f.native.is_some());
    build_table(prog, roots.map(|(i, _)| i as u32))
}

/// Compiles the units of `roots` (in order; a function already compiled in
/// an earlier unit keeps that code) into a table.
fn build_table(prog: &Program, roots: impl Iterator<Item = u32>) -> Option<JitTable> {
    isa()?;
    let mut entries: Vec<Option<Installed>> = (0..prog.funcs.len()).map(|_| None).collect();
    for root in roots {
        if entries[root as usize].is_some() {
            continue;
        }
        for (f, installed) in compile_root(prog, root) {
            let slot = &mut entries[f as usize];
            if slot.is_none() {
                *slot = Some(installed);
            }
        }
    }
    if entries.iter().all(|e| e.is_none()) {
        return None;
    }
    let writes_args = prog.funcs.iter().map(writes_args).collect();
    Some(JitTable::new(entries, writes_args, impure(prog)))
}

/// Per function: it changes an object or writes output, or calls a
/// function that may (see `JitTable::impure`).
fn impure(prog: &Program) -> Vec<bool> {
    let n = prog.funcs.len();
    let effect = |ins: &Instr| match ins {
        Instr::SetProp(..)
        | Instr::Echo(_)
        | Instr::EchoAttr(_)
        | Instr::EchoHtml(_)
        | Instr::EchoNonce
        | Instr::GuardOutput
        | Instr::EndGuard => true,
        Instr::SetPath(p) | Instr::AppendPath { path: p, .. } => matches!(p.base, PathBase::Prop(..)),
        _ => false,
    };
    let mut imp: Vec<bool> = prog.funcs.iter().map(|f| f.code.iter().any(effect)).collect();
    let mut callers: Vec<Vec<u32>> = vec![Vec::new(); n];
    for (i, f) in prog.funcs.iter().enumerate() {
        for ins in &f.code {
            for c in callees_of(prog, ins) {
                if let Some(v) = callers.get_mut(c as usize) {
                    v.push(i as u32);
                }
            }
        }
    }
    let mut work: Vec<u32> = (0..n as u32).filter(|&i| imp[i as usize]).collect();
    while let Some(f) = work.pop() {
        for &c in &callers[f as usize] {
            if !imp[c as usize] {
                imp[c as usize] = true;
                work.push(c);
            }
        }
    }
    imp
}

/// The host's instruction set, made once per process.
fn isa() -> Option<cranelift_codegen::isa::OwnedTargetIsa> {
    static ISA: std::sync::OnceLock<Option<cranelift_codegen::isa::OwnedTargetIsa>> = std::sync::OnceLock::new();
    ISA.get_or_init(|| {
        let mut flags = settings::builder();
        flags.set("use_colocated_libcalls", "false").ok()?;
        flags.set("is_pic", "false").ok()?;
        flags.set("opt_level", "speed").ok()?;
        cranelift_native::builder().ok()?.finish(settings::Flags::new(flags)).ok()
    })
    .clone()
}

/// A unit of native code: one function and the functions it calls, compiled
/// together (their calls are direct). Immutable once made, and shared between
/// the programs whose functions it compiles (see `canonical`).
struct Unit {
    /// None when the root function could not be compiled (remembered, so
    /// that the check is not run again for the same functions).
    code: Option<Code>,
    /// Per function of the canonical program: its entry, when compiled.
    entries: Vec<Option<JitEntry>>,
}

/// A function and what its native code depends on, renamed so that the same
/// program text gives the same result wherever it is loaded: the functions it
/// reaches through calls (numbered from the root in the order they are first
/// called), the constants they name (numbered in the order they are first
/// named) and the builtins they call (by name).
///
/// The machine code of a unit is computed from this program alone (the
/// instructions, signatures, register counts, constant values and builtin
/// names: `well_formed`, `step` and `Translator` read nothing else of a
/// program), and names nothing of the program it came from: the VM's state
/// is reached through `JitCtx`, constants through `JitCtx::consts`, calls
/// between the unit's functions are direct, and helpers are process-wide
/// symbols. So two programs with the same canonical form run the same code,
/// which the standard library's functions and a project's modules give for
/// every page that loads them.
struct Canonical {
    prog: Program,
    /// The original ids of the canonical functions, in order.
    members: Vec<u32>,
    /// The original indices of the canonical constants, in order.
    consts: Vec<u32>,
    /// The original ids of the canonical classes, in order.
    classes: Vec<u32>,
    /// Everything of `prog` the code depends on, serialized so that no other
    /// canonical program gives the same bytes (the key under which the unit
    /// is shared; see `unit_key`).
    key: Vec<u8>,
}

/// The canonical form of function `root` and the functions it reaches.
fn canonical(prog: &Program, root: u32) -> Canonical {
    let native = |f: u32| prog.funcs.get(f as usize).is_some_and(|f| f.native.is_some());
    let mut members = vec![root];
    let mut local: HashMap<u32, u32> = HashMap::from([(root, 0)]);
    // The classes the code names (in signatures, `new` and the kinds of
    // properties), with every class that extends one of them: a method call
    // through the method table may reach any of these.
    let mut classes: Vec<u32> = Vec::new();
    let mut class_local: HashMap<u32, u32> = HashMap::new();
    // The method table slots the code calls through.
    let mut slots: Vec<u32> = Vec::new();
    let add_class = |c: u32, classes: &mut Vec<u32>, class_local: &mut HashMap<u32, u32>| {
        if (c as usize) < prog.classes.len() && !class_local.contains_key(&c) {
            class_local.insert(c, classes.len() as u32);
            classes.push(c);
        }
    };
    let kinds_class = |k: &ValKind| match *k {
        ValKind::Object(c) | ValKind::OptObject(c) => Some(c),
        _ => None,
    };
    let (mut seen_members, mut seen_classes, mut seen_slots) = (0, 0, 0);
    loop {
        let before = (members.len(), classes.len(), slots.len());
        while seen_members < members.len() {
            let f = &prog.funcs[members[seen_members] as usize];
            seen_members += 1;
            if let Some(sig) = &f.native {
                for c in sig.params.iter().chain([&sig.ret]).filter_map(kinds_class) {
                    add_class(c, &mut classes, &mut class_local);
                }
            }
            for ins in &f.code {
                match *ins {
                    Instr::Call { func, .. } if native(func) && !local.contains_key(&func) => {
                        local.insert(func, members.len() as u32);
                        members.push(func);
                    }
                    Instr::NewObject(_, c) => add_class(c, &mut classes, &mut class_local),
                    Instr::CallVirtual { slot, .. } if !slots.contains(&slot) => slots.push(slot),
                    _ => {}
                }
            }
        }
        while seen_classes < classes.len() {
            let c = classes[seen_classes];
            seen_classes += 1;
            for k in prog.classes[c as usize].fields.iter().filter_map(|f| f.kind.as_ref()) {
                if let Some(x) = kinds_class(k) {
                    add_class(x, &mut classes, &mut class_local);
                }
            }
            for (x, class) in prog.classes.iter().enumerate() {
                if class.supers.binary_search(&c).is_ok() {
                    add_class(x as u32, &mut classes, &mut class_local);
                }
            }
        }
        if seen_slots < slots.len() || before != (members.len(), classes.len(), slots.len()) {
            seen_slots = slots.len();
            for &c in &classes {
                for &slot in &slots {
                    let Some(&func) = prog.classes[c as usize].vtable.get(slot as usize) else { continue };
                    if native(func) && !local.contains_key(&func) {
                        local.insert(func, members.len() as u32);
                        members.push(func);
                    }
                }
            }
        }
        if before == (members.len(), classes.len(), slots.len()) && seen_members == members.len() {
            break;
        }
    }
    let class_id = |c: u32| class_local.get(&c).copied().unwrap_or(u32::MAX);
    let kind = |k: ValKind| match k {
        ValKind::Object(c) => ValKind::Object(class_id(c)),
        ValKind::OptObject(c) => ValKind::OptObject(class_id(c)),
        k => k,
    };
    let mut consts: Vec<u32> = Vec::new();
    let mut const_local: HashMap<u32, u32> = HashMap::new();
    let mut builtins: Vec<String> = Vec::new();
    let mut builtin_local: HashMap<u32, u32> = HashMap::new();
    let mut funcs = Vec::with_capacity(members.len());
    for &m in &members {
        let f = &prog.funcs[m as usize];
        let mut code = f.code.clone();
        for ins in &mut code {
            match ins {
                Instr::Const(_, c) | Instr::EchoHtml(c) => {
                    let n = consts.len() as u32;
                    *c = *const_local.entry(*c).or_insert_with(|| {
                        consts.push(*c);
                        n
                    });
                }
                // A callee that cannot be compiled (no native signature) is
                // out of range: `step` refuses the call where it is reached.
                Instr::Call { func, .. } => *func = local.get(func).copied().unwrap_or(u32::MAX),
                Instr::NewObject(_, c) => *c = class_id(*c),
                Instr::CallBuiltin { id, .. } => {
                    let n = builtins.len() as u32;
                    *id = *builtin_local.entry(*id).or_insert_with(|| {
                        builtins.push(prog.builtin_names.get(*id as usize).cloned().unwrap_or_default());
                        n
                    });
                }
                _ => {}
            }
        }
        let native = f
            .native
            .as_ref()
            .map(|sig| NativeSig { params: sig.params.iter().map(|&p| kind(p)).collect(), ret: kind(sig.ret) });
        // The name only serves `PHV_JIT_EXPLAIN`.
        funcs.push(Function {
            native,
            name: f.name.clone(),
            num_params: f.num_params,
            num_regs: f.num_regs,
            code,
            ..Default::default()
        });
    }
    // What the code reads of each class: its name, what it extends, the
    // kinds of its properties by slot and, for the calls through its method
    // table, the functions of the unit in each slot (others are out of
    // range, so `step` refuses a call that would reach them).
    let canon_classes: Vec<Class> = classes
        .iter()
        .map(|&c| {
            let class = &prog.classes[c as usize];
            let mut supers: Vec<u32> = class.supers.iter().filter_map(|s| class_local.get(s).copied()).collect();
            supers.sort_unstable();
            Class {
                name: class.name.clone(),
                parent: class.parent.and_then(|p| class_local.get(&p).copied()),
                fields: class
                    .fields
                    .iter()
                    .map(|f| Field {
                        name: f.name.clone(),
                        default: Const::Null,
                        public: f.public,
                        kind: f.kind.map(kind),
                    })
                    .collect(),
                vtable: class.vtable.iter().map(|f| local.get(f).copied().unwrap_or(u32::MAX)).collect(),
                supers,
                by_name: Vec::new(),
                cases: Vec::new(),
                acyclic: false,
            }
        })
        .collect();
    let const_values: Vec<Const> =
        consts.iter().map(|&c| prog.consts.get(c as usize).cloned().unwrap_or(Const::Null)).collect();
    let key = unit_key(&funcs, &const_values, &builtins, &canon_classes);
    let prog =
        Program { funcs, consts: const_values, builtin_names: builtins, classes: canon_classes, ..Default::default() };
    Canonical { prog, members, consts, classes, key }
}

// The tags of the unit key (see `unit_key`).
const KEY_FUNCS: u8 = 0x01;
const KEY_FUNC: u8 = 0x02;
const KEY_CONSTS: u8 = 0x03;
const KEY_BUILTINS: u8 = 0x04;
const KEY_BUILTIN: u8 = 0x05;
const KEY_CONST_NULL: u8 = 0x10;
const KEY_CONST_BOOL: u8 = 0x11;
const KEY_CONST_INT: u8 = 0x12;
const KEY_CONST_FLOAT: u8 = 0x13;
const KEY_CONST_STR: u8 = 0x14;
const KEY_CONST_EMPTY_LIST: u8 = 0x15;
const KEY_CONST_EMPTY_MAP: u8 = 0x16;
const KEY_CONST_EMPTY_SET: u8 = 0x17;
const KEY_CLASSES: u8 = 0x20;
const KEY_CLASS: u8 = 0x21;
const KEY_FIELDS: u8 = 0x22;
const KEY_FIELD: u8 = 0x23;
const KEY_SUPERS: u8 = 0x24;
const KEY_VTABLE: u8 = 0x25;
const KEY_PRIVATE: u8 = 0xFF;

/// The key a unit is shared under: a serialization of the canonical
/// program that no other canonical program gives. Every component starts
/// with a tag; a sequence with its count, a byte string with its length
/// (u32, little-endian), and a number is written fixed-width, so the bytes
/// of one component never run into the next. The instructions and the
/// signature are written as postcard (the variant index, then the fields,
/// each fixed-width or length-prefixed), and a float constant as its bits
/// (so that no two NaNs are alike, and nothing reads as another constant's
/// kind). A unit shares code only with a canonical program of the same
/// functions, the same number of constants of the same kinds and values,
/// and the same builtin names, in the same order. Should a component not
/// encode (postcard refusing a value, or a length above `u32::MAX`), the
/// key gets a number no other key has, and the unit is not shared.
fn unit_key(funcs: &[Function], consts: &[Const], builtins: &[String], classes: &[Class]) -> Vec<u8> {
    let mut key = UnitKey::default();
    key.seq(KEY_FUNCS, funcs.len());
    for f in funcs {
        key.bytes.push(KEY_FUNC);
        key.text(f.name.as_bytes());
        key.bytes.extend_from_slice(&f.num_params.to_le_bytes());
        key.bytes.extend_from_slice(&f.num_regs.to_le_bytes());
        key.postcard(&f.native);
        key.postcard(&f.code);
    }
    key.seq(KEY_CONSTS, consts.len());
    for c in consts {
        match c {
            Const::Null => key.bytes.push(KEY_CONST_NULL),
            Const::Bool(b) => key.bytes.extend_from_slice(&[KEY_CONST_BOOL, *b as u8]),
            Const::Int(v) => {
                key.bytes.push(KEY_CONST_INT);
                key.bytes.extend_from_slice(&v.to_le_bytes());
            }
            Const::Float(v) => {
                key.bytes.push(KEY_CONST_FLOAT);
                key.bytes.extend_from_slice(&v.to_bits().to_le_bytes());
            }
            Const::Str(s) => {
                key.bytes.push(KEY_CONST_STR);
                key.text(s.as_bytes());
            }
            Const::EmptyList => key.bytes.push(KEY_CONST_EMPTY_LIST),
            Const::EmptyMap => key.bytes.push(KEY_CONST_EMPTY_MAP),
            Const::EmptySet => key.bytes.push(KEY_CONST_EMPTY_SET),
        }
    }
    key.seq(KEY_BUILTINS, builtins.len());
    for b in builtins {
        key.bytes.push(KEY_BUILTIN);
        key.text(b.as_bytes());
    }
    // The classes, with what native code depends on: the layout of their
    // properties (slot, name and kind), what they extend and their method
    // tables (as functions of the unit).
    key.seq(KEY_CLASSES, classes.len());
    for c in classes {
        key.bytes.push(KEY_CLASS);
        key.text(c.name.as_bytes());
        key.seq(KEY_SUPERS, c.supers.len());
        for s in &c.supers {
            key.bytes.extend_from_slice(&s.to_le_bytes());
        }
        key.seq(KEY_FIELDS, c.fields.len());
        for f in &c.fields {
            key.bytes.push(KEY_FIELD);
            key.text(f.name.as_bytes());
            key.postcard(&f.kind);
        }
        key.seq(KEY_VTABLE, c.vtable.len());
        for f in &c.vtable {
            key.bytes.extend_from_slice(&f.to_le_bytes());
        }
    }
    if !key.ok {
        static PRIVATE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = PRIVATE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        key.bytes.push(KEY_PRIVATE);
        key.bytes.extend_from_slice(&n.to_le_bytes());
    }
    key.bytes
}

struct UnitKey {
    bytes: Vec<u8>,
    /// False once a component could not be encoded.
    ok: bool,
}

impl Default for UnitKey {
    fn default() -> Self {
        UnitKey { bytes: Vec::new(), ok: true }
    }
}

impl UnitKey {
    /// A tag and the number of items that follow.
    fn seq(&mut self, tag: u8, n: usize) {
        self.bytes.push(tag);
        self.len(n);
    }

    fn len(&mut self, n: usize) {
        match u32::try_from(n) {
            Ok(n) => self.bytes.extend_from_slice(&n.to_le_bytes()),
            Err(_) => self.ok = false,
        }
    }

    /// A length-prefixed byte string.
    fn text(&mut self, s: &[u8]) {
        self.len(s.len());
        self.bytes.extend_from_slice(s);
    }

    /// A value in postcard, length-prefixed.
    fn postcard<T: serde::Serialize>(&mut self, v: &T) {
        match postcard::to_allocvec(v) {
            Ok(b) => self.text(&b),
            Err(_) => self.ok = false,
        }
    }
}

/// The units compiled in this process, by canonical form: a page's standard
/// library functions are compiled once, not once per page. Bounded; the
/// oldest units leave first (a unit still installed in a table lives on
/// through it).
struct Shared {
    units: HashMap<Vec<u8>, Arc<Unit>>,
    order: std::collections::VecDeque<Vec<u8>>,
    key_bytes: usize,
}

const SHARED_UNITS: usize = 4096;
const SHARED_KEY_BYTES: usize = 64 << 20;

static SHARED: Mutex<Option<Shared>> = Mutex::new(None);

fn shared_get(key: &[u8]) -> Option<Arc<Unit>> {
    SHARED.lock().unwrap_or_else(|e| e.into_inner()).as_ref()?.units.get(key).cloned()
}

/// Keeps `unit` under `key`, or gives the one another thread kept meanwhile.
fn shared_put(key: Vec<u8>, unit: Arc<Unit>) -> Arc<Unit> {
    let mut guard = SHARED.lock().unwrap_or_else(|e| e.into_inner());
    let shared = guard.get_or_insert_with(|| Shared {
        units: HashMap::new(),
        order: std::collections::VecDeque::new(),
        key_bytes: 0,
    });
    if let Some(u) = shared.units.get(&key) {
        return u.clone();
    }
    while shared.units.len() >= SHARED_UNITS || shared.key_bytes + key.len() > SHARED_KEY_BYTES {
        let Some(old) = shared.order.pop_front() else { break };
        shared.key_bytes -= old.len();
        shared.units.remove(&old);
    }
    shared.key_bytes += key.len();
    shared.order.push_back(key.clone());
    shared.units.insert(key, unit.clone());
    unit
}

/// Compiles function `root` of `prog` with the functions it calls, sharing
/// the unit with other programs of the same text, and gives the code of each
/// function compiled, by function id (nothing when `root` cannot be
/// compiled). `PHV_JIT_STATS=1` reports each compilation on stderr.
fn compile_root(prog: &Program, root: u32) -> Vec<(u32, Installed)> {
    let start = std::time::Instant::now();
    let loop_prop_cache = !std::env::var_os("PHV_JIT_LOOP_PROP_CACHE").is_some_and(|v| v == "0");
    let mut canon = canonical(prog, root);
    canon.key.push(u8::from(loop_prop_cache));
    let (unit, hit) = match shared_get(&canon.key) {
        Some(u) => (u, true),
        None => {
            let unit = Arc::new(compile_unit(&canon.prog, loop_prop_cache));
            (shared_put(canon.key, unit), false)
        }
    };
    if std::env::var_os("PHV_JIT_STATS").is_some() {
        let name = &prog.funcs[root as usize].name;
        let n = unit.entries.iter().filter(|e| e.is_some()).count();
        let how = if hit { "shared" } else { "compiled" };
        eprintln!(
            "phv jit: {name}: {how} {n} of {} functions in {} µs",
            canon.members.len(),
            start.elapsed().as_micros()
        );
    }
    if unit.code.is_none() {
        return Vec::new();
    }
    let consts: Arc<[u32]> = canon.consts.into();
    let funcs: Arc<[u32]> = canon.members.clone().into();
    let classes: Arc<[u32]> = canon.classes.into();
    let keep_alive: Arc<dyn std::any::Any + Send + Sync> = unit.clone();
    canon
        .members
        .iter()
        .zip(&unit.entries)
        .filter_map(|(&m, e)| {
            let entry = (*e)?;
            Some((
                m,
                Installed {
                    entry,
                    consts: consts.clone(),
                    funcs: funcs.clone(),
                    classes: classes.clone(),
                    keep_alive: keep_alive.clone(),
                },
            ))
        })
        .collect()
}

/// Compiles the eligible functions of canonical program `prog` into one
/// unit; function 0 is the root.
fn compile_unit(prog: &Program, loop_prop_cache: bool) -> Unit {
    let none = Unit { code: None, entries: vec![None; prog.funcs.len()] };
    let checked = eligible(prog);
    if !checked.contains_key(&0) {
        return none;
    }
    build(prog, &checked, loop_prop_cache).unwrap_or(none)
}

/// Compiles the functions in `checked` (see `compile_unit`); `None` if
/// Cranelift refuses something.
fn build(prog: &Program, checked: &HashMap<u32, Checked>, loop_prop_cache: bool) -> Option<Unit> {
    let mut jb = JITBuilder::with_isa(isa()?, default_libcall_names());
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
            for (ins, st) in func.code.iter().zip(&checked[&f].kinds) {
                // (unreachable calls were not checked)
                let Some(st) = st else { continue };
                let targets = match *ins {
                    Instr::Call { func: c, .. } => vec![c],
                    Instr::CallVirtual { slot, args, .. } => {
                        let class = st.get(args as usize).and_then(|k| k.class());
                        class.and_then(|c| virtual_targets(prog, c, slot)).unwrap_or_default()
                    }
                    _ => continue,
                };
                for c in targets {
                    if let Some((id, _)) = ids.get(c as usize).copied().flatten() {
                        callees.entry(c).or_insert_with(|| module.declare_func_in_func(id, b.func));
                    }
                }
            }
            let t = Translator::new(&mut b, prog, func, &checked[&f], helpers, callees, f);
            t.translate(loop_prop_cache);
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
    Some(Unit { code: Some(Code(Some(module))), entries })
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
    /// Per register some instruction leaves stale (`Checked::stale`): 1 while
    /// its window slot may hold a value to let go (see `Checked::fills`).
    held: HashMap<Reg, Variable>,
    blocks: Vec<Block>,
    /// Branches back to an instruction (a loop's test at its end): the block
    /// each goes through, which counts the budget, and the target's block.
    back_edges: Vec<(Block, Block)>,
    ctx: Value,
    /// The function's index in the unit, and the position after the
    /// instruction being translated (for `JitRt::trace`).
    fid: u32,
    pc: u32,
}

impl<'a, 'b> Translator<'a, 'b> {
    fn new(
        b: &'a mut FunctionBuilder<'b>,
        prog: &'a Program,
        func: &'a Function,
        checked: &'a Checked,
        helpers: Vec<FuncRef>,
        callees: HashMap<u32, FuncRef>,
        fid: u32,
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
            held: HashMap::new(),
            blocks: Vec::new(),
            back_edges: Vec::new(),
            ctx: Value::from_u32(0),
            fid,
            pc: 0,
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
            // Every instruction that makes an object register sets its null
            // flag (see `obj_flag`).
            K::OptObj(_) => self.getn(r),
            K::Opt(_) => self.getn(r),
            _ => self.iconst(0),
        }
    }

    /// Sets the null flag of object register `r`, whose value is in its
    /// slot: read from the slot when it may be null. Objects are tested for
    /// null by this flag, so every instruction that makes one sets it.
    fn obj_flag(&mut self, r: Reg, nullable: bool) {
        if nullable {
            let n = self.call_regs(H::IsNull, &[r as i64]).unwrap();
            self.b.def_var(self.nvars[r as usize], n);
        } else {
            self.setn(r, 0);
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

    /// Stores `code` in `ctx.error` and returns, after noting this frame for
    /// the trace of an exception. The VM drops the windows.
    fn fail(&mut self, code: Value) {
        let (f, pc) = (self.fid as i64, self.pc as i64);
        self.call_regs(H::Unwind, &[f, pc]);
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

    /// Ends instruction `i`, which goes on to `next`: a slot that held a
    /// string, list, map or object of a register it made a number lets the
    /// value go (see `stale_slots`), as the interpreter's write does.
    fn after(&mut self, i: usize, next: u32) {
        for &r in &self.checked.stale[i] {
            // Only when the slot holds a value: in a loop it does at the
            // first pass, and a call per pass cost as much as the rest of
            // a method call.
            let var = self.held[&r];
            let held = self.b.use_var(var);
            let (put, cont) = (self.b.create_block(), self.b.create_block());
            self.b.ins().brif(held, put, &[], cont, &[]);
            self.b.switch_to_block(put);
            self.moved_out(r);
            self.b.ins().jump(cont, &[]);
            self.b.switch_to_block(cont);
            let zero = self.iconst(0);
            self.b.def_var(var, zero);
        }
        self.jump(next);
    }

    /// Fails with the error a helper left in `ctx.error`, if any.
    fn check_ctx(&mut self) {
        let e = self.b.ins().load(I64, self.mf, self.ctx, CTX_ERROR);
        self.check(e);
    }

    fn read_scalar_prop(&mut self, object: Reg, slot: u16, kind: i64, cache: Option<(Variable, Variable)>) -> Value {
        let Some((ready, bits)) = cache else {
            let value = self.call_regs(H::GetPropScalar, &[object as i64, slot as i64, kind]).unwrap();
            self.check_ctx();
            return value;
        };

        // Read at the original instruction on its first visit. A skipped
        // branch or zero-trip loop must not perform the read or raise its error.
        let read = self.b.create_block();
        let done = self.b.create_block();
        let initialized = self.b.use_var(ready);
        self.b.ins().brif(initialized, done, &[], read, &[]);
        self.b.switch_to_block(read);
        let value = self.call_regs(H::GetPropScalar, &[object as i64, slot as i64, kind]).unwrap();
        self.check_ctx();
        self.b.def_var(bits, value);
        let one = self.iconst(1);
        self.b.def_var(ready, one);
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(done);
        self.b.use_var(bits)
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
            K::Str | K::List(_) | K::Map(..) | K::Opt(E::Str) | K::Obj(_) | K::OptObj(_) => return,
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

    /// `a == b` (of kinds `ka` and `kb`, checked by `eq_mode`) as 0 or 1.
    fn eq_flag(&mut self, a: Reg, b: Reg, ka: K, kb: K) -> Value {
        match eq_mode(ka, kb) {
            Some(EqMode::Plain) if ka == K::Str => self.call_regs(H::StrEq, &[a as i64, b as i64]).unwrap(),
            Some(EqMode::Plain) => {
                let (x, y) = (self.get(a), self.get(b));
                let c = self.b.ins().icmp(IntCC::Equal, x, y);
                self.bool_val(c)
            }
            Some(EqMode::BothNull) => self.iconst(1),
            Some(EqMode::NullTest(true)) => self.is_null(a, ka),
            Some(EqMode::NullTest(false)) => self.is_null(b, kb),
            Some(EqMode::Opt(e)) => {
                // Both null, or neither and the same value.
                let (na, nb) = (self.is_null(a, ka), self.is_null(b, kb));
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
            None => unreachable!("checked"),
        }
    }

    fn jump(&mut self, target: u32) {
        let blk = self.blocks[target as usize];
        self.b.ins().jump(blk, &[]);
    }

    /// The block a branch of instruction `from` to `t` goes to: the target's,
    /// or for a branch back, one that first counts the budget (filled at the
    /// end of `translate`), as `Jump` does.
    fn edge(&mut self, from: usize, t: u32) -> Block {
        let blk = self.blocks[t as usize];
        if t as usize > from {
            return blk;
        }
        let through = self.b.create_block();
        self.back_edges.push((through, blk));
        through
    }

    /// A direct call of function `func` of the unit with the `argc`
    /// arguments from register `args` (of kinds `st`), its result into `dst`.
    fn emit_call(&mut self, func: u32, dst: Reg, args: Reg, argc: u16, st: &[K]) {
        let k = |r: Reg| st[r as usize];
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
                match v {
                    ValKind::Opt(s) => self.unbox(dst, E::from(s)),
                    ValKind::Object(_) => self.obj_flag(dst, false),
                    ValKind::OptObject(_) => self.obj_flag(dst, true),
                    _ => {}
                }
            }
            _ => self.set(dst, r),
        }
    }

    fn translate(mut self, loop_prop_cache: bool) {
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
        let mut cached_props = HashMap::new();
        if loop_prop_cache {
            for i in loop_prop_reads(self.prog, self.func, self.checked) {
                let ready = self.b.declare_var(I64);
                let bits = self.b.declare_var(I64);
                self.b.def_var(ready, zero);
                self.b.def_var(bits, zero);
                cached_props.insert(i, (ready, bits));
            }
        }
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
        // Whether the slot of a register that goes stale holds a value: at
        // first only an argument's does (the window comes empty).
        for r in self.checked.stale.iter().flatten().copied().collect::<std::collections::BTreeSet<Reg>>() {
            let var = self.b.declare_var(I64);
            let boxed = self.sig.params.get(r as usize).is_some_and(|p| p.boxed());
            let v = self.iconst(boxed as i64);
            self.b.def_var(var, v);
            self.held.insert(r, var);
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
            // Nullable scalar arguments come in the window; so does
            // whether a nullable object is null.
            for (r, p) in self.sig.params.iter().enumerate() {
                match p {
                    ValKind::Opt(s) => self.unbox(r as Reg, E::from(*s)),
                    ValKind::OptObject(_) => self.obj_flag(r as Reg, true),
                    _ => {}
                }
            }
        }
        self.tick();
        self.jump(0);

        for (i, ins) in code.iter().enumerate() {
            self.b.switch_to_block(self.blocks[i]);
            self.pc = (i + 1) as u32;
            let Some(st) = &self.checked.kinds[i] else {
                // Never reached (e.g. the `return null` after the last `return`).
                self.b.ins().trap(TrapCode::unwrap_user(1));
                continue;
            };
            let k = |r: Reg| st[r as usize];
            let next = (i + 1) as u32;
            for &(r, shared) in &self.checked.fills[i] {
                let v = self.iconst(shared as i64);
                self.b.def_var(self.held[&r], v);
            }
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
                        if ks.class().is_some() {
                            let n = self.getn(s);
                            self.b.def_var(self.nvars[d as usize], n);
                        }
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
                    let (yes, no) = (self.edge(i, yes), self.edge(i, no));
                    self.b.ins().brif(null, yes, &[], no, &[]);
                    continue;
                }
                Instr::Eq(d, a, b) | Instr::Ne(d, a, b) => {
                    let eq = self.eq_flag(a, b, k(a), k(b));
                    let r = if matches!(ins, Instr::Ne(..)) { self.b.ins().bxor_imm_s(eq, 1) } else { eq };
                    self.set(d, r);
                }
                Instr::JumpIfEq(a, b, t) | Instr::JumpIfNotEq(a, b, t) => {
                    let eq = self.eq_flag(a, b, k(a), k(b));
                    let (yes, no) = if matches!(ins, Instr::JumpIfEq(..)) { (t, next) } else { (next, t) };
                    let (yes, no) = (self.edge(i, yes), self.edge(i, no));
                    self.b.ins().brif(eq, yes, &[], no, &[]);
                    continue;
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
                Instr::Lt(d, a, b) | Instr::Le(d, a, b) | Instr::Cmp(d, a, b) if k(a).strip() == K::Str => {
                    // Compare the strings (-1, 0 or 1), then that with 0.
                    let c = self.call_regs(H::StrCmp, &[a as i64, b as i64]).unwrap();
                    let r = match ins {
                        Instr::Cmp(..) => c,
                        _ => {
                            let cc = match ins {
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
                | Instr::LtI(d, a, b)
                | Instr::Lt(d, a, b)
                | Instr::LeI(d, a, b)
                | Instr::Le(d, a, b) => {
                    let cc = match ins {
                        Instr::EqI(..) => IntCC::Equal,
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
                    let (yes, no) = (self.edge(i, yes), self.edge(i, no));
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
                    let (yes, no) = (self.edge(i, next), self.edge(i, t));
                    self.b.ins().brif(c, yes, &[], no, &[]);
                    continue;
                }
                Instr::JumpIfNotLtIImm(a, n, t)
                | Instr::JumpIfNotLeIImm(a, n, t)
                | Instr::JumpIfLtIImm(a, n, t)
                | Instr::JumpIfLeIImm(a, n, t) => {
                    let x = self.get(a);
                    let cc = if matches!(ins, Instr::JumpIfNotLtIImm(..) | Instr::JumpIfLtIImm(..)) {
                        IntCC::SignedLessThan
                    } else {
                        IntCC::SignedLessThanOrEqual
                    };
                    let c = self.b.ins().icmp_imm_s(cc, x, n as i64);
                    // The `Not` forms jump when the comparison fails.
                    let jumps_when = matches!(ins, Instr::JumpIfLtIImm(..) | Instr::JumpIfLeIImm(..));
                    let (yes, no) = if jumps_when { (t, next) } else { (next, t) };
                    let (yes, no) = (self.edge(i, yes), self.edge(i, no));
                    self.b.ins().brif(c, yes, &[], no, &[]);
                    continue;
                }
                Instr::Call { dst, func, args, argc } => self.emit_call(func, dst, args, argc, st),
                Instr::CallVirtual { dst, slot, args, argc } => {
                    let class = k(args).class().expect("checked");
                    let targets = virtual_targets(self.prog, class, slot).expect("checked");
                    // One function in that slot for the class and every class
                    // that extends it (the unit has them all, see `canonical`):
                    // the call is direct.
                    if let [only] = targets[..] {
                        self.emit_call(only, dst, args, argc, st);
                        self.after(i, next);
                        continue;
                    }
                    let t = self.call_regs(H::VTarget, &[args as i64, slot as i64]).unwrap();
                    self.check_ctx();
                    // The program's id of each function of the unit it may be.
                    let funcs = self.b.ins().load(I64, self.mf, self.ctx, CTX_FUNCS);
                    let join = self.b.create_block();
                    for f in targets {
                        let id = self.b.ins().uload32(self.mf, funcs, (f * 4) as i32);
                        let is = self.b.ins().icmp(IntCC::Equal, t, id);
                        let (yes, no) = (self.b.create_block(), self.b.create_block());
                        self.b.ins().brif(is, yes, &[], no, &[]);
                        self.b.switch_to_block(yes);
                        self.emit_call(f, dst, args, argc, st);
                        self.b.ins().jump(join, &[]);
                        self.b.switch_to_block(no);
                    }
                    self.call(H::Bad, &[]);
                    let c = self.iconst(JIT_ERR_THROW);
                    self.fail(c);
                    self.b.switch_to_block(join);
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
                    // A long key may have passed the time limit.
                    self.check_ctx();
                }
                Instr::SetPath(ref path) if matches!(path.base, PathBase::Prop(..)) => {
                    let PathBase::Prop(o, slot) = path.base else { unreachable!("checked") };
                    let key = path.keys.first().copied();
                    if let Some(key) = key {
                        self.spill(key, k(key));
                    }
                    self.spill(path.val, k(path.val));
                    let kv = key.map_or(-1, |r| r as i64);
                    let args = [o as i64, slot as i64, kv, path.push as i64, path.val as i64];
                    self.call_regs(H::SetPropPath, &args);
                    self.check_ctx();
                }
                Instr::AppendPath { ref path, cur } if matches!(path.base, PathBase::Prop(..)) => {
                    let PathBase::Prop(o, slot) = path.base else { unreachable!("checked") };
                    self.spill(path.val, k(path.val));
                    self.call_regs(H::AppendProp, &[o as i64, slot as i64, cur as i64, path.val as i64]);
                    self.check_ctx();
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
                Instr::AppendPath { ref path, cur } => {
                    let PathBase::Local(r) = path.base else { unreachable!("checked") };
                    let key = path.keys[0];
                    self.spill(key, k(key));
                    self.spill(path.val, k(path.val));
                    self.call_regs(H::AppendKey, &[r as i64, key as i64, cur as i64, path.val as i64]);
                    self.check_ctx();
                }
                Instr::IterInit(it, src) => {
                    if it != src {
                        self.call_regs(H::Move, &[it as i64, src as i64]);
                    }
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
                    let end = self.edge(i, end);
                    self.b.ins().brif(done, end, &[], cont, &[]);
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
                    let end = self.edge(i, end);
                    self.b.ins().brif(done, end, &[], cont, &[]);
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
                Instr::NewObject(d, c) => {
                    self.call_regs(H::New, &[d as i64, c as i64]);
                    self.check_ctx();
                    self.setn(d, 0);
                }
                Instr::GetProp(d, o, slot) => {
                    let fk = field_kind(self.prog, k(o), slot).expect("checked");
                    let (code, nullable) = fk.elem().expect("checked");
                    let v = if matches!(fk, K::Int | K::Bool | K::Float) {
                        self.read_scalar_prop(o, slot, code, cached_props.get(&i).copied())
                    } else {
                        let value = self
                            .call_regs(H::GetProp, &[d as i64, o as i64, slot as i64, code, nullable as i64])
                            .unwrap();
                        self.check_ctx();
                        value
                    };
                    match fk {
                        K::Float | K::Opt(E::Float) => {
                            let f = self.b.ins().bitcast(F64, MemFlagsData::new(), v);
                            self.setf(d, f);
                        }
                        K::Int | K::Bool | K::Opt(E::Int) | K::Opt(E::Bool) => self.set(d, v),
                        _ => {}
                    }
                    if matches!(fk, K::Opt(E::Int | E::Bool | E::Float) | K::OptObj(_)) {
                        let null = self.b.ins().load(I64, self.mf, self.ctx, CTX_DETAIL);
                        self.b.def_var(self.nvars[d as usize], null);
                    } else if matches!(fk, K::Obj(_)) {
                        self.setn(d, 0);
                    }
                }
                Instr::SetProp(o, slot, v) if matches!(k(v), K::Int | K::Bool | K::Float | K::Null) => {
                    let (tag, bits) = match k(v) {
                        K::Float => {
                            let f = self.getf(v);
                            (JIT_PUT_FLOAT, self.b.ins().bitcast(I64, MemFlagsData::new(), f))
                        }
                        K::Bool => (JIT_PUT_BOOL, self.get(v)),
                        K::Null => (JIT_PUT_NULL, self.iconst(0)),
                        _ => (JIT_PUT_INT, self.get(v)),
                    };
                    let (ov, sv, tv) = (self.iconst(o as i64), self.iconst(slot as i64), self.iconst(tag));
                    self.call(H::SetPropScalar, &[ov, sv, tv, bits]);
                    self.check_ctx();
                }
                Instr::SetProp(o, slot, v) => {
                    self.spill(v, k(v));
                    self.call_regs(H::SetProp, &[o as i64, slot as i64, v as i64]);
                    self.check_ctx();
                }
                Instr::Echo(r) | Instr::EchoAttr(r) => {
                    self.spill(r, k(r));
                    let attr = matches!(ins, Instr::EchoAttr(_)) as i64;
                    let e = self.call_regs(H::Echo, &[r as i64, attr]).unwrap();
                    self.check(e);
                }
                Instr::EchoHtml(c) => {
                    let e = self.call_regs(H::EchoHtml, &[c as i64]).unwrap();
                    self.check(e);
                }
                Instr::GuardOutput | Instr::EndGuard => {
                    self.call_regs(H::Guard, &[matches!(ins, Instr::EndGuard) as i64]);
                }
                Instr::EchoNonce => {
                    let e = self.call(H::EchoNonce, &[]).unwrap();
                    self.check(e);
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
            self.after(i, next);
        }
        for (through, blk) in std::mem::take(&mut self.back_edges) {
            self.b.switch_to_block(through);
            self.tick();
            self.b.ins().jump(blk, &[]);
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
        well_formed(prog, &prog.funcs[0], &vec![true; prog.funcs.len()]).is_some()
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

    /// A register made a number where it held a list read from a parameter
    /// lets the list go after the instruction (and only there); one that held
    /// a constant string does not (audit 20).
    #[test]
    fn numbers_written_over_values_empty_their_slots() {
        let list = ValKind::List(phv_vm::bytecode::Scalar::Int);
        let f = func(
            vec![list, I],
            I,
            5,
            vec![Move(2, 0), Index(2, 2, 1), Const(3, 0), Int(3, 1), AddI(4, 2, 3), Int(2, 0), Ret(4)],
        );
        let prog = Program { funcs: vec![f], consts: vec![C::Str("a".into())], ..Default::default() };
        let checked = well_formed(&prog, &prog.funcs[0], &[true]).expect("compiled");
        let stale: Vec<(usize, Vec<Reg>)> =
            checked.stale.iter().enumerate().filter(|(_, s)| !s.is_empty()).map(|(i, s)| (i, s.clone())).collect();
        assert_eq!(stale, vec![(1, vec![2])]);
        // Native code empties the slot only while it holds a value: the
        // `Move` fills it (the only register that goes stale).
        let fills: Vec<(usize, Vec<(Reg, bool)>)> =
            checked.fills.iter().enumerate().filter(|(_, s)| !s.is_empty()).map(|(i, s)| (i, s.clone())).collect();
        assert_eq!(fills, vec![(0, vec![(2, true)])]);
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
        // `Concat` of no registers.
        assert!(!one(func(vec![S], S, 2, vec![Concat(1, 9, 0, false), Ret(1)])));
        // A branch back is compiled: it counts toward the time limit's check
        // as `Jump` does (a loop's test at its end).
        assert!(one(func(vec![I], ValKind::Void, 1, vec![JumpIfNotLtIImm(0, 5, 0), RetNull])));
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
        assert!(well_formed(&prog, &prog.funcs[0], &[true, false]).is_none());
    }

    /// A long chain of calls whose last function cannot be compiled: one
    /// function drops out per pass of a fixpoint over all of them (quadratic),
    /// and once each with the worklist.
    #[test]
    fn eligibility_of_long_call_chains_takes_linear_time() {
        let n = 20_000u32;
        let mut funcs: Vec<Function> = (0..n - 1)
            .map(|i| func(vec![I], I, 2, vec![Call { dst: 1, func: i + 1, args: 0, argc: 1 }, Ret(1)]))
            .collect();
        funcs.push(func(vec![I], I, 1, vec![Throw(0), Ret(0)]));
        let prog = Program { funcs, ..Default::default() };
        let start = std::time::Instant::now();
        assert!(eligible(&prog).is_empty());
        assert!(start.elapsed() < std::time::Duration::from_secs(10), "took {:?}", start.elapsed());
        // Without the last one, the whole chain is compiled.
        let mut prog = prog;
        prog.funcs.pop();
        prog.funcs.push(func(vec![I], I, 1, vec![Ret(0)]));
        assert_eq!(eligible(&prog).len(), n as usize);
    }

    #[test]
    fn rejects_bad_signatures() {
        let mut f = func(vec![S], S, 1, vec![Ret(0)]);
        f.num_params = 2;
        assert!(!one(f));
        assert!(!one(func(vec![ValKind::Void], ValKind::Void, 1, vec![RetNull])));
        assert!(!one(func(vec![I; 17], I, 17, vec![Ret(0)])));
    }

    /// Programs whose constants differ never share a unit, whatever the
    /// constants' bytes spell: the key is a serialization, not text.
    #[test]
    fn unit_keys_tell_constants_apart() {
        let key = |consts: Vec<C>, code: Vec<Instr>| {
            let prog = Program { funcs: vec![func(vec![], S, 3, code)], consts, ..Default::default() };
            canonical(&prog, 0).key
        };
        let to_str = vec![Instr::Const(1, 0), ToStr(2, 1), Ret(2)];
        let one = |c: C| key(vec![c], to_str.clone());
        // A float whose bits spell the text of another constant (before
        // 1.2.1 the key held a float's raw bytes between the other
        // constants' texts, and these were the same key).
        let spells = |text: &[u8; 8]| C::Float(f64::from_bits(u64::from_le_bytes(*text)));
        assert_ne!(one(spells(b"Int(100)")), one(C::Int(100)));
        assert_ne!(one(spells(b"Str(\"a\")")), one(C::Str("a".into())));
        // Floats by their bits: the sign of zero, a byte that is a newline,
        // the payload of a NaN.
        assert_ne!(one(C::Float(0.0)), one(C::Float(-0.0)));
        let nl = f64::from_bits(0x4000_0000_0000_000A);
        assert!(nl.to_bits().to_le_bytes().contains(&b'\n'));
        assert_ne!(one(C::Float(2.0)), one(C::Float(nl)));
        assert_ne!(one(C::Float(f64::NAN)), one(C::Float(f64::from_bits(f64::NAN.to_bits() | 1))));
        // A newline in a string is not a separator.
        assert_ne!(one(C::Str("a\n".into())), one(C::Str("a".into())));
        let two = vec![Instr::Const(1, 0), Instr::Const(2, 1), Concat(1, 1, 2, false), Ret(1)];
        let pair = |a: &str, b: &str| key(vec![C::Str(a.into()), C::Str(b.into())], two.clone());
        assert_ne!(pair("a\n", "b"), pair("a", "\nb"));
        assert_ne!(pair("ab", ""), pair("a", "b"));
        // Every kind of constant has its own tag.
        let kinds = [
            C::Null,
            C::Bool(false),
            C::Int(0),
            C::Float(0.0),
            C::Str(String::new()),
            C::EmptyList,
            C::EmptyMap,
            C::EmptySet,
        ];
        let keys: std::collections::HashSet<Vec<u8>> = kinds.iter().map(|c| one(c.clone())).collect();
        assert_eq!(keys.len(), kinds.len());
        // The number of constants: a unit names constants by index.
        let twice = vec![Instr::Const(1, 0), Instr::Const(2, 0), Concat(1, 1, 2, false), Ret(1)];
        assert_ne!(pair("a", "a"), key(vec![C::Str("a".into())], twice));
        // Builtins by their names, each delimited.
        let call = vec![Instr::Const(1, 0), CallBuiltin { dst: 2, id: 0, args: 1, argc: 1 }, Ret(2)];
        let with = |names: &[&str]| Program {
            funcs: vec![func(vec![], I, 3, call.clone())],
            consts: vec![C::Str("a".into())],
            builtin_names: names.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        assert_ne!(canonical(&with(&["strlen"]), 0).key, canonical(&with(&["strlen\n"]), 0).key);
        // The same text numbered differently is the same key.
        let a = Program {
            funcs: vec![func(vec![], S, 3, vec![Instr::Const(1, 1), ToStr(2, 1), Ret(2)])],
            consts: vec![C::Str("z".into()), C::Float(1.0)],
            ..Default::default()
        };
        assert_eq!(canonical(&a, 0).key, one(C::Float(1.0)));
    }

    fn class(name: &str, fields: &[(&str, ValKind)], supers: Vec<u32>, vtable: Vec<u32>) -> Class {
        Class {
            name: name.into(),
            // (in these tests, a class extends at most the first one)
            parent: (supers.len() > 1).then(|| supers[0]),
            fields: fields
                .iter()
                .map(|(n, k)| Field { name: n.to_string(), default: C::Int(0), public: true, kind: Some(*k) })
                .collect(),
            vtable,
            supers,
            by_name: Vec::new(),
            cases: Vec::new(),
            acyclic: true,
        }
    }

    /// Units that read properties by slot or call through method tables
    /// are shared only between programs whose classes have the same layout
    /// (the kinds and names of their properties, what they extend, their
    /// method tables): the key holds them. Defaults and the numbering of
    /// classes do not change the code, and do not change the key.
    #[test]
    fn unit_keys_tell_class_layouts_apart() {
        let o = ValKind::Object(0);
        let key = |classes: Vec<Class>| {
            let read = func(vec![o], S, 3, vec![GetProp(1, 0, 0), ToStr(2, 1), Ret(2)]);
            let prog = Program { funcs: vec![read], classes, ..Default::default() };
            assert!(accepts(&prog));
            canonical(&prog, 0).key
        };
        let base = key(vec![class("P", &[("v", I)], vec![0], vec![])]);
        assert_ne!(base, key(vec![class("P", &[("v", F)], vec![0], vec![])]), "another kind");
        assert_ne!(base, key(vec![class("P", &[("w", I)], vec![0], vec![])]), "another name");
        assert_ne!(base, key(vec![class("P", &[("v", I), ("w", I)], vec![0], vec![])]), "another property");
        assert_ne!(base, key(vec![class("Q", &[("v", I)], vec![0], vec![])]), "another class");
        // A subclass can reach the code (a call through the method table).
        let sub = vec![class("P", &[("v", I)], vec![0], vec![]), class("R", &[("v", I)], vec![0, 1], vec![])];
        assert_ne!(base, key(sub), "a subclass");
        // The same layout: other defaults, or the class numbered otherwise.
        let mut other = class("P", &[("v", I)], vec![0], vec![]);
        other.fields[0].default = C::Int(5);
        assert_eq!(base, key(vec![other]));
        let moved = Program {
            funcs: vec![func(vec![ValKind::Object(1)], S, 3, vec![GetProp(1, 0, 0), ToStr(2, 1), Ret(2)])],
            classes: vec![class("Z", &[("x", F)], vec![0], vec![]), class("P", &[("v", I)], vec![1], vec![])],
            ..Default::default()
        };
        assert_eq!(base, canonical(&moved, 0).key);
        // Method tables: the function in a slot is part of the key.
        let call = |ret: i64| {
            let caller = func(vec![o], I, 2, vec![CallVirtual { dst: 1, slot: 0, args: 0, argc: 1 }, Ret(1)]);
            let method = func(vec![o], I, 2, vec![Int(1, ret), Ret(1)]);
            let prog = Program {
                funcs: vec![caller, method],
                classes: vec![class("P", &[], vec![0], vec![1])],
                ..Default::default()
            };
            assert!(accepts(&prog));
            canonical(&prog, 0).key
        };
        assert_ne!(call(1), call(2));
        assert_eq!(call(1), call(1));
        // A property of a kind native code does not hold is not read natively.
        let mut none = class("P", &[("v", I)], vec![0], vec![]);
        none.fields[0].kind = None;
        let prog = Program {
            funcs: vec![func(vec![o], I, 2, vec![GetProp(1, 0, 0), Ret(1)])],
            classes: vec![none],
            ..Default::default()
        };
        assert!(!accepts(&prog));
    }

    /// Objects: a property is read and written with the kind its class
    /// declares, a method call through the method table needs every function
    /// that may receive it to take the arguments and return the same kind,
    /// and an object of another class fits only where a class it extends is
    /// expected.
    #[test]
    fn checks_the_kinds_of_objects() {
        let (o, o1) = (ValKind::Object(0), ValKind::Object(1));
        let classes = || vec![class("P", &[("v", I)], vec![0], vec![1]), class("Q", &[("v", I)], vec![0, 1], vec![2])];
        let with = |funcs: Vec<Function>| Program { funcs, classes: classes(), ..Default::default() };
        // Reading an int property gives an int; writing a float to it is refused.
        assert!(accepts(&with(vec![func(vec![o], I, 2, vec![GetProp(1, 0, 0), Ret(1)])])));
        assert!(!accepts(&with(vec![func(vec![o], F, 2, vec![GetProp(1, 0, 0), Ret(1)])])));
        let prog = Program {
            funcs: vec![func(vec![o, F], ValKind::Void, 2, vec![SetProp(0, 0, 1), RetNull])],
            classes: classes(),
            ..Default::default()
        };
        assert!(!accepts(&prog));
        // A property that is not there, or a value that is not an object.
        assert!(!accepts(&with(vec![func(vec![o], I, 2, vec![GetProp(1, 0, 3), Ret(1)])])));
        assert!(!accepts(&with(vec![func(vec![I], I, 2, vec![GetProp(1, 0, 0), Ret(1)])])));
        // A subclass where the class is expected, not the other way round.
        let callee = |p: ValKind| func(vec![p], I, 1, vec![Int(0, 1), Ret(0)]);
        let caller = |p: ValKind| func(vec![p], I, 2, vec![Call { dst: 1, func: 1, args: 0, argc: 1 }, Ret(1)]);
        assert!(accepts(&with(vec![caller(o1), callee(o)])));
        assert!(!accepts(&with(vec![caller(o), callee(o1)])));
        // Through the method table: P's slot 0 is function 1, Q's is 2.
        let virt = |ret2: ValKind| {
            with(vec![
                func(vec![o], I, 2, vec![CallVirtual { dst: 1, slot: 0, args: 0, argc: 1 }, Ret(1)]),
                func(vec![o], I, 1, vec![Int(0, 1), Ret(0)]),
                func(vec![o1], ret2, 1, vec![Int(0, 2), Ret(0)]),
            ])
        };
        assert!(accepts(&virt(I)));
        assert!(!accepts(&virt(ValKind::Opt(Scalar::Int))), "another return kind");
        assert_eq!(virtual_targets(&virt(I), 0, 0), Some(vec![1, 2]));
        assert_eq!(virtual_targets(&virt(I), 1, 0), Some(vec![2]));
    }
}
