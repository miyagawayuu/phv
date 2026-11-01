use crate::span::Span;

#[derive(Debug, Clone)]
pub struct Module {
    pub file: u32,
    pub is_template: bool,
    pub stmts: Vec<Stmt>,
    /// Literal template paths passed to `render(...)`, so the driver can load them.
    pub template_refs: Vec<(String, Span)>,
}

#[derive(Debug, Clone)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignOp {
    Set,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    Concat,
    Coalesce,
}

#[derive(Debug, Clone)]
pub enum StmtKind {
    Expr(Expr),
    Echo(Vec<Expr>),
    InlineHtml(String),
    Assign { target: Expr, op: AssignOp, value: Expr },
    /// `?User $found = null;`
    TypedDecl { ty: TypeExpr, name: String, name_span: Span, value: Expr },
    IncDec { target: Expr, inc: bool },
    If { cond: Expr, then: Vec<Stmt>, elifs: Vec<(Expr, Vec<Stmt>)>, els: Option<Vec<Stmt>> },
    While { cond: Expr, body: Vec<Stmt> },
    DoWhile { body: Vec<Stmt>, cond: Expr },
    For { init: Vec<Stmt>, cond: Option<Expr>, step: Vec<Stmt>, body: Vec<Stmt> },
    Foreach { subject: Expr, key: Option<(String, Span)>, value: (String, Span), body: Vec<Stmt> },
    Break(u32),
    Continue(u32),
    Return(Option<Expr>),
    Throw(Expr),
    Try { body: Vec<Stmt>, catches: Vec<Catch>, finally: Option<Vec<Stmt>> },
    Block(Vec<Stmt>),
    Function(FunctionDecl),
    Class(ClassDecl),
    Import(ImportDecl),
    /// `params(string $title, list<Item> $items);` at the top of a template.
    Params(Vec<Param>),
}

#[derive(Debug, Clone)]
pub struct Catch {
    pub classes: Vec<(String, Span)>,
    pub var: Option<(String, Span)>,
    pub body: Vec<Stmt>,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Public,
    Protected,
    Private,
}

#[derive(Debug, Clone)]
pub struct Param {
    pub name: String,
    pub ty: Option<TypeExpr>,
    pub default: Option<Expr>,
    /// Constructor property promotion: `public readonly int $id`.
    pub promote: Option<(Visibility, bool)>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct FunctionDecl {
    pub name: String,
    pub name_span: Span,
    pub params: Vec<Param>,
    pub ret: Option<TypeExpr>,
    pub body: Vec<Stmt>,
    pub exported: bool,
    pub is_static: bool,
    pub visibility: Visibility,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ClassDecl {
    pub name: String,
    pub name_span: Span,
    pub parent: Option<(String, Span)>,
    pub props: Vec<PropDecl>,
    pub methods: Vec<FunctionDecl>,
    pub exported: bool,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct PropDecl {
    pub name: String,
    pub ty: TypeExpr,
    pub default: Option<Expr>,
    pub visibility: Visibility,
    pub readonly: bool,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ImportDecl {
    /// `std.http` in `import std.http.{request, Request};`
    pub module: Vec<String>,
    pub names: Vec<(String, Span)>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct TypeExpr {
    pub kind: TypeExprKind,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum TypeExprKind {
    Named(String, Vec<TypeExpr>),
    Nullable(Box<TypeExpr>),
    Union(Vec<TypeExpr>),
    Func(Vec<TypeExpr>, Box<TypeExpr>),
}

#[derive(Debug, Clone)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    Concat,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Cmp,
    And,
    Or,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    Coalesce,
}

impl BinOp {
    pub fn symbol(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Mod => "%",
            BinOp::Pow => "**",
            BinOp::Concat => ".",
            BinOp::Eq => "==",
            BinOp::Ne => "!=",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::Cmp => "<=>",
            BinOp::And => "&&",
            BinOp::Or => "||",
            BinOp::BitAnd => "&",
            BinOp::BitOr => "|",
            BinOp::BitXor => "^",
            BinOp::Shl => "<<",
            BinOp::Shr => ">>",
            BinOp::Coalesce => "??",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
    BitNot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastKind {
    Int,
    Float,
    String,
    Bool,
}

#[derive(Debug, Clone)]
pub enum InterpPart {
    Lit(String),
    Expr(Expr),
}

#[derive(Debug, Clone)]
pub struct Arg {
    pub name: Option<(String, Span)>,
    pub value: Expr,
}

#[derive(Debug, Clone)]
pub struct MatchArm {
    /// Empty for the `default` arm.
    pub conds: Vec<Expr>,
    pub body: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum ClosureBody {
    Expr(Box<Expr>),
    Block(Vec<Stmt>),
}

#[derive(Debug, Clone)]
pub struct Closure {
    pub params: Vec<Param>,
    pub ret: Option<TypeExpr>,
    pub uses: Vec<(String, Span)>,
    pub body: ClosureBody,
    /// `fn() =>` captures automatically; `function() use (...)` only what it lists.
    pub arrow: bool,
}

#[derive(Debug, Clone)]
pub enum ExprKind {
    Int(i64),
    Float(f64),
    Str(String),
    Interp(Vec<InterpPart>),
    Bool(bool),
    Null,
    Var(String),
    Name(String),
    List(Vec<Expr>),
    Map(Vec<(Expr, Expr)>),
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Ternary(Box<Expr>, Box<Expr>, Box<Expr>),
    Call { callee: Box<Expr>, args: Vec<Arg> },
    MethodCall { obj: Box<Expr>, name: String, name_span: Span, args: Vec<Arg>, nullsafe: bool },
    Prop { obj: Box<Expr>, name: String, name_span: Span, nullsafe: bool },
    StaticCall { class: String, class_span: Span, name: String, args: Vec<Arg> },
    New { class: String, class_span: Span, args: Vec<Arg> },
    /// `$a[$k]`, or `$a[]` (push, only valid as an assignment target).
    Index { base: Box<Expr>, index: Option<Box<Expr>> },
    Closure(Box<Closure>),
    Match { subject: Option<Box<Expr>>, arms: Vec<MatchArm> },
    Is(Box<Expr>, TypeExpr),
    Cast(CastKind, Box<Expr>),
    Range { from: Box<Expr>, to: Box<Expr>, inclusive: bool },
    Throw(Box<Expr>),
}
