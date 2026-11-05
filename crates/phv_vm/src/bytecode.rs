//! Compiled program format. A `Program` holds no `Rc` values, so it can be
//! shared between request threads behind an `Arc`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub type Reg = u16;
/// Placeholder register for an absent operand (e.g. no foreach key).
pub const NO_REG: Reg = u16::MAX;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Const {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    EmptyList,
    EmptyMap,
    EmptySet,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TypeTag {
    Null,
    Bool,
    Int,
    Float,
    Str,
    Html,
    List,
    Map,
    Set,
    Closure,
    Object,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum PathBase {
    Local(Reg),
    Prop(Reg, u16),
}

/// `$a[k1][k2] = v` or `$a[k1][] = v` on a local or a property.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PathSet {
    pub base: PathBase,
    pub keys: Vec<Reg>,
    /// For each key: the empty container to create when it is missing.
    pub create: Vec<Option<Const>>,
    pub push: bool,
    pub val: Reg,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Instr {
    Const(Reg, u32),
    Null(Reg),
    Bool(Reg, bool),
    Int(Reg, i64),
    Move(Reg, Reg),

    AddI(Reg, Reg, Reg),
    SubI(Reg, Reg, Reg),
    MulI(Reg, Reg, Reg),
    ModI(Reg, Reg, Reg),
    PowI(Reg, Reg, Reg),
    NegI(Reg, Reg),
    IncI(Reg, i64),
    /// `dst = a + imm` / `dst = a - imm` with a small constant.
    AddIImm(Reg, Reg, i32),
    SubIImm(Reg, Reg, i32),
    AddF(Reg, Reg, Reg),
    SubF(Reg, Reg, Reg),
    MulF(Reg, Reg, Reg),
    DivF(Reg, Reg, Reg),
    ModF(Reg, Reg, Reg),
    PowF(Reg, Reg, Reg),
    NegF(Reg, Reg),
    BitAnd(Reg, Reg, Reg),
    BitOr(Reg, Reg, Reg),
    BitXor(Reg, Reg, Reg),
    Shl(Reg, Reg, Reg),
    Shr(Reg, Reg, Reg),
    BitNot(Reg, Reg),
    Not(Reg, Reg),

    EqI(Reg, Reg, Reg),
    LtI(Reg, Reg, Reg),
    LeI(Reg, Reg, Reg),
    Eq(Reg, Reg, Reg),
    Ne(Reg, Reg, Reg),
    Lt(Reg, Reg, Reg),
    Le(Reg, Reg, Reg),
    Cmp(Reg, Reg, Reg),
    /// `==`, `!=`, `<`, `<=` and `<=>` on two floats (the compiler converts an
    /// int operand first), with the same results as the generic forms.
    EqF(Reg, Reg, Reg),
    NeF(Reg, Reg, Reg),
    LtF(Reg, Reg, Reg),
    LeF(Reg, Reg, Reg),
    CmpF(Reg, Reg, Reg),

    IntToFloat(Reg, Reg),
    FloatToInt(Reg, Reg),
    ToStr(Reg, Reg),
    /// dst, first operand, count, produce `Html` instead of `string`.
    Concat(Reg, Reg, u16, bool),
    /// `$s .= x` on a local string, appended in place when not shared.
    AppendStr(Reg, Reg),

    Jump(u32),
    JumpIfFalse(Reg, u32),
    JumpIfTrue(Reg, u32),
    JumpIfNull(Reg, u32),
    JumpIfNotNull(Reg, u32),
    JumpIfNotLtI(Reg, Reg, u32),
    JumpIfNotLeI(Reg, Reg, u32),
    JumpIfNotLtIImm(Reg, i32, u32),
    JumpIfNotLeIImm(Reg, i32, u32),
    /// Jumps when the int is less than (or at most) the constant: the test at
    /// the end of a loop, which jumps back to its start.
    JumpIfLtIImm(Reg, i32, u32),
    JumpIfLeIImm(Reg, i32, u32),

    Call {
        dst: Reg,
        func: u32,
        args: Reg,
        argc: u16,
    },
    CallVirtual {
        dst: Reg,
        slot: u32,
        args: Reg,
        argc: u16,
    },
    CallClosure {
        dst: Reg,
        callee: Reg,
        args: Reg,
        argc: u16,
    },
    CallBuiltin {
        dst: Reg,
        id: u32,
        args: Reg,
        argc: u16,
    },
    /// Method call through an interface: looked up by interned name.
    CallNamed {
        dst: Reg,
        name: u32,
        args: Reg,
        argc: u16,
    },
    Ret(Reg),
    RetNull,
    /// The statement at this source offset (of the function's file) runs.
    /// Only in programs compiled with statement markers (`phv test --coverage`, `phv dap`).
    Cover(u32),
    /// The function starts. Only in programs compiled for `phv run --profile`.
    Enter,

    NewObject(Reg, u32),
    GetProp(Reg, Reg, u16),
    SetProp(Reg, u16, Reg),
    NewList(Reg, Reg, u16),
    NewMap(Reg, Reg, u16),
    Range(Reg, Reg, Reg, bool),
    Index(Reg, Reg, Reg),
    IndexOrNull(Reg, Reg, Reg),
    SetPath(Box<PathSet>),

    /// Iterator state lives in two registers: the collection and the position.
    IterInit(Reg, Reg),
    IterNext {
        it: Reg,
        key: Reg,
        val: Reg,
        end: u32,
    },

    Throw(Reg),
    TryBegin {
        catch: u32,
        exc: Reg,
    },
    TryEnd,
    InstanceOf(Reg, Reg, u32),
    IsType(Reg, Reg, TypeTag),
    MatchError(Reg),
    EnumCase(Reg, u32, u32),
    EnumFrom {
        dst: Reg,
        class: u32,
        val: Reg,
        try_: bool,
    },
    EnumCases(Reg, u32),

    MakeClosure {
        dst: Reg,
        func: u32,
        caps: Reg,
        n: u16,
    },

    Echo(Reg),
    /// `Echo` into a quoted attribute value: `"` and `'` in `Html` are encoded.
    EchoAttr(Reg),
    EchoHtml(u32),
    /// ` nonce="..."` when the server sent a CSP nonce, else nothing.
    EchoNonce,
    PushOut,
    PopOutHtml(Reg),
    /// Statements in HTML other than element text start: output from
    /// functions called until `EndGuard` is refused (a runtime error).
    GuardOutput,
    EndGuard,

    /// `$o->p .= x`, `$a[k] .= x` and `$o->p[k] .= x`: `cur` holds the value
    /// read from the place before `x` (`path.val`) was evaluated. When the
    /// place still holds that string, `x` is appended to it in place;
    /// otherwise (`x` changed the place) the place gets `cur . x`.
    AppendPath {
        path: Box<PathSet>,
        cur: Reg,
    },
    /// Jumps when the two registers are equal (`==`), or are not.
    JumpIfEq(Reg, Reg, u32),
    JumpIfNotEq(Reg, Reg, u32),
}

/// Kinds of values a natively compiled function takes and returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ValKind {
    Int,
    Bool,
    Float,
    Str,
    Void,
    /// `list<T>` of a scalar.
    List(Scalar),
    /// `map<K, V>` with `int` or `string` keys and scalar values.
    Map(Scalar, Scalar),
    /// `?T` of a scalar.
    Opt(Scalar),
}

/// The scalar types: the elements of lists and maps, and nullable values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scalar {
    Int,
    Bool,
    Float,
    Str,
}

impl ValKind {
    /// Passed and returned as a `Value` in the register window, not in a
    /// machine register.
    pub fn boxed(self) -> bool {
        matches!(self, ValKind::Str | ValKind::List(_) | ValKind::Map(..) | ValKind::Opt(_))
    }
}

/// Present when every local of a function is an `int`, `bool`, `float` or
/// `string`, a nullable one, or a list or map of them, so the JIT may compile
/// it to native code (it still checks every instruction, see `phv_jit`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NativeSig {
    pub params: Vec<ValKind>,
    pub ret: ValKind,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Function {
    pub native: Option<NativeSig>,
    pub name: String,
    pub file: u32,
    pub num_params: u16,
    pub num_regs: u16,
    pub code: Vec<Instr>,
    /// Source byte offset of each instruction, for stack traces.
    pub offsets: Vec<u32>,
    /// The variables of the source, their registers, and whether their type
    /// allows null (for the debugger).
    #[serde(default)]
    pub locals: Vec<(String, u16, bool)>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Field {
    pub name: String,
    pub default: Const,
    pub public: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Class {
    pub name: String,
    pub parent: Option<u32>,
    pub fields: Vec<Field>,
    pub vtable: Vec<u32>,
    /// Sorted: the class, its ancestors and its interfaces.
    pub supers: Vec<u32>,
    /// Method name id to function, for interface calls, sorted by name id.
    pub by_name: Vec<(u32, u32)>,
    pub cases: Vec<(String, Const)>,
}

impl Class {
    /// The function of interned method name `name`, for interface calls.
    #[inline]
    pub fn method(&self, name: u32) -> Option<u32> {
        self.by_name.binary_search_by_key(&name, |&(n, _)| n).ok().map(|i| self.by_name[i].1)
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FileInfo {
    pub name: String,
    pub line_starts: Vec<u32>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Program {
    pub funcs: Vec<Function>,
    pub classes: Vec<Class>,
    pub consts: Vec<Const>,
    pub main: u32,
    pub builtin_names: Vec<String>,
    /// Interned method names used by `CallNamed`.
    pub method_names: Vec<String>,
    pub files: Vec<FileInfo>,
    /// Built-in exception classes the VM throws itself (`DivisionByZeroError`, ...).
    pub exc_classes: HashMap<String, u32>,
    /// Per builtin id: it takes constant time (see `CHEAP_BUILTINS`); filled
    /// on first use.
    #[serde(skip)]
    pub cheap_builtins: std::sync::OnceLock<Vec<bool>>,
}

/// Builtins of the standard library that take constant time and allocate
/// nothing (they read a length, a number or one element): the interpreter
/// does not read the clock after them. The clock is still read at the next
/// loop step or call, as after any instruction; every other builtin is
/// timed as soon as it returns, since one call can take long.
pub const CHEAP_BUILTINS: &[&str] = &[
    "strlen",
    "str_is_empty",
    "list_len",
    "list_is_empty",
    "list_first",
    "list_last",
    "map_len",
    "map_is_empty",
    "set_len",
    "set_is_empty",
    "abs_float",
    "min_int",
    "max_int",
    "min_float",
    "max_float",
    "floor",
    "ceil",
    "round",
    "sqrt",
    "fmod",
    "sin",
    "cos",
    "tan",
    "asin",
    "acos",
    "atan",
    "atan2",
    "exp",
    "log",
    "log_base",
    "log10",
    "log2",
    "hypot",
];

impl Program {
    /// Whether builtin `id` is one of `CHEAP_BUILTINS`.
    #[inline]
    pub fn builtin_is_cheap(&self, id: u32) -> bool {
        let cheap = self
            .cheap_builtins
            .get_or_init(|| self.builtin_names.iter().map(|n| CHEAP_BUILTINS.contains(&n.as_str())).collect());
        cheap.get(id as usize).copied().unwrap_or(false)
    }

    pub fn is_subclass(&self, class: u32, target: u32) -> bool {
        class == target || self.classes[class as usize].supers.binary_search(&target).is_ok()
    }
}
