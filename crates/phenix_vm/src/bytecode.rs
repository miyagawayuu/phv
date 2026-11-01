//! Compiled program format. A `Program` holds no `Rc` values, so it can be
//! shared between request threads behind an `Arc`.

use std::collections::HashMap;

pub type Reg = u16;
/// Placeholder register for an absent operand (e.g. no foreach key).
pub const NO_REG: Reg = u16::MAX;

#[derive(Clone, Debug, PartialEq)]
pub enum Const {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    EmptyList,
    EmptyMap,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TypeTag {
    Null,
    Bool,
    Int,
    Float,
    Str,
    Html,
    List,
    Map,
    Closure,
    Object,
}

#[derive(Clone, Debug)]
pub enum PathBase {
    Local(Reg),
    Prop(Reg, u16),
}

/// `$a[k1][k2] = v` or `$a[k1][] = v` on a local or a property.
#[derive(Clone, Debug)]
pub struct PathSet {
    pub base: PathBase,
    pub keys: Vec<Reg>,
    pub push: bool,
    pub val: Reg,
}

#[derive(Clone, Debug)]
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

    Call { dst: Reg, func: u32, args: Reg, argc: u16 },
    CallVirtual { dst: Reg, slot: u32, args: Reg, argc: u16 },
    CallClosure { dst: Reg, callee: Reg, args: Reg, argc: u16 },
    CallBuiltin { dst: Reg, id: u32, args: Reg, argc: u16 },
    Ret(Reg),
    RetNull,

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
    IterNext { it: Reg, key: Reg, val: Reg, end: u32 },

    Throw(Reg),
    TryBegin { catch: u32, exc: Reg },
    TryEnd,
    InstanceOf(Reg, Reg, u32),
    IsType(Reg, Reg, TypeTag),
    MatchError(Reg),

    MakeClosure { dst: Reg, func: u32, caps: Reg, n: u16 },

    Echo(Reg),
    EchoHtml(u32),
    PushOut,
    PopOutHtml(Reg),
}

#[derive(Debug, Default)]
pub struct Function {
    pub name: String,
    pub file: u32,
    pub num_params: u16,
    pub num_regs: u16,
    pub code: Vec<Instr>,
    /// Source byte offset of each instruction, for stack traces.
    pub offsets: Vec<u32>,
}

#[derive(Debug)]
pub struct Field {
    pub name: String,
    pub default: Const,
    pub public: bool,
}

#[derive(Debug)]
pub struct Class {
    pub name: String,
    pub parent: Option<u32>,
    pub fields: Vec<Field>,
    pub vtable: Vec<u32>,
}

#[derive(Debug)]
pub struct FileInfo {
    pub name: String,
    pub line_starts: Vec<u32>,
}

#[derive(Debug, Default)]
pub struct Program {
    pub funcs: Vec<Function>,
    pub classes: Vec<Class>,
    pub consts: Vec<Const>,
    pub main: u32,
    pub builtin_names: Vec<String>,
    pub files: Vec<FileInfo>,
    /// Built-in exception classes the VM throws itself (`DivisionByZeroError`, ...).
    pub exc_classes: HashMap<String, u32>,
}

impl Program {
    pub fn is_subclass(&self, mut class: u32, target: u32) -> bool {
        loop {
            if class == target {
                return true;
            }
            match self.classes[class as usize].parent {
                Some(p) => class = p,
                None => return false,
            }
        }
    }
}
