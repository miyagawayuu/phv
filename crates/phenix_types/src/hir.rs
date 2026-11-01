//! Typed, name-resolved intermediate representation produced by the checker
//! and consumed by the bytecode compiler.

use crate::types::{ClassId, FuncId, LocalId, Type};
use phenix_syntax::Span;
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq)]
pub enum HConst {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    EmptyList,
    EmptyMap,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinKind {
    AddI,
    SubI,
    MulI,
    ModI,
    PowI,
    AddF,
    SubF,
    MulF,
    DivF,
    ModF,
    PowF,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    EqI,
    Eq,
    Ne,
    LtI,
    LeI,
    Lt,
    Le,
    Cmp,
    /// Only used by `HStmt::Compound` for `.=` on a property or element.
    Concat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnKind {
    NegI,
    NegF,
    Not,
    BitNot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestKind {
    Null,
    Bool,
    Int,
    Float,
    Str,
    Html,
    List,
    Map,
    Func,
    Class(ClassId),
}

#[derive(Clone, Debug)]
pub struct HExpr {
    pub kind: HExprKind,
    pub ty: Type,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum HExprKind {
    Const(HConst),
    Local(LocalId),
    Unary(UnKind, Box<HExpr>),
    /// The final flag swaps the operands of the operation (so `a > b` is `b < a`)
    /// while still evaluating `a` first.
    Binary(BinKind, Box<HExpr>, Box<HExpr>, bool),
    And(Box<HExpr>, Box<HExpr>),
    Or(Box<HExpr>, Box<HExpr>),
    Coalesce(Box<HExpr>, Box<HExpr>),
    Ternary(Box<HExpr>, Box<HExpr>, Box<HExpr>),
    Concat(Vec<HExpr>, bool),
    IntToFloat(Box<HExpr>),
    FloatToInt(Box<HExpr>),
    ToStr(Box<HExpr>),
    List(Vec<HExpr>),
    Map(Vec<(HExpr, HExpr)>),
    Range(Box<HExpr>, Box<HExpr>, bool),
    Index(Box<HExpr>, Box<HExpr>),
    IndexOrNull(Box<HExpr>, Box<HExpr>),
    Prop(Box<HExpr>, u32),
    /// `obj?->...`: evaluates `obj` into `tmp`; null short-circuits, otherwise `body`.
    NullSafe { tmp: LocalId, obj: Box<HExpr>, body: Box<HExpr> },
    Call(FuncId, Vec<HExpr>),
    /// Receiver first.
    CallVirtual(u32, Vec<HExpr>),
    CallBuiltin(String, Vec<HExpr>),
    CallClosure(Box<HExpr>, Vec<HExpr>),
    /// Method call through an interface type, dispatched by name. Receiver first.
    CallNamed(String, Vec<HExpr>),
    EnumCase(ClassId, u32),
    /// `E::from($v)` / `E::tryFrom($v)` (the flag is true for `tryFrom`).
    EnumFrom(ClassId, Box<HExpr>, bool),
    EnumCases(ClassId),
    New(ClassId, Option<FuncId>, Vec<HExpr>),
    Closure(FuncId, Vec<HExpr>),
    Match { subject: Option<Box<HExpr>>, arms: Vec<(Vec<HExpr>, HExpr)>, default: Option<Box<HExpr>> },
    Is(Box<HExpr>, TestKind),
    Throw(Box<HExpr>),
}

#[derive(Clone, Debug)]
pub enum HPlaceBase {
    Local(LocalId),
    Prop(Box<HExpr>, u32),
}

#[derive(Clone, Debug)]
pub enum HPathElem {
    /// The constant creates a missing intermediate entry (`$m["a"]["b"] = 1`).
    Index(HExpr, Option<HConst>),
    Push,
}

#[derive(Clone, Debug)]
pub struct HPlace {
    pub base: HPlaceBase,
    pub path: Vec<HPathElem>,
}

#[derive(Clone, Debug)]
pub struct HCatch {
    pub classes: Vec<ClassId>,
    pub var: Option<LocalId>,
    pub body: Vec<HStmt>,
}

#[derive(Clone, Debug)]
pub enum HStmt {
    Expr(HExpr),
    Echo(HExpr),
    Html(String),
    Assign(HPlace, HExpr),
    Compound(HPlace, BinKind, HExpr),
    CoalesceAssign(HPlace, HExpr),
    /// `$s .= x` on a local string.
    Append(LocalId, HExpr),
    If(HExpr, Vec<HStmt>, Vec<HStmt>),
    Loop { cond: Option<HExpr>, body: Vec<HStmt>, step: Vec<HStmt>, do_while: bool },
    ForRange { var: LocalId, from: HExpr, to: HExpr, inclusive: bool, body: Vec<HStmt> },
    Foreach { subject: HExpr, key: Option<LocalId>, val: LocalId, body: Vec<HStmt> },
    Break(u32),
    Continue(u32),
    Return(Option<HExpr>, Span),
    Throw(HExpr),
    Try { body: Vec<HStmt>, catches: Vec<HCatch>, finally: Option<Vec<HStmt>> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FuncKind {
    Normal,
    Method,
    Ctor,
    Closure,
    Template,
    Main,
}

#[derive(Clone, Debug)]
pub struct HLocal {
    pub name: String,
    pub ty: Type,
}

#[derive(Clone, Debug)]
pub struct HFunc {
    pub name: String,
    pub file: u32,
    pub span: Span,
    pub kind: FuncKind,
    /// Locals filled by the caller, in order (`$this` first for methods).
    pub params: Vec<LocalId>,
    /// Locals filled from the closure's captured values (before `params`).
    pub captures: Vec<LocalId>,
    pub locals: Vec<HLocal>,
    pub body: Vec<HStmt>,
    pub ret: Type,
}

#[derive(Clone, Debug)]
pub struct HField {
    pub name: String,
    pub default: HConst,
    pub public: bool,
}

#[derive(Clone, Debug)]
pub struct HClass {
    pub name: String,
    pub parent: Option<ClassId>,
    pub fields: Vec<HField>,
    pub vtable: Vec<FuncId>,
    /// The class itself, its ancestors and every interface it implements.
    pub supers: Vec<ClassId>,
    /// Non-static methods by name, for calls through interfaces.
    pub by_name: Vec<(String, FuncId)>,
    /// Enum cases: name and backing value (`Null` for pure enums).
    pub cases: Vec<(String, HConst)>,
}

#[derive(Clone, Debug, Default)]
pub struct HProgram {
    pub funcs: Vec<HFunc>,
    pub classes: Vec<HClass>,
    pub main: FuncId,
    pub exc_classes: HashMap<String, ClassId>,
}
