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
    /// Set when the statement is in HTML other than element text (a
    /// `<script>`, an attribute, a URL, CSS...): output written by the
    /// functions it calls would land there, escaped only for text.
    pub out: Option<EchoContext>,
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
    /// `<?= ... ?>` with the HTML context it appears in.
    HtmlEcho(Vec<Expr>, EchoContext),
    /// Literal HTML, with the offsets of opening `<script` tags (for CSP nonces).
    InlineHtml(String, Vec<usize>),
    Assign {
        target: Expr,
        op: AssignOp,
        value: Expr,
    },
    /// `?User $found = null;`
    TypedDecl {
        ty: Box<TypeExpr>,
        name: String,
        name_span: Span,
        value: Expr,
    },
    IncDec {
        target: Expr,
        inc: bool,
    },
    If {
        cond: Expr,
        then: Vec<Stmt>,
        elifs: Vec<(Expr, Vec<Stmt>)>,
        els: Option<Vec<Stmt>>,
    },
    While {
        cond: Expr,
        body: Vec<Stmt>,
    },
    DoWhile {
        body: Vec<Stmt>,
        cond: Expr,
    },
    For {
        init: Vec<Stmt>,
        cond: Option<Expr>,
        step: Vec<Stmt>,
        body: Vec<Stmt>,
    },
    Foreach {
        subject: Box<Expr>,
        key: Option<(String, Span)>,
        value: (String, Span),
        body: Vec<Stmt>,
    },
    Break(u32),
    Continue(u32),
    Return(Option<Expr>),
    Throw(Expr),
    /// `yield $value;` or `yield $key => $value;` (the function is a generator).
    Yield {
        key: Option<Expr>,
        value: Expr,
    },
    /// `yield from $values;`: each key and value of a list, map, set or generator.
    YieldFrom(Expr),
    Try {
        body: Vec<Stmt>,
        catches: Vec<Catch>,
        finally: Option<Vec<Stmt>>,
    },
    Block(Vec<Stmt>),
    Function(Box<FunctionDecl>),
    Class(Box<ClassDecl>),
    Import(ImportDecl),
    Const(Box<ConstDecl>),
    /// `type UserId = int;`
    TypeAlias(Box<TypeAliasDecl>),
    /// `params(string $title, list<Item> $items);` at the top of a template.
    Params(Vec<Param>),
}

/// Where in an HTML document a `<?= ?>` value is inserted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EchoContext {
    /// Element content.
    Text,
    /// Element content inside an open `<select>`. Older browsers ignore the
    /// start tag of a raw-text element such as `<title>` or `<style>` there
    /// and read its text as markup (R19-26), so `Html` inserted here may hold
    /// none (the template that made it was checked on its own). Otherwise as
    /// `Text`: functions called here may write output.
    SelectText,
    /// A quoted attribute value.
    Attr,
    /// The start of a quoted URL attribute (`href="<?= ?>"`).
    Url,
    /// The start of a quoted URL attribute after a literal `&`: a value would
    /// continue a character reference.
    UrlReference,
    /// An unquoted attribute value.
    Unquoted,
    /// Inside a tag, between attributes.
    Tag,
    /// Right after `<`, `</`, `<!` (or in a script `-`, `<!-`, `<scr`...): the
    /// value would be read with them, and could start a tag or comment.
    AfterMarkup,
    /// Inside `<script>`, where an expression goes.
    Script,
    /// Inside `<script>`, in a string, template literal, comment or regular
    /// expression, where a JSON value's quotes or text would change the code.
    ScriptLiteral,
    /// Inside `<style>` or a `style="..."` attribute.
    Style,
    /// An event handler attribute (`onclick="..."`).
    Handler,
    /// A URL attribute that starts with `javascript:`, `vbscript:` or `data:`.
    ScriptUrl,
    /// `<iframe srcdoc="...">`: its value is parsed as an HTML document.
    Srcdoc,
    /// The URL of a `<script>` (`src`, or `href` in SVG): the tag gets the CSP
    /// nonce, so any URL inserted there would run as a trusted script.
    ScriptSrc,
    /// `content` of a `<meta>` that is not a `name`/`property`/`itemprop`
    /// meta, such as `http-equiv="refresh"` (`content="0;url=..."` redirects).
    MetaContent,
    /// Element content or a comment inside `<svg>` or `<math>`. Browsers read
    /// markup there as foreign content, while a template's or function's
    /// output was checked as HTML: `Html` is checked when it is inserted, and
    /// functions called there may not write output.
    ForeignText,
    /// The text of a raw-text element such as `<textarea>` or `<title>` (its
    /// name): `Html` inserted there may not hold its end tag, and functions
    /// called there may not write output.
    RawText(&'static str),
    /// A comment: `Html` inserted there may not end it, and functions called
    /// there may not write output.
    Comment {
        /// A bogus comment such as `<!x ...>` (or a CDATA section in SVG or
        /// MathML), which ends at the first `>`.
        bogus: bool,
        /// Inside `<noscript>`, `<svg>` or `<math>`, where a browser may read
        /// raw text here: `Html` may not hold the end tag of a raw-text
        /// element either.
        watched: bool,
    },
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
    /// `int ...$xs`: takes the remaining arguments as a list.
    pub variadic: bool,
    pub span: Span,
}

/// `#[Deprecated("use load()")]` before a declaration.
#[derive(Debug, Clone)]
pub struct Attribute {
    pub name: String,
    pub name_span: Span,
    pub args: Vec<Arg>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct FunctionDecl {
    pub attrs: Vec<Attribute>,
    pub name: String,
    pub name_span: Span,
    /// `function first<T>(list<T> $xs): ?T`
    pub type_params: Vec<(String, Span)>,
    /// `<T: Shape>`: the bound of each type parameter.
    pub type_bounds: Vec<Option<TypeExpr>>,
    /// Declared without a body (`abstract` methods and interface methods).
    pub is_abstract: bool,
    /// `final public function ...`: subclasses cannot override it.
    pub is_final: bool,
    pub params: Vec<Param>,
    pub ret: Option<TypeExpr>,
    pub body: Vec<Stmt>,
    pub exported: bool,
    pub is_static: bool,
    pub visibility: Visibility,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassKind {
    Class,
    Abstract,
    Interface,
    Enum,
    /// `trait T { ... }`: its members are copied into the classes that `use` it.
    Trait,
}

/// `use A, B<int> { A::hello insteadof B; B::hello as protected helloB; }`
/// in the body of a class, an enum or a trait.
#[derive(Debug, Clone)]
pub struct TraitUse {
    pub traits: Vec<TraitRef>,
    pub rules: Vec<TraitRule>,
    pub span: Span,
}

/// A trait named in a `use`, with its type arguments.
#[derive(Debug, Clone)]
pub struct TraitRef {
    pub name: String,
    pub span: Span,
    pub args: Vec<TypeExpr>,
}

/// A rule in the `{ ... }` of a `use`.
#[derive(Debug, Clone)]
pub enum TraitRule {
    /// `A::m insteadof B, C;`: the method `m` of `A`, not those of `B` and `C`.
    Insteadof { from: (String, Span), method: (String, Span), excluded: Vec<(String, Span)>, span: Span },
    /// `[A::]m as [visibility] [alias];`: with an alias, a copy of `m` under that
    /// name; without one, `m` with another visibility.
    As {
        from: Option<(String, Span)>,
        method: (String, Span),
        vis: Option<Visibility>,
        alias: Option<(String, Span)>,
        span: Span,
    },
}

#[derive(Debug, Clone)]
pub struct ClassDecl {
    pub attrs: Vec<Attribute>,
    pub name: String,
    pub name_span: Span,
    pub kind: ClassKind,
    /// `final class`: no class can extend it.
    pub is_final: bool,
    /// `class Box<T>`
    pub type_params: Vec<(String, Span)>,
    /// `class Repo<T: Model>`: the bound of each type parameter.
    pub type_bounds: Vec<Option<TypeExpr>>,
    pub parent: Option<(String, Span)>,
    /// Type arguments of the parent: `extends Base<int>`.
    pub parent_args: Vec<TypeExpr>,
    /// `implements` list (or the `extends` list of an interface).
    pub interfaces: Vec<(String, Span)>,
    /// Type arguments of each entry of `interfaces`.
    pub interface_args: Vec<Vec<TypeExpr>>,
    /// `use T;` in the body: the traits whose members it takes.
    pub uses: Vec<TraitUse>,
    pub props: Vec<PropDecl>,
    pub methods: Vec<FunctionDecl>,
    pub consts: Vec<ConstDecl>,
    pub cases: Vec<EnumCase>,
    /// `enum Status: string`
    pub backing: Option<TypeExpr>,
    pub exported: bool,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ConstDecl {
    pub attrs: Vec<Attribute>,
    pub name: String,
    pub name_span: Span,
    pub ty: Option<TypeExpr>,
    pub value: Expr,
    pub exported: bool,
    pub span: Span,
}

/// `type Pair<T> = map<string, T>;`
#[derive(Debug, Clone)]
pub struct TypeAliasDecl {
    pub name: String,
    pub name_span: Span,
    pub type_params: Vec<(String, Span)>,
    pub ty: TypeExpr,
    pub exported: bool,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct EnumCase {
    pub name: String,
    pub value: Option<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct PropDecl {
    pub attrs: Vec<Attribute>,
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
    pub names: Vec<ImportName>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ImportName {
    pub name: String,
    pub span: Span,
    /// `import lib.a.{render as renderA};`
    pub alias: Option<(String, Span)>,
}

impl ImportName {
    /// The name the importing file uses, and where it is written.
    pub fn local(&self) -> (&str, Span) {
        match &self.alias {
            Some((a, s)) => (a, *s),
            None => (&self.name, self.span),
        }
    }
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

/// `$obj->name(...)`, or `$obj?->name(...)`.
#[derive(Debug, Clone)]
pub struct MethodCall {
    pub obj: Box<Expr>,
    pub name: String,
    pub name_span: Span,
    pub args: Vec<Arg>,
    pub nullsafe: bool,
}

/// `$obj->name`, or `$obj?->name`.
#[derive(Debug, Clone)]
pub struct Prop {
    pub obj: Box<Expr>,
    pub name: String,
    pub name_span: Span,
    pub nullsafe: bool,
}

/// `Cls::name(...)`.
#[derive(Debug, Clone)]
pub struct StaticCall {
    pub class: String,
    pub class_span: Span,
    pub name: String,
    pub name_span: Span,
    pub args: Vec<Arg>,
}

/// `Cls::NAME`.
#[derive(Debug, Clone)]
pub struct ClassConst {
    pub class: String,
    pub class_span: Span,
    pub name: String,
    pub name_span: Span,
}

/// `new Cls(...)`.
#[derive(Debug, Clone)]
pub struct New {
    pub class: String,
    pub class_span: Span,
    /// `new Box<int>(...)`; empty when they are inferred.
    pub type_args: Vec<TypeExpr>,
    pub args: Vec<Arg>,
}

/// The rare or large variants hold a box, so that the common nodes stay
/// small (the sizes are checked in the tests).
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
    Call {
        callee: Box<Expr>,
        args: Box<[Arg]>,
    },
    MethodCall(Box<MethodCall>),
    Prop(Box<Prop>),
    StaticCall(Box<StaticCall>),
    /// `Suit::Hearts` (enum case) or `Config::MAX` (class constant).
    ClassConst(Box<ClassConst>),
    /// A parenthesized member access; stops a `?->` chain like in PHP.
    Paren(Box<Expr>),
    /// `sql"SELECT * FROM t WHERE id = {$id}"`
    Sql(Vec<InterpPart>),
    New(Box<New>),
    /// `$a[$k]`, or `$a[]` (push, only valid as an assignment target).
    Index {
        base: Box<Expr>,
        index: Option<Box<Expr>>,
    },
    Closure(Box<Closure>),
    Match {
        subject: Option<Box<Expr>>,
        arms: Box<[MatchArm]>,
    },
    Is(Box<Expr>, Box<TypeExpr>),
    Cast(CastKind, Box<Expr>),
    Range {
        from: Box<Expr>,
        to: Box<Expr>,
        inclusive: bool,
    },
    Throw(Box<Expr>),
    /// `strlen(...)`, `$obj->method(...)`, `Cls::method(...)`: the function as
    /// a value. Holds the call, written without arguments.
    FuncRef(Box<Expr>),
    /// `...$xs` in an argument list or a list literal.
    Spread(Box<Expr>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::size_of;

    /// Every node of a kind is as large as its largest variant, so a large
    /// variant that is rare would make the common ones (`$x`, `"..."`,
    /// `f($x)`) pay for it, in memory and in parse time.
    #[test]
    #[cfg(target_pointer_width = "64")]
    fn common_nodes_stay_small() {
        const BOX: &str =
            "a variant grew past the others: box its payload (as `ExprKind::MethodCall(Box<MethodCall>)` \
                           or `StmtKind::Class(Box<ClassDecl>)`), box a large field of it (as `Foreach`'s `subject`), \
                           or hold a list as `Box<[T]>` (as `Call`'s `args`)";
        let sizes = [
            ("ExprKind", size_of::<ExprKind>(), 32),
            ("Expr", size_of::<Expr>(), 48),
            ("StmtKind", size_of::<StmtKind>(), 128),
            ("Stmt", size_of::<Stmt>(), 168),
            ("Arg", size_of::<Arg>(), 88),
            ("InterpPart", size_of::<InterpPart>(), 48),
            ("MatchArm", size_of::<MatchArm>(), 88),
            ("Param", size_of::<Param>(), 152),
            ("TypeExpr", size_of::<TypeExpr>(), 64),
        ];
        for (name, size, max) in sizes {
            assert!(size <= max, "`{name}` is {size} bytes, more than {max}: {BOX}");
        }
    }

    /// The lists of statements and of arguments the parser keeps have no
    /// room left over from growing: they live as long as the tree.
    #[test]
    fn lists_keep_no_spare_room() {
        let src = "<?phv\nfunction f(int $x): int {\n    if ($x > 0) {\n        return $x;\n    }\n    return 0;\n}\n$o->m(1);\nf(2);\n";
        let (m, diags) = crate::parse_module(src, 0, false);
        assert!(diags.is_empty(), "{diags:?}");
        assert_eq!(m.stmts.capacity(), m.stmts.len());
        let StmtKind::Function(f) = &m.stmts[0].kind else { panic!("{:?}", m.stmts[0]) };
        assert_eq!((f.body.len(), f.body.capacity()), (2, 2));
        let StmtKind::If { then, .. } = &f.body[0].kind else { panic!("{:?}", f.body[0]) };
        assert_eq!((then.len(), then.capacity()), (1, 1));
        let StmtKind::Expr(e) = &m.stmts[1].kind else { panic!("{:?}", m.stmts[1]) };
        let ExprKind::MethodCall(c) = &e.kind else { panic!("{e:?}") };
        assert_eq!((c.args.len(), c.args.capacity()), (1, 1));
    }

    /// The boxed variants hold what they held before, with the same spans.
    #[test]
    fn boxed_variants_keep_their_parts() {
        let src = "<?phv\n$a?->b->c(1);\nX::Y;\nX::f(2);\nnew Box<int>(3);\n$x is int;\nf(4, 5);\n";
        let (m, diags) = crate::parse_module(src, 0, false);
        assert!(diags.is_empty(), "{diags:?}");
        let exprs: Vec<&Expr> = m
            .stmts
            .iter()
            .map(|s| match &s.kind {
                StmtKind::Expr(e) => e,
                k => panic!("{k:?}"),
            })
            .collect();
        let text = |s: Span| &src[s.start as usize..s.end as usize];
        let ExprKind::MethodCall(c) = &exprs[0].kind else { panic!("{:?}", exprs[0]) };
        assert_eq!((c.name.as_str(), text(c.name_span), c.args.len(), c.nullsafe), ("c", "c", 1, false));
        let ExprKind::Prop(p) = &c.obj.kind else { panic!("{:?}", c.obj) };
        assert_eq!((p.name.as_str(), text(p.name_span), p.nullsafe, text(c.obj.span)), ("b", "b", true, "$a?->b"));
        assert_eq!(text(exprs[0].span), "$a?->b->c(1)");
        let ExprKind::ClassConst(k) = &exprs[1].kind else { panic!("{:?}", exprs[1]) };
        assert_eq!((k.class.as_str(), text(k.class_span), k.name.as_str(), text(k.name_span)), ("X", "X", "Y", "Y"));
        let ExprKind::StaticCall(s) = &exprs[2].kind else { panic!("{:?}", exprs[2]) };
        assert_eq!((s.class.as_str(), s.name.as_str(), s.args.len(), text(exprs[2].span)), ("X", "f", 1, "X::f(2)"));
        let ExprKind::New(n) = &exprs[3].kind else { panic!("{:?}", exprs[3]) };
        assert_eq!((n.class.as_str(), text(n.class_span), n.type_args.len(), n.args.len()), ("Box", "Box", 1, 1));
        let ExprKind::Is(x, ty) = &exprs[4].kind else { panic!("{:?}", exprs[4]) };
        assert_eq!((text(x.span), text(ty.span), text(exprs[4].span)), ("$x", "int", "$x is int"));
        let ExprKind::Call { args, .. } = &exprs[5].kind else { panic!("{:?}", exprs[5]) };
        assert_eq!(args.len(), 2);
    }
}
