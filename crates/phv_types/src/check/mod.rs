//! Name resolution and type checking. Produces the typed HIR.
//!
//! Phases:
//! 1. collect declarations of every module
//! 2. resolve imports
//! 3. order classes by inheritance
//! 4. resolve class members (fields, methods, vtables)
//! 5. resolve function and template signatures
//! 6. check bodies

mod expr;
mod flow;
mod stmt;

use crate::builtins::{BuiltinSig, Recv, BUILTINS};
use crate::hir::*;
use crate::index::{EditorIndex, Item, ItemKind, Member, MemberSite, VarScope};
use crate::types::*;
use flow::FnCtx;
use phv_syntax::ast::{
    self, ClassDecl, ClassKind, ExprKind, FunctionDecl, StmtKind, TypeExpr, TypeExprKind, Visibility,
};
use phv_syntax::{parse_type_str, Diagnostic, Span};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ModuleKind {
    /// Built-in declarations visible everywhere (exception classes).
    Prelude,
    /// `std.*` modules.
    Std,
    /// A module imported by path.
    User,
    /// A `.phv.html` template used through `render(...)`.
    Template,
    /// The script being run.
    Entry,
}

pub struct ModuleInput<'a> {
    pub ast: &'a ast::Module,
    pub kind: ModuleKind,
    /// Dotted module name or template path, for messages.
    pub name: String,
}

pub struct CheckInput<'a> {
    pub modules: Vec<ModuleInput<'a>>,
    /// Dotted module name (`std.http`, `lib.posts`) to module index.
    pub imports: HashMap<String, usize>,
    /// Normalized template path to module index.
    pub templates: HashMap<String, usize>,
}

pub fn check(input: &CheckInput) -> (HProgram, Vec<Diagnostic>) {
    let mut c = Checker::new(input);
    c.run();
    c.finish()
}

/// Like `check`, also recording what editors need (see `crate::index`).
pub fn check_for_editor(input: &CheckInput) -> (HProgram, Vec<Diagnostic>, EditorIndex) {
    let mut c = Checker::new(input);
    c.index = Some(EditorIndex::default());
    c.run();
    let index = c.build_index();
    let (hir, diags) = c.finish();
    (hir, diags, index)
}

#[derive(Default)]
struct Scope {
    funcs: HashMap<String, FuncId>,
    classes: HashMap<String, ClassId>,
    consts: HashMap<String, (HConst, Type)>,
    /// Where each constant in `consts` is declared.
    const_spans: HashMap<String, Span>,
    export_funcs: HashMap<String, FuncId>,
    export_classes: HashMap<String, ClassId>,
    export_consts: HashMap<String, (HConst, Type)>,
}

#[derive(Clone)]
struct FieldInfo {
    name: String,
    ty: Type,
    vis: Visibility,
    readonly: bool,
    owner: ClassId,
    default: Option<HConst>,
    /// Non-nullable without a default: the constructor must assign it.
    required: bool,
    span: Span,
}

#[derive(Clone)]
struct MethodInfo {
    func: FuncId,
    slot: Option<u32>,
    vis: Visibility,
    is_static: bool,
    owner: ClassId,
}

struct ClassInfo<'a> {
    name: String,
    decl: Option<&'a ClassDecl>,
    kind: ClassKind,
    module: usize,
    /// `class Box<T>`: the names of its type parameters.
    type_params: Vec<String>,
    /// The type arguments each ancestor and interface is used with, in terms of
    /// this class's own type parameters (`class A<T> extends B<list<T>>` maps
    /// `B` to `[list<T>]`). Only generic supers are listed.
    super_args: HashMap<ClassId, Vec<Type>>,
    parent: Option<ClassId>,
    /// Directly implemented (or, for interfaces, extended) interfaces.
    ifaces: Vec<ClassId>,
    /// Itself, ancestors and all interfaces; filled when the class is resolved.
    supers: HashSet<ClassId>,
    fields: Vec<FieldInfo>,
    /// The slot of each field in `fields`, by name.
    field_slots: HashMap<String, u32>,
    methods: HashMap<String, MethodInfo>,
    vtable: Vec<FuncId>,
    ctor: Option<FuncId>,
    /// Visibility of the constructor and the class that declares it: a
    /// private constructor means only the class itself can use `new`.
    ctor_access: (Visibility, ClassId),
    consts: HashMap<String, (HConst, Type)>,
    cases: Vec<(String, HConst)>,
    backing: Option<Type>,
    span: Span,
}

#[derive(Clone, Copy)]
enum Body<'a> {
    Function(&'a FunctionDecl),
    Template(&'a ast::Module),
    Main(&'a ast::Module),
    Done,
}

#[derive(Clone)]
struct ParamInfo {
    name: String,
    ty: Type,
    default: Option<HConst>,
    promote: Option<(Visibility, bool)>,
    span: Span,
}

struct FuncInfo<'a> {
    name: String,
    /// Names of the function's type parameters (`function f<T>(...)`).
    type_params: Vec<String>,
    is_abstract: bool,
    params: Vec<ParamInfo>,
    ret: Type,
    body: Body<'a>,
    module: usize,
    class: Option<ClassId>,
    is_static: bool,
    kind: FuncKind,
    span: Span,
}

pub(crate) struct ParsedBuiltin {
    sig: &'static BuiltinSig,
    params: Vec<Type>,
    ret: Type,
}

pub(crate) struct Checker<'a> {
    input: &'a CheckInput<'a>,
    diags: Vec<Diagnostic>,
    classes: Vec<ClassInfo<'a>>,
    class_names: Vec<String>,
    funcs: Vec<FuncInfo<'a>>,
    hfuncs: Vec<Option<HFunc>>,
    scopes: Vec<Scope>,
    builtins: HashMap<&'static str, Vec<ParsedBuiltin>>,
    ctx: Vec<FnCtx>,
    module: usize,
    template_funcs: HashMap<usize, FuncId>,
    main: Option<FuncId>,
    /// Type parameters in scope while resolving or checking a generic function.
    type_params: Vec<String>,
    /// Only built for editors (`check_for_editor`).
    index: Option<EditorIndex>,
    /// Members copied into subclasses so far (see `MAX_INHERITED_MEMBERS`).
    inherited_members: usize,
    reported_hierarchy: bool,
    /// What `#[Deprecated]` says about functions, classes and properties
    /// (`(owner, name)`); using them gives a warning.
    deprecated_funcs: HashMap<FuncId, String>,
    deprecated_classes: HashMap<ClassId, String>,
    deprecated_fields: HashMap<(ClassId, String), String>,
    /// `#[Test]` and `#[Skip]` functions.
    tests: Vec<(FuncId, Option<String>)>,
    /// Names each module has looked up in its own scope (`(module, name)`),
    /// to tell which imports are used. Lookups take `&self`, hence the cell.
    used_names: RefCell<HashSet<(usize, String)>>,
}

/// What an annotation can be written before.
#[derive(Clone, Copy, PartialEq)]
enum AnnTarget {
    Function,
    Method,
    Class,
    Property,
    Const,
}

/// The annotations of one declaration, checked.
#[derive(Default)]
struct Annotations {
    deprecated: Option<String>,
    is_override: bool,
    test: bool,
    skip: Option<String>,
}

/// Annotations PHV knows; any other name is an error (a typo would otherwise be ignored).
const ANNOTATIONS: &str = "Deprecated, Override, Test, Skip";

/// Members (fields, methods, constants) all classes may inherit together.
const MAX_INHERITED_MEMBERS: usize = 200_000;

/// Properties of one class, its inherited ones included: a property's slot is a
/// 16-bit operand of the bytecode.
const MAX_FIELDS: usize = u16::MAX as usize;

fn sig_type(t: &TypeExpr) -> Type {
    match &t.kind {
        TypeExprKind::Named(n, args) => match (n.as_str(), args.as_slice()) {
            ("int", []) => Type::Int,
            ("float", []) => Type::Float,
            ("string", []) => Type::Str,
            ("bool", []) => Type::Bool,
            ("void", []) => Type::Void,
            ("any", []) => Type::Any,
            ("Html", []) => Type::Html,
            ("SafeUrl", []) => Type::SafeUrl,
            ("SafeJs", []) => Type::SafeJs,
            ("Sql", []) => Type::Sql,
            ("T", []) => Type::Generic(0),
            ("U", []) => Type::Generic(1),
            ("K", []) => Type::Generic(2),
            ("V", []) => Type::Generic(3),
            ("list", [a]) => Type::list(sig_type(a)),
            ("map", [k, v]) => Type::map(sig_type(k), sig_type(v)),
            _ => panic!("bad builtin signature type `{n}`"),
        },
        TypeExprKind::Nullable(inner) => Type::nullable(sig_type(inner)),
        TypeExprKind::Union(ms) => Type::union(ms.iter().map(sig_type).collect()),
        TypeExprKind::Func(ps, r) => Type::Func(ps.iter().map(sig_type).collect(), Box::new(sig_type(r))),
    }
}

impl<'a> Checker<'a> {
    fn new(input: &'a CheckInput<'a>) -> Checker<'a> {
        let mut builtins: HashMap<_, Vec<ParsedBuiltin>> = HashMap::new();
        for sig in BUILTINS {
            let t = parse_type_str(sig.sig).unwrap_or_else(|| panic!("bad builtin signature: {}", sig.sig));
            let Type::Func(params, ret) = sig_type(&t) else { panic!("builtin signature must be a function type") };
            builtins.entry(sig.name).or_default().push(ParsedBuiltin { sig, params, ret: *ret });
        }
        Checker {
            input,
            diags: Vec::new(),
            classes: Vec::new(),
            class_names: Vec::new(),
            funcs: Vec::new(),
            hfuncs: Vec::new(),
            scopes: (0..input.modules.len()).map(|_| Scope::default()).collect(),
            builtins,
            ctx: Vec::new(),
            module: 0,
            template_funcs: HashMap::new(),
            main: None,
            type_params: Vec::new(),
            index: None,
            inherited_members: 0,
            reported_hierarchy: false,
            deprecated_funcs: HashMap::new(),
            deprecated_classes: HashMap::new(),
            deprecated_fields: HashMap::new(),
            tests: Vec::new(),
            used_names: RefCell::new(HashSet::new()),
        }
    }

    /// Checks the annotations written before a declaration of kind `target`.
    fn annotations(&mut self, attrs: &[ast::Attribute], target: AnnTarget) -> Annotations {
        let mut out = Annotations::default();
        let mut seen: Vec<&str> = Vec::new();
        for a in attrs {
            if seen.contains(&a.name.as_str()) {
                self.error(format!("`#[{}]` is written more than once", a.name), a.name_span);
                continue;
            }
            seen.push(&a.name);
            let allowed: &[AnnTarget] = match a.name.as_str() {
                "Deprecated" => &[AnnTarget::Function, AnnTarget::Method, AnnTarget::Class, AnnTarget::Property],
                "Override" => &[AnnTarget::Method],
                "Test" | "Skip" => &[AnnTarget::Function],
                _ => {
                    self.error(format!("unknown annotation `#[{}]`", a.name), a.name_span)
                        .add_note(format!("PHV knows {ANNOTATIONS}"));
                    continue;
                }
            };
            if !allowed.contains(&target) {
                let what = match target {
                    AnnTarget::Function => "a function",
                    AnnTarget::Method => "a method",
                    AnnTarget::Class => "a class",
                    AnnTarget::Property => "a property",
                    AnnTarget::Const => "a constant",
                };
                self.error(format!("`#[{}]` cannot be written before {what}", a.name), a.name_span);
                continue;
            }
            // `Deprecated` and `Skip` take an optional reason; the others nothing.
            let takes_text = matches!(a.name.as_str(), "Deprecated" | "Skip");
            let text = match (a.args.as_slice(), takes_text) {
                ([], _) => String::new(),
                ([arg], true) if arg.name.is_none() => match &arg.value.kind {
                    ExprKind::Str(s) => s.clone(),
                    _ => {
                        self.error(format!("`#[{}]` takes a string literal", a.name), arg.value.span);
                        continue;
                    }
                },
                _ => {
                    let msg = if takes_text {
                        format!("`#[{}]` takes one string, the reason", a.name)
                    } else {
                        format!("`#[{}]` takes no arguments", a.name)
                    };
                    self.error(msg, a.span);
                    continue;
                }
            };
            match a.name.as_str() {
                "Deprecated" => out.deprecated = Some(text),
                "Override" => out.is_override = true,
                "Test" => out.test = true,
                _ => out.skip = Some(text),
            }
        }
        out
    }

    /// The warning for using something marked `#[Deprecated]` that belongs to
    /// class `owner` (none inside that class itself, which still uses it).
    fn warn_deprecated(&mut self, what: String, reason: Option<String>, owner: Option<ClassId>, span: Span) {
        let Some(reason) = reason else { return };
        if owner.is_some() && self.ctx.last().and_then(|c| c.class) == owner {
            return;
        }
        let msg =
            if reason.is_empty() { format!("{what} is deprecated") } else { format!("{what} is deprecated: {reason}") };
        self.warn(msg, span);
    }

    fn run(&mut self) {
        self.collect();
        self.resolve_imports();
        let order = self.order_classes();
        self.resolve_classes(&order);
        self.resolve_functions();
        self.check_bodies();
        self.warn_unused_imports();
    }

    /// Warns about imported names that their module never uses. Only the
    /// project's own code is checked (not `std` or the prelude).
    fn warn_unused_imports(&mut self) {
        let used = std::mem::take(&mut *self.used_names.borrow_mut());
        for (mi, m) in self.input.modules.iter().enumerate() {
            if matches!(m.kind, ModuleKind::Std | ModuleKind::Prelude) {
                continue;
            }
            for s in &m.ast.stmts {
                let StmtKind::Import(d) = &s.kind else { continue };
                for (name, span) in &d.names {
                    let known = self.scopes[mi].funcs.contains_key(name)
                        || self.scopes[mi].classes.contains_key(name)
                        || self.scopes[mi].consts.contains_key(name);
                    if known && !used.contains(&(mi, name.clone())) {
                        self.diags.push(
                            Diagnostic::warning(format!("`{name}` is imported but never used"), *span)
                                .note("remove it from the `import`")
                                .code(phv_syntax::diag::UNUSED),
                        );
                    }
                }
            }
        }
    }

    /// Warns about the locals of a finished function that are never read.
    pub(crate) fn warn_unused_locals(&mut self, ctx: &FnCtx, skip: &[LocalId]) {
        if matches!(self.module_kind(), ModuleKind::Std | ModuleKind::Prelude) {
            return;
        }
        let captured: HashSet<LocalId> = ctx.captures.iter().map(|(inner, _)| *inner).collect();
        for id in ctx.unused_locals(skip) {
            let name = &ctx.locals[id as usize].name;
            let span = ctx.spans[id as usize];
            let note = if ctx.catch_vars.contains(&id) {
                "leave the variable out: `catch (SomeException)`".to_string()
            } else if captured.contains(&id) {
                "remove it from `use (...)`".to_string()
            } else {
                format!("remove it, or start its name with `_` (`$_{name}`) if it is meant to be unused")
            };
            self.diags.push(
                Diagnostic::warning(format!("variable `${name}` is never used"), span)
                    .note(note)
                    .code(phv_syntax::diag::UNUSED),
            );
        }
    }

    /// Warns (once per block) about a statement that runs after control has
    /// already left the block.
    pub(crate) fn note_unreachable(&mut self, s: &ast::Stmt, warned: &mut bool) {
        if *warned || !self.cx().flow.dead {
            return;
        }
        if let StmtKind::InlineHtml(text, _) = &s.kind {
            if text.trim().is_empty() {
                return;
            }
        }
        *warned = true;
        if matches!(self.module_kind(), ModuleKind::Std | ModuleKind::Prelude) {
            return;
        }
        self.diags.push(
            Diagnostic::warning("unreachable code", s.span)
                .note("the code before it always leaves here (`return`, `throw`, `break`, `continue` or a loop that never ends)")
                .code(phv_syntax::diag::UNUSED),
        );
    }

    /// Records that the current module used `name` from its own scope.
    fn note_used(&self, name: &str) {
        let key = (self.module, name.to_string());
        let mut used = self.used_names.borrow_mut();
        if !used.contains(&key) {
            used.insert(key);
        }
    }

    // ----- editor index ----------------------------------------------------

    /// Records that the name at `use_` refers to the definition at `def`.
    pub(crate) fn note_ref(&mut self, use_: Span, def: Span) {
        if let Some(ix) = &mut self.index {
            if use_ != def {
                ix.refs.push((use_, def));
            }
        }
    }

    /// Records a `->name` (or `::name`) looked up on `recv`.
    pub(crate) fn note_site(&mut self, span: Span, recv: &Type, is_static: bool) {
        if self.index.is_none() {
            return;
        }
        let class = self.ctx.last().and_then(|c| c.class);
        if let Some(ix) = &mut self.index {
            ix.sites.push(MemberSite { span, recv: recv.clone(), is_static, class });
        }
    }

    /// Records the variables of a function whose code covers `span`.
    pub(crate) fn note_scope(&mut self, span: Span, ctx: &FnCtx) {
        if self.index.is_none() {
            return;
        }
        let vars = ctx
            .locals
            .iter()
            .zip(&ctx.spans)
            .filter(|(l, _)| !l.name.starts_with('%'))
            .map(|(l, s)| Item {
                name: l.name.clone(),
                kind: ItemKind::Variable,
                detail: self.tn(&l.ty),
                def: Some(*s),
            })
            .collect();
        if let Some(ix) = &mut self.index {
            ix.scopes.push(VarScope { span, sees_outer: ctx.auto_capture, vars });
        }
    }

    /// `lookup_class`, recording the use of the name at `span`.
    pub(crate) fn class_ref(&mut self, name: &str, span: Span) -> Option<ClassId> {
        let c = self.lookup_class(name)?;
        let def = self.classes[c as usize].span;
        self.note_ref(span, def);
        Some(c)
    }

    /// Where the constant `name` visible in the current module is declared.
    pub(crate) fn const_span(&self, name: &str) -> Option<Span> {
        if let Some(s) = self.scopes[self.module].const_spans.get(name) {
            return Some(*s);
        }
        self.prelude().and_then(|p| self.scopes[p].const_spans.get(name).copied())
    }

    /// Where the enum case or class constant `cid::name` is declared.
    pub(crate) fn member_const_span(&self, cid: ClassId, name: &str) -> Option<Span> {
        let mut c = Some(cid);
        while let Some(id) = c {
            let info = &self.classes[id as usize];
            if let Some(d) = info.decl {
                if let Some(case) = d.cases.iter().find(|k| k.name == name) {
                    return Some(case.span);
                }
                if let Some(k) = d.consts.iter().find(|k| k.name == name) {
                    return Some(k.name_span);
                }
            }
            c = info.parent;
        }
        None
    }

    fn signature(&self, f: FuncId) -> String {
        let info = &self.funcs[f as usize];
        let params: Vec<String> = info.params.iter().map(|p| format!("{} ${}", self.tn(&p.ty), p.name)).collect();
        if info.kind == FuncKind::Ctor {
            return format!("({})", params.join(", "));
        }
        format!("({}): {}", params.join(", "), self.tn(&info.ret))
    }

    /// `signature` of a method of class `c` (perhaps inherited), with its types
    /// in terms of `c`'s own type parameters.
    fn signature_in(&self, f: FuncId, c: ClassId) -> String {
        let info = &self.funcs[f as usize];
        let Some(owner) = info.class else { return self.signature(f) };
        let Type::Class(_, own) = self.own_type(c) else { return self.signature(f) };
        let view = |t: &Type| self.tn(&self.member_type(t, c, &own, owner));
        let params: Vec<String> = info.params.iter().map(|p| format!("{} ${}", view(&p.ty), p.name)).collect();
        if info.kind == FuncKind::Ctor {
            return format!("({})", params.join(", "));
        }
        format!("({}): {}", params.join(", "), view(&info.ret))
    }

    fn builtin_items(&self, recv: Option<Recv>) -> Vec<Item> {
        let mut out: Vec<Item> = Vec::new();
        for sig in BUILTINS.iter().filter(|b| b.recv == recv && !b.internal) {
            if out.iter().any(|i| i.name == sig.name) {
                continue;
            }
            let Some(b) = self.builtins.get(sig.name).and_then(|l| l.iter().find(|b| b.sig.recv == recv)) else {
                continue;
            };
            let skip = usize::from(recv.is_some());
            let params: Vec<String> = b.params.iter().skip(skip).map(|t| self.tn(t)).collect();
            out.push(Item {
                name: sig.name.to_string(),
                kind: if recv.is_some() { ItemKind::Method } else { ItemKind::Function },
                detail: format!("({}): {}", params.join(", "), self.tn(&b.ret)),
                def: None,
            });
        }
        out
    }

    /// Fills in the members of every class and the names visible in the entry module.
    fn build_index(&mut self) -> EditorIndex {
        let mut ix = self.index.take().unwrap_or_default();
        ix.class_names = self.class_names.clone();
        ix.type_params = self.classes.iter().map(|c| c.type_params.clone()).collect();
        for (cid, c) in self.classes.iter().enumerate() {
            let cid = cid as ClassId;
            let Type::Class(_, own) = self.own_type(cid) else { continue };
            let mut members = Vec::new();
            for f in &c.fields {
                members.push(Member {
                    item: Item {
                        name: f.name.clone(),
                        kind: ItemKind::Property,
                        detail: self.tn(&self.member_type(&f.ty, cid, &own, f.owner)),
                        def: Some(f.span),
                    },
                    is_static: false,
                    vis: f.vis,
                    owner: f.owner,
                });
            }
            let mut methods: Vec<(&String, &MethodInfo)> = c.methods.iter().collect();
            methods.sort_by_key(|(n, _)| n.as_str());
            for (name, m) in methods {
                members.push(Member {
                    item: Item {
                        name: name.clone(),
                        kind: ItemKind::Method,
                        detail: self.signature_in(m.func, cid),
                        def: Some(self.funcs[m.func as usize].span),
                    },
                    is_static: m.is_static,
                    vis: m.vis,
                    owner: m.owner,
                });
            }
            for case in c.decl.map(|d| d.cases.as_slice()).unwrap_or_default() {
                members.push(Member {
                    item: Item {
                        name: case.name.clone(),
                        kind: ItemKind::EnumCase,
                        detail: c.name.clone(),
                        def: Some(case.span),
                    },
                    is_static: true,
                    vis: Visibility::Public,
                    owner: cid,
                });
            }
            let mut consts: Vec<(&String, &(HConst, Type))> = c.consts.iter().collect();
            consts.sort_by_key(|(n, _)| n.as_str());
            for (name, (_, ty)) in consts {
                members.push(Member {
                    item: Item {
                        name: name.clone(),
                        kind: ItemKind::Constant,
                        detail: self.tn(ty),
                        def: self.member_const_span(cid, name),
                    },
                    is_static: true,
                    vis: Visibility::Public,
                    owner: cid,
                });
            }
            ix.classes.push(members);
        }
        // A template opened on its own is the first template (it is loaded before the ones it renders).
        let entry = self
            .input
            .modules
            .iter()
            .position(|m| m.kind == ModuleKind::Entry)
            .or_else(|| self.input.modules.iter().position(|m| m.kind == ModuleKind::Template));
        for scope in entry.into_iter().chain(self.prelude()) {
            let s = &self.scopes[scope];
            for (name, f) in &s.funcs {
                let def = Some(self.funcs[*f as usize].span);
                ix.globals.push(Item { name: name.clone(), kind: ItemKind::Function, detail: self.signature(*f), def });
            }
            for (name, c) in &s.classes {
                let info = &self.classes[*c as usize];
                let kind = match info.kind {
                    ClassKind::Interface => ItemKind::Interface,
                    ClassKind::Enum => ItemKind::Enum,
                    ClassKind::Class | ClassKind::Abstract => ItemKind::Class,
                };
                let detail = info.ctor.map(|f| self.signature(f)).unwrap_or_default();
                ix.globals.push(Item { name: name.clone(), kind, detail, def: Some(info.span) });
            }
            for (name, (_, ty)) in &s.consts {
                let def = s.const_spans.get(name).copied();
                ix.globals.push(Item { name: name.clone(), kind: ItemKind::Constant, detail: self.tn(ty), def });
            }
        }
        ix.globals.extend(self.builtin_items(None));
        ix.globals.sort_by(|a, b| a.name.cmp(&b.name));
        ix.globals.dedup_by(|a, b| a.name == b.name);
        ix.string_methods = self.builtin_items(Some(Recv::Str));
        ix.list_methods = self.builtin_items(Some(Recv::List));
        ix.map_methods = self.builtin_items(Some(Recv::Map));
        ix
    }

    // ----- helpers -------------------------------------------------------

    fn error(&mut self, msg: impl Into<String>, span: Span) -> &mut Diagnostic {
        self.diags.push(Diagnostic::error(msg, span));
        self.diags.last_mut().unwrap()
    }

    fn warn(&mut self, msg: impl Into<String>, span: Span) -> &mut Diagnostic {
        self.diags.push(Diagnostic::warning(msg, span));
        self.diags.last_mut().unwrap()
    }

    fn tn(&self, t: &Type) -> String {
        t.display(&self.class_names).to_string()
    }

    fn module_kind(&self) -> ModuleKind {
        self.input.modules[self.module].kind
    }

    fn add_func(&mut self, info: FuncInfo<'a>) -> FuncId {
        self.funcs.push(info);
        self.hfuncs.push(None);
        self.funcs.len() as FuncId - 1
    }

    fn prelude(&self) -> Option<usize> {
        self.input.modules.iter().position(|m| m.kind == ModuleKind::Prelude)
    }

    pub(crate) fn lookup_class(&self, name: &str) -> Option<ClassId> {
        if let Some(c) = self.scopes[self.module].classes.get(name) {
            self.note_used(name);
            return Some(*c);
        }
        self.prelude().and_then(|p| self.scopes[p].classes.get(name).copied())
    }

    pub(crate) fn lookup_func(&self, name: &str) -> Option<FuncId> {
        if let Some(f) = self.scopes[self.module].funcs.get(name) {
            self.note_used(name);
            return Some(*f);
        }
        self.prelude().and_then(|p| self.scopes[p].funcs.get(name).copied())
    }

    pub(crate) fn builtin_overloads(&self, name: &str, recv: Option<Recv>) -> Option<Vec<&ParsedBuiltin>> {
        let internal_ok = matches!(self.module_kind(), ModuleKind::Std | ModuleKind::Prelude);
        let list: Vec<&ParsedBuiltin> = self
            .builtins
            .get(name)?
            .iter()
            .filter(|b| b.sig.recv == recv && (internal_ok || !b.sig.internal))
            .collect();
        if list.is_empty() {
            None
        } else {
            Some(list)
        }
    }

    fn is_builtin_name(&self, name: &str) -> bool {
        self.builtins.get(name).is_some_and(|l| l.iter().any(|b| b.sig.recv.is_none()))
    }

    pub(crate) fn is_subclass(&self, c: ClassId, target: ClassId) -> bool {
        if c == target {
            return true;
        }
        let ci = &self.classes[c as usize];
        if !ci.supers.is_empty() {
            return ci.supers.contains(&target);
        }
        ci.parent.is_some_and(|p| self.is_subclass(p, target)) || ci.ifaces.iter().any(|i| self.is_subclass(*i, target))
    }

    pub(crate) fn lookup_const(&self, name: &str) -> Option<(HConst, Type)> {
        if let Some(c) = self.scopes[self.module].consts.get(name) {
            self.note_used(name);
            return Some(c.clone());
        }
        self.prelude().and_then(|p| self.scopes[p].consts.get(name).cloned())
    }

    pub(crate) fn class_kind(&self, c: ClassId) -> ClassKind {
        self.classes[c as usize].kind
    }

    fn func_is_final(&self, f: FuncId) -> bool {
        matches!(self.funcs[f as usize].body, Body::Function(d) if d.is_final)
    }

    pub(crate) fn func_is_abstract(&self, f: FuncId) -> bool {
        self.funcs[f as usize].is_abstract
    }

    /// The value of a `const` declaration: a scalar literal.
    fn literal_const(
        &mut self,
        e: &ast::Expr,
        ty: Option<&TypeExpr>,
        self_class: Option<ClassId>,
    ) -> Option<(HConst, Type)> {
        let (c, t) = match &e.kind {
            ExprKind::Int(i) => (HConst::Int(*i), Type::Int),
            ExprKind::Float(f) => (HConst::Float(*f), Type::Float),
            ExprKind::Str(s) => (HConst::Str(s.clone()), Type::Str),
            ExprKind::Bool(b) => (HConst::Bool(*b), Type::Bool),
            ExprKind::Null => (HConst::Null, Type::Null),
            _ => {
                self.error("constants must be literals (int, float, string, bool or null)", e.span);
                return None;
            }
        };
        let Some(ty) = ty else { return Some((c, t)) };
        let declared = self.resolve_type(ty, self_class);
        let c = self.const_default(e, &declared)?;
        Some((c, declared))
    }

    /// The closest class that every class in `classes` extends.
    pub(crate) fn common_ancestor(&self, classes: &[ClassId]) -> Option<ClassId> {
        let (&first, rest) = classes.split_first()?;
        let mut cand = Some(first);
        while let Some(c) = cand {
            if rest.iter().all(|r| self.is_subclass(*r, c)) {
                return Some(c);
            }
            cand = self.classes[c as usize].parent;
        }
        None
    }

    /// For a union made only of classes, their common ancestor as a class type
    /// (when they all use it with the same type arguments).
    pub(crate) fn as_single_class(&self, t: &Type) -> Option<Type> {
        let Type::Union(ms) = t else { return None };
        let classes: Option<Vec<(ClassId, &Vec<Type>)>> =
            ms.iter().map(|m| if let Type::Class(c, a) = m { Some((*c, a)) } else { None }).collect();
        let classes = classes?;
        let ids: Vec<ClassId> = classes.iter().map(|(c, _)| *c).collect();
        let anc = self.common_ancestor(&ids)?;
        let mut args = None;
        for (c, a) in classes {
            let up = self.upcast(c, a, anc)?;
            if args.as_ref().is_some_and(|x| *x != up) {
                return None;
            }
            args = Some(up);
        }
        Some(Type::Class(anc, args.unwrap_or_default()))
    }

    /// The class type `c` as seen inside its own declaration: its type
    /// parameters as arguments (`Box<T>` inside `class Box<T>`).
    pub(crate) fn own_type(&self, c: ClassId) -> Type {
        Type::Class(c, self.classes[c as usize].type_params.iter().map(|n| Type::Param(n.clone())).collect())
    }

    /// The type arguments of `target` (an ancestor or interface of `c`) when
    /// `c` is used with `args`. None if `target` is not a super of `c`.
    pub(crate) fn upcast(&self, c: ClassId, args: &[Type], target: ClassId) -> Option<Vec<Type>> {
        if c == target {
            return Some(args.to_vec());
        }
        if !self.is_subclass(c, target) {
            return None;
        }
        let ci = &self.classes[c as usize];
        Some(match ci.super_args.get(&target) {
            Some(sa) => sa.iter().map(|t| subst_params(t, &ci.type_params, args)).collect(),
            None => Vec::new(),
        })
    }

    /// The type `t` of a member declared in `owner`, read through a value of
    /// type `c<args>`: the owner's type parameters replaced by its arguments.
    pub(crate) fn member_type(&self, t: &Type, c: ClassId, args: &[Type], owner: ClassId) -> Type {
        let tps = &self.classes[owner as usize].type_params;
        if tps.is_empty() {
            return t.clone();
        }
        let owner_args = self.upcast(c, args, owner).unwrap_or_default();
        subst_params(t, tps, &owner_args)
    }

    /// The member types of `owner` substituted for a value of type `c<args>`,
    /// as `(name, type)` pairs for `invoke_with`.
    pub(crate) fn receiver_args(&self, c: ClassId, args: &[Type], owner: ClassId) -> Vec<(String, Type)> {
        let tps = self.classes[owner as usize].type_params.clone();
        if tps.is_empty() {
            return Vec::new();
        }
        let owner_args = self.upcast(c, args, owner).unwrap_or_default();
        tps.into_iter().enumerate().map(|(i, n)| (n, owner_args.get(i).cloned().unwrap_or(Type::Error))).collect()
    }

    /// Resolves the type arguments written after a generic class's name,
    /// checking their number.
    fn class_args(&mut self, c: ClassId, args: &[TypeExpr], self_class: Option<ClassId>, span: Span) -> Vec<Type> {
        let want = self.classes[c as usize].type_params.len();
        let resolved: Vec<Type> = args.iter().map(|a| self.resolve_type(a, self_class)).collect();
        if resolved.len() != want {
            let name = self.class_names[c as usize].clone();
            let msg = match want {
                0 => format!("`{name}` does not take type arguments"),
                1 => format!("`{name}` takes 1 type argument"),
                n => format!("`{name}` takes {n} type arguments"),
            };
            self.error(msg, span);
            return vec![Type::Error; want];
        }
        resolved
    }

    pub(crate) fn exception_class(&self) -> Option<ClassId> {
        self.prelude().and_then(|p| self.scopes[p].classes.get("Exception").copied())
    }

    fn resolve_type(&mut self, t: &TypeExpr, self_class: Option<ClassId>) -> Type {
        match &t.kind {
            TypeExprKind::Named(n, args) => {
                let simple = |ty: Type| (ty, 0usize);
                if self.type_params.iter().any(|p| p == n) {
                    if !args.is_empty() {
                        self.error(format!("type parameter `{n}` does not take type arguments"), t.span);
                    }
                    return Type::Param(n.clone());
                }
                let (ty, arity) = match n.as_str() {
                    "Sql" => simple(Type::Sql),
                    "int" => simple(Type::Int),
                    "float" => simple(Type::Float),
                    "string" => simple(Type::Str),
                    "bool" => simple(Type::Bool),
                    "void" => simple(Type::Void),
                    "null" => simple(Type::Null),
                    "any" => simple(Type::Any),
                    "Html" => simple(Type::Html),
                    "SafeUrl" => simple(Type::SafeUrl),
                    "SafeJs" => simple(Type::SafeJs),
                    "list" => {
                        if args.len() != 1 {
                            self.error("`list` takes one type argument, e.g. `list<string>`", t.span);
                            return Type::Error;
                        }
                        let inner = self.resolve_type(&args[0], self_class);
                        return Type::list(inner);
                    }
                    "map" => {
                        if args.len() != 2 {
                            self.error("`map` takes two type arguments, e.g. `map<string, int>`", t.span);
                            return Type::Error;
                        }
                        let k = self.resolve_type(&args[0], self_class);
                        let v = self.resolve_type(&args[1], self_class);
                        if !matches!(k, Type::Int | Type::Str | Type::Error) {
                            let msg = format!("map keys must be `int` or `string`, not `{}`", self.tn(&k));
                            self.error(msg, args[0].span);
                        }
                        return Type::map(k, v);
                    }
                    "self" => match self_class {
                        Some(c) => simple(self.own_type(c)),
                        None => {
                            self.error("`self` can only be used inside a class", t.span);
                            return Type::Error;
                        }
                    },
                    "mixed" => {
                        self.error("`mixed` does not exist in PHV", t.span)
                            .add_note("use `any` and narrow it with `is` before use");
                        return Type::Error;
                    }
                    "array" => {
                        self.error("`array` does not exist in PHV", t.span)
                            .add_note("use `list<T>` for sequences or `map<K, V>` for key/value data");
                        return Type::Error;
                    }
                    "callable" | "Closure" => {
                        self.error(format!("`{n}` does not exist in PHV"), t.span)
                            .add_note("use a function type such as `fn(int): string`");
                        return Type::Error;
                    }
                    other => match self.class_ref(other, t.span) {
                        Some(c) => {
                            let args = self.class_args(c, args, self_class, t.span);
                            return Type::Class(c, args);
                        }
                        None => {
                            self.error(format!("unknown type `{other}`"), t.span);
                            return Type::Error;
                        }
                    },
                };
                if args.len() != arity {
                    self.error(format!("`{n}` does not take type arguments"), t.span);
                }
                ty
            }
            TypeExprKind::Nullable(inner) => {
                let i = self.resolve_type(inner, self_class);
                if i == Type::Void {
                    self.error("`void` cannot be nullable", t.span);
                }
                Type::nullable(i)
            }
            TypeExprKind::Union(ms) => {
                let members: Vec<Type> = ms.iter().map(|m| self.resolve_type(m, self_class)).collect();
                if members.contains(&Type::Void) {
                    self.error("`void` cannot be part of a union", t.span);
                }
                Type::union(members)
            }
            TypeExprKind::Func(ps, r) => {
                let params = ps.iter().map(|p| self.resolve_type(p, self_class)).collect();
                let ret = self.resolve_type(r, self_class);
                Type::Func(params, Box::new(ret))
            }
        }
    }

    /// Default values for parameters and properties must be constants.
    fn const_default(&mut self, e: &ast::Expr, ty: &Type) -> Option<HConst> {
        let (c, cty) = match &e.kind {
            ExprKind::Int(i) => {
                if ty.contains(&Type::Float) && !ty.contains(&Type::Int) {
                    (HConst::Float(*i as f64), Type::Float)
                } else {
                    (HConst::Int(*i), Type::Int)
                }
            }
            ExprKind::Float(f) => (HConst::Float(*f), Type::Float),
            ExprKind::Str(s) => (HConst::Str(s.clone()), Type::Str),
            ExprKind::Bool(b) => (HConst::Bool(*b), Type::Bool),
            ExprKind::Null => (HConst::Null, Type::Null),
            ExprKind::List(items) if items.is_empty() => match ty.without_null() {
                t @ Type::List(_) => (HConst::EmptyList, t),
                _ => (HConst::EmptyList, Type::list(Type::Error)),
            },
            ExprKind::Map(items) if items.is_empty() => (HConst::EmptyMap, Type::map(Type::Error, Type::Error)),
            _ => {
                self.error("default values must be constants (a literal, `null` or `[]`)", e.span);
                return None;
            }
        };
        let ok = match (&c, ty.without_null()) {
            (HConst::EmptyList, Type::List(_)) => true,
            (HConst::EmptyList, Type::Map(..)) => true,
            (HConst::EmptyMap, Type::Map(..)) => true,
            _ => self.assignable(&cty, ty),
        };
        if !ok {
            let msg = format!("default value of type `{}` does not match `{}`", self.tn(&cty), self.tn(ty));
            self.error(msg, e.span);
            return None;
        }
        // `[]` with a map type is an empty map.
        if c == HConst::EmptyList && matches!(ty.without_null(), Type::Map(..)) {
            return Some(HConst::EmptyMap);
        }
        Some(c)
    }

    fn resolve_params(&mut self, params: &[ast::Param], self_class: Option<ClassId>) -> Vec<ParamInfo> {
        let mut out: Vec<ParamInfo> = Vec::new();
        let mut seen: HashSet<&str> = HashSet::new();
        for p in params {
            let ty = match &p.ty {
                Some(t) => self.resolve_type(t, self_class),
                None => {
                    self.error(format!("parameter `${}` needs a type", p.name), p.span)
                        .add_note(format!("for example `string ${}`", p.name));
                    Type::Error
                }
            };
            if ty == Type::Void {
                self.error("parameters cannot be `void`", p.span);
            }
            let default = p.default.as_ref().and_then(|d| self.const_default(d, &ty));
            if !seen.insert(p.name.as_str()) {
                self.error(format!("duplicate parameter `${}`", p.name), p.span);
            }
            out.push(ParamInfo { name: p.name.clone(), ty, default, promote: p.promote, span: p.span });
        }
        out
    }

    // ----- phase 1: declarations -----------------------------------------

    fn collect(&mut self) {
        for (mi, m) in self.input.modules.iter().enumerate() {
            self.module = mi;
            let span0 = Span::new(m.ast.file, 0, 0);
            match m.kind {
                ModuleKind::Template => {
                    let fid = self.add_func(FuncInfo {
                        name: format!("template {}", m.name),
                        type_params: Vec::new(),
                        is_abstract: false,
                        params: Vec::new(),
                        ret: Type::Html,
                        body: Body::Template(m.ast),
                        module: mi,
                        class: None,
                        is_static: true,
                        kind: FuncKind::Template,
                        span: span0,
                    });
                    self.template_funcs.insert(mi, fid);
                }
                ModuleKind::Entry => {
                    let fid = self.add_func(FuncInfo {
                        name: "{main}".into(),
                        type_params: Vec::new(),
                        is_abstract: false,
                        params: Vec::new(),
                        ret: Type::Void,
                        body: Body::Main(m.ast),
                        module: mi,
                        class: None,
                        is_static: true,
                        kind: FuncKind::Main,
                        span: span0,
                    });
                    self.main = Some(fid);
                }
                _ => {}
            }
            let mut reported_code = false;
            for s in &m.ast.stmts {
                match &s.kind {
                    StmtKind::Function(d) => {
                        if m.kind == ModuleKind::Template {
                            self.error(
                                "templates cannot declare functions; put them in a module and import it",
                                d.name_span,
                            );
                            continue;
                        }
                        if self.is_builtin_name(&d.name) {
                            self.error(
                                format!("`{}` is a built-in function and cannot be redefined", d.name),
                                d.name_span,
                            );
                        }
                        let fid = self.add_func(FuncInfo {
                            name: d.name.clone(),
                            type_params: d.type_params.iter().map(|(n, _)| n.clone()).collect(),
                            is_abstract: false,
                            params: Vec::new(),
                            ret: Type::Void,
                            body: Body::Function(d),
                            module: mi,
                            class: None,
                            is_static: true,
                            kind: FuncKind::Normal,
                            span: d.name_span,
                        });
                        let ann = self.annotations(&d.attrs, AnnTarget::Function);
                        if let Some(r) = ann.deprecated {
                            self.deprecated_funcs.insert(fid, r);
                        }
                        if ann.test || ann.skip.is_some() {
                            if !d.params.is_empty() {
                                self.error("a test cannot take parameters", d.name_span);
                            }
                            self.tests.push((fid, ann.skip));
                        }
                        let scope = &mut self.scopes[mi];
                        if scope.funcs.insert(d.name.clone(), fid).is_some() {
                            self.error(format!("function `{}` is defined more than once", d.name), d.name_span);
                        }
                        if d.exported {
                            self.scopes[mi].export_funcs.insert(d.name.clone(), fid);
                        }
                    }
                    StmtKind::Class(d) => {
                        if m.kind == ModuleKind::Template {
                            self.error(
                                "templates cannot declare classes; put them in a module and import it",
                                d.name_span,
                            );
                            continue;
                        }
                        let cid = self.classes.len() as ClassId;
                        self.classes.push(ClassInfo {
                            name: d.name.clone(),
                            decl: Some(d),
                            kind: d.kind,
                            module: mi,
                            type_params: d.type_params.iter().map(|(n, _)| n.clone()).collect(),
                            super_args: HashMap::new(),
                            parent: None,
                            ifaces: Vec::new(),
                            supers: HashSet::new(),
                            fields: Vec::new(),
                            field_slots: HashMap::new(),
                            methods: HashMap::new(),
                            vtable: Vec::new(),
                            ctor: None,
                            ctor_access: (Visibility::Public, 0),
                            consts: HashMap::new(),
                            cases: Vec::new(),
                            backing: None,
                            span: d.name_span,
                        });
                        self.class_names.push(d.name.clone());
                        let ann = self.annotations(&d.attrs, AnnTarget::Class);
                        if let Some(r) = ann.deprecated {
                            self.deprecated_classes.insert(cid, r);
                        }
                        let exists = self.scopes[mi].classes.insert(d.name.clone(), cid).is_some()
                            || (m.kind != ModuleKind::Prelude
                                && self.prelude().is_some_and(|p| self.scopes[p].classes.contains_key(&d.name)));
                        if exists {
                            self.error(format!("class `{}` is defined more than once", d.name), d.name_span);
                        }
                        if d.exported {
                            self.scopes[mi].export_classes.insert(d.name.clone(), cid);
                        }
                    }
                    StmtKind::Const(d) => {
                        self.annotations(&d.attrs, AnnTarget::Const);
                        let Some(value) = self.literal_const(&d.value, d.ty.as_ref(), None) else { continue };
                        self.scopes[mi].const_spans.insert(d.name.clone(), d.name_span);
                        if self.scopes[mi].consts.insert(d.name.clone(), value.clone()).is_some() {
                            self.error(format!("constant `{}` is defined more than once", d.name), d.name_span);
                        }
                        if d.exported {
                            self.scopes[mi].export_consts.insert(d.name.clone(), value);
                        }
                    }
                    StmtKind::Import(_) | StmtKind::Params(_) => {}
                    _ => {
                        let code_allowed = matches!(m.kind, ModuleKind::Entry | ModuleKind::Template);
                        if !code_allowed && !reported_code {
                            reported_code = true;
                            self.error(
                                "only declarations are allowed at the top level of an imported module",
                                s.span,
                            )
                            .add_note("imported modules may contain `function`, `class`, `interface`, `enum`, `const` and `import`; code runs only in the entry script");
                        }
                    }
                }
            }
        }
    }

    // ----- phase 2: imports ----------------------------------------------

    fn resolve_imports(&mut self) {
        for (mi, m) in self.input.modules.iter().enumerate() {
            self.module = mi;
            for s in &m.ast.stmts {
                let StmtKind::Import(d) = &s.kind else { continue };
                let key = d.module.join(".");
                let Some(&target) = self.input.imports.get(&key) else {
                    self.error(format!("unknown module `{key}`"), d.span);
                    continue;
                };
                self.note_ref(d.span, Span::new(self.input.modules[target].ast.file, 0, 0));
                for (name, span) in &d.names {
                    let f = self.scopes[target].export_funcs.get(name).copied();
                    let c = self.scopes[target].export_classes.get(name).copied();
                    let def = match (f, c) {
                        (Some(f), _) => Some(self.funcs[f as usize].span),
                        (_, Some(c)) => Some(self.classes[c as usize].span),
                        _ => self.scopes[target].const_spans.get(name).copied(),
                    };
                    if let Some(def) = def {
                        self.note_ref(*span, def);
                    }
                    if let Some(k) = self.scopes[target].export_consts.get(name).cloned() {
                        if let Some(def) = def {
                            self.scopes[mi].const_spans.insert(name.clone(), def);
                        }
                        if self.scopes[mi].consts.insert(name.clone(), k).is_some() {
                            self.error(format!("`{name}` is already defined in this module"), *span);
                        }
                        continue;
                    }
                    if f.is_none() && c.is_none() {
                        let declared = self.scopes[target].funcs.contains_key(name)
                            || self.scopes[target].classes.contains_key(name)
                            || self.scopes[target].consts.contains_key(name);
                        let e = self.error(format!("module `{key}` has no exported `{name}`"), *span);
                        if declared {
                            e.add_note(format!("`{name}` exists but is not exported; add `export` to its declaration"));
                        }
                        continue;
                    }
                    let scope = &mut self.scopes[mi];
                    let clash = match (f, c) {
                        (Some(f), _) => scope.funcs.insert(name.clone(), f).is_some_and(|old| old != f),
                        (_, Some(c)) => scope.classes.insert(name.clone(), c).is_some_and(|old| old != c),
                        _ => false,
                    };
                    if clash {
                        self.error(format!("`{name}` is already defined in this module"), *span);
                    }
                }
            }
        }
    }

    // ----- phase 3/4: classes --------------------------------------------

    fn order_classes(&mut self) -> Vec<ClassId> {
        for cid in 0..self.classes.len() {
            let Some(decl) = self.classes[cid].decl else { continue };
            self.module = self.classes[cid].module;
            if let Some((pname, pspan)) = &decl.parent {
                match self.class_ref(pname, *pspan) {
                    Some(p) => self.classes[cid].parent = Some(p),
                    None => {
                        self.error(format!("unknown parent class `{pname}`"), *pspan);
                    }
                }
            }
            for (iname, ispan) in &decl.interfaces {
                match self.class_ref(iname, *ispan) {
                    Some(i) => self.classes[cid].ifaces.push(i),
                    None => {
                        self.error(format!("unknown interface `{iname}`"), *ispan);
                    }
                }
            }
        }
        // 0 = unvisited, 1 = visiting, 2 = done
        let mut state = vec![0u8; self.classes.len()];
        let mut order = Vec::new();
        for cid in 0..self.classes.len() {
            self.visit_class(cid as ClassId, &mut state, &mut order);
        }
        order
    }

    fn visit_class(&mut self, cid: ClassId, state: &mut [u8], order: &mut Vec<ClassId>) {
        match state[cid as usize] {
            2 => return,
            1 => {
                let span = self.classes[cid as usize].span;
                let name = self.classes[cid as usize].name.clone();
                self.error(format!("`{name}` inherits from itself"), span);
                self.classes[cid as usize].parent = None;
                self.classes[cid as usize].ifaces.clear();
                return;
            }
            _ => {}
        }
        state[cid as usize] = 1;
        if let Some(p) = self.classes[cid as usize].parent {
            self.visit_class(p, state, order);
        }
        for i in self.classes[cid as usize].ifaces.clone() {
            self.visit_class(i, state, order);
        }
        state[cid as usize] = 2;
        order.push(cid);
    }

    /// Whether method `sub` can stand in for `base` (same parameters, compatible
    /// return) in class `through`, both seen through it: `class A extends B<int>`
    /// overrides `B::f(T $x)` with `f(int $x)`.
    fn compatible_override(&self, sub: FuncId, base: FuncId, through: ClassId) -> bool {
        let (a, b) = (&self.funcs[sub as usize], &self.funcs[base as usize]);
        let Type::Class(_, own) = self.own_type(through) else { return false };
        // Each method's own type parameters become positions first, so they
        // match by position and never by name (a method's `T` is not the
        // class's `T`).
        let view = |t: &Type, f: &FuncInfo<'_>| {
            let t = expr::instantiate(t, &f.type_params);
            match f.class {
                Some(o) => self.member_type(&t, through, &own, o),
                None => t,
            }
        };
        a.type_params.len() == b.type_params.len()
            && a.params.len() == b.params.len()
            && a.params.iter().zip(&b.params).all(|(x, y)| view(&x.ty, a) == view(&y.ty, b))
            && self.assignable(&view(&a.ret, a), &view(&b.ret, b))
    }

    /// Counts `n` more inherited members; true (after reporting it once) when
    /// that goes over `MAX_INHERITED_MEMBERS`, and the members are then not copied.
    fn over_inherit_limit(&mut self, n: usize, decl: &ClassDecl) -> bool {
        self.inherited_members += n;
        if self.inherited_members <= MAX_INHERITED_MEMBERS {
            return false;
        }
        if !self.reported_hierarchy {
            self.reported_hierarchy = true;
            self.error(
                format!("the classes inherit more than {MAX_INHERITED_MEMBERS} members in total"),
                decl.name_span,
            )
            .add_note("split the program into smaller class hierarchies");
        }
        true
    }

    /// Copies what class `ci` inherits from its parent `p` (already resolved).
    fn inherit_from(&mut self, ci: usize, p: ClassId) {
        let parent = &self.classes[p as usize];
        let (fields, field_slots, methods, vtable, ctor, ctor_access, consts) = (
            parent.fields.clone(),
            parent.field_slots.clone(),
            parent.methods.clone(),
            parent.vtable.clone(),
            parent.ctor,
            parent.ctor_access,
            parent.consts.clone(),
        );
        let c = &mut self.classes[ci];
        c.fields = fields;
        c.field_slots = field_slots;
        c.methods = methods;
        c.vtable = vtable;
        c.ctor = ctor;
        c.ctor_access = ctor_access;
        c.consts = consts;
    }

    fn resolve_classes(&mut self, order: &[ClassId]) {
        for &cid in order {
            let ci = cid as usize;
            let Some(decl) = self.classes[ci].decl else { continue };
            self.module = self.classes[ci].module;
            let kind = decl.kind;
            let class_name = self.classes[ci].name.clone();
            let class_tps = self.classes[ci].type_params.clone();
            if !class_tps.is_empty() && kind == ClassKind::Enum {
                self.error("enums cannot have type parameters", decl.name_span);
            }
            if class_tps.len() > 4 {
                self.error("at most 4 type parameters are supported", decl.name_span);
            }
            for (i, (n, s)) in decl.type_params.iter().enumerate() {
                if decl.type_params[..i].iter().any(|(m, _)| m == n) {
                    self.error(format!("type parameter `{n}` is declared more than once"), *s);
                }
            }
            // Type arguments of the parent and interfaces, in terms of this class's parameters.
            self.type_params = class_tps.clone();
            let mut super_args: HashMap<ClassId, Vec<Type>> = HashMap::new();
            let mut supers_written: Vec<(ClassId, &[TypeExpr], Span)> = Vec::new();
            if let (Some(p), Some((_, pspan))) = (self.classes[ci].parent, &decl.parent) {
                supers_written.push((p, &decl.parent_args, *pspan));
            }
            for ((iname, ispan), iargs) in decl.interfaces.iter().zip(&decl.interface_args) {
                if let Some(i) = self.lookup_class(iname) {
                    supers_written.push((i, iargs, *ispan));
                }
            }
            for (s, written, span) in supers_written {
                let args = self.class_args(s, written, Some(cid), span);
                let stps = self.classes[s as usize].type_params.clone();
                let mut reached: Vec<(ClassId, Vec<Type>)> = self.classes[s as usize]
                    .super_args
                    .iter()
                    .map(|(k, v)| (*k, v.iter().map(|t| subst_params(t, &stps, &args)).collect()))
                    .collect();
                if !args.is_empty() {
                    reached.push((s, args));
                }
                reached.sort_by_key(|(k, _)| *k);
                // A generic class or interface reached along two paths must
                // have the same type arguments on both: a value seen through
                // either is then the same type.
                for (k, v) in reached {
                    match super_args.get(&k) {
                        Some(old) if *old != v => {
                            let name = self.classes[k as usize].name.clone();
                            let (a, b) = (self.tn(&Type::Class(k, old.clone())), self.tn(&Type::Class(k, v)));
                            self.error(
                                format!("`{class_name}` would be both `{a}` and `{b}`; `{name}` can have one type argument list only"),
                                span,
                            );
                        }
                        Some(_) => {}
                        None => {
                            super_args.insert(k, v);
                        }
                    }
                }
            }
            self.classes[ci].super_args = super_args;
            self.type_params.clear();
            if let Some(p) = self.classes[ci].parent {
                match self.classes[p as usize].kind {
                    ClassKind::Interface => {
                        let span = decl.parent.as_ref().unwrap().1;
                        self.error("a class cannot extend an interface; use `implements`", span);
                    }
                    ClassKind::Enum => {
                        let span = decl.parent.as_ref().unwrap().1;
                        self.error("enums cannot be extended", span);
                    }
                    _ => {}
                }
                if self.classes[p as usize].decl.is_some_and(|d| d.is_final) {
                    let span = decl.parent.as_ref().unwrap().1;
                    let parent_name = self.classes[p as usize].name.clone();
                    self.error(format!("`{class_name}` cannot extend `{parent_name}`, which is `final`"), span);
                }
                let parent = &self.classes[p as usize];
                // Each class holds a copy of what it inherits; a long chain or a wide fan of
                // classes would otherwise take memory and time far out of proportion to the source.
                let inherited = parent.fields.len() + parent.methods.len() + parent.consts.len();
                if self.over_inherit_limit(inherited, decl) {
                    // Checked as if it had no parent; the error stops the program anyway.
                    self.classes[ci].parent = None;
                } else {
                    self.inherit_from(ci, p);
                }
            }
            let mut supers: HashSet<ClassId> = HashSet::from([cid]);
            if let Some(p) = self.classes[ci].parent {
                supers.extend(self.classes[p as usize].supers.iter().copied());
            }
            for (i, (_, ispan)) in self.classes[ci].ifaces.clone().into_iter().zip(&decl.interfaces) {
                if self.classes[i as usize].kind != ClassKind::Interface {
                    let iname = self.classes[i as usize].name.clone();
                    self.error(format!("`{iname}` is not an interface"), *ispan);
                    continue;
                }
                supers.extend(self.classes[i as usize].supers.iter().copied());
                for (k, v) in self.classes[i as usize].consts.clone() {
                    self.classes[ci].consts.entry(k).or_insert(v);
                }
                if kind == ClassKind::Interface
                    && !self.over_inherit_limit(self.classes[i as usize].methods.len(), decl)
                {
                    // Calls through interfaces dispatch by name; the slot belongs to the
                    // other interface's vtable, so it is not copied (a redeclaration gets its own).
                    for (name, m) in self.classes[i as usize].methods.clone() {
                        self.classes[ci].methods.entry(name).or_insert(MethodInfo { slot: None, ..m });
                    }
                }
            }
            // Classes get a vtable slot for every interface method they do not
            // define yet, so abstract classes can call them on `$this`.
            if kind != ClassKind::Interface {
                let mut ifs: Vec<ClassId> =
                    supers.iter().copied().filter(|s| self.classes[*s as usize].kind == ClassKind::Interface).collect();
                ifs.sort();
                for i in ifs {
                    if self.over_inherit_limit(self.classes[i as usize].methods.len(), decl) {
                        break;
                    }
                    let mut ms: Vec<(String, MethodInfo)> =
                        self.classes[i as usize].methods.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                    ms.sort_by(|a, b| a.0.cmp(&b.0));
                    for (name, m) in ms {
                        if self.classes[ci].methods.contains_key(&name) {
                            continue;
                        }
                        let c = &mut self.classes[ci];
                        c.vtable.push(m.func);
                        let slot = Some(c.vtable.len() as u32 - 1);
                        c.methods.insert(name, MethodInfo { slot, ..m });
                    }
                }
            }
            self.classes[ci].supers = supers;
            if matches!(kind, ClassKind::Interface | ClassKind::Enum) {
                if let Some(prop) = decl.props.first() {
                    let what = if kind == ClassKind::Interface { "interfaces" } else { "enums" };
                    self.error(format!("{what} cannot have properties"), prop.span);
                }
            }
            if kind == ClassKind::Enum {
                let backing = decl.backing.as_ref().map(|t| {
                    let ty = self.resolve_type(t, Some(cid));
                    if !matches!(ty, Type::Int | Type::Str | Type::Error) {
                        self.error("enum backing type must be `int` or `string`", t.span);
                    }
                    ty
                });
                let name_field = FieldInfo {
                    name: "name".into(),
                    ty: Type::Str,
                    vis: Visibility::Public,
                    readonly: true,
                    owner: cid,
                    default: Some(HConst::Str(String::new())),
                    required: false,
                    span: decl.name_span,
                };
                self.add_field(cid, name_field);
                if let Some(b) = &backing {
                    let value_field = FieldInfo {
                        name: "value".into(),
                        ty: b.clone(),
                        vis: Visibility::Public,
                        readonly: true,
                        owner: cid,
                        default: Some(HConst::Null),
                        required: false,
                        span: decl.name_span,
                    };
                    self.add_field(cid, value_field);
                }
                for case in &decl.cases {
                    let value = match (&backing, &case.value) {
                        (Some(b), Some(v)) => self.const_default(v, &b.clone()).unwrap_or(HConst::Null),
                        (Some(_), None) => {
                            self.error(format!("case `{}` of a backed enum needs a value", case.name), case.span);
                            HConst::Null
                        }
                        (None, Some(v)) => {
                            self.error(
                                "cases of a pure enum cannot have values; add a backing type, e.g. `enum E: string`",
                                v.span,
                            );
                            HConst::Null
                        }
                        (None, None) => HConst::Null,
                    };
                    let dup_name = self.classes[ci].cases.iter().any(|(n, _)| *n == case.name);
                    let dup_value = value != HConst::Null && self.classes[ci].cases.iter().any(|(_, v)| *v == value);
                    if dup_name {
                        self.error(format!("case `{}` is declared more than once", case.name), case.span);
                    } else if dup_value {
                        self.error("two cases have the same value", case.span);
                    }
                    self.classes[ci].cases.push((case.name.clone(), value));
                }
                self.classes[ci].backing = backing;
            } else if let Some(case) = decl.cases.first() {
                self.error("`case` can only be used in an enum", case.span);
            }
            for c in &decl.consts {
                self.annotations(&c.attrs, AnnTarget::Const);
                let Some(value) = self.literal_const(&c.value, c.ty.as_ref(), Some(cid)) else { continue };
                let own_dup = self.classes[ci].consts.contains_key(&c.name)
                    && decl.consts.iter().filter(|o| o.name == c.name).count() > 1;
                if own_dup {
                    self.error(format!("constant `{}` is declared more than once", c.name), c.name_span);
                }
                self.classes[ci].consts.insert(c.name.clone(), value);
            }
            self.type_params = class_tps.clone();
            for prop in &decl.props {
                let ann = self.annotations(&prop.attrs, AnnTarget::Property);
                if let Some(r) = ann.deprecated {
                    self.deprecated_fields.insert((cid, prop.name.clone()), r);
                }
                let ty = self.resolve_type(&prop.ty, Some(cid));
                let default = prop.default.as_ref().and_then(|d| self.const_default(d, &ty));
                let required = !ty.is_nullable() && default.is_none() && ty != Type::Error;
                self.add_field(
                    cid,
                    FieldInfo {
                        name: prop.name.clone(),
                        ty,
                        vis: prop.visibility,
                        readonly: prop.readonly,
                        owner: cid,
                        default,
                        required,
                        span: prop.span,
                    },
                );
            }
            let mut own_ctor = false;
            for m in &decl.methods {
                let type_params: Vec<String> = m.type_params.iter().map(|(n, _)| n.clone()).collect();
                for (n, s) in &m.type_params {
                    if class_tps.contains(n) {
                        self.error(format!("type parameter `{n}` is already a type parameter of `{class_name}`"), *s);
                    }
                }
                // Static methods do not belong to an instance, so the class's parameters are not in scope.
                self.type_params = if m.is_static && m.name != "__construct" { Vec::new() } else { class_tps.clone() };
                self.type_params.extend(type_params.iter().cloned());
                let params = self.resolve_params(&m.params, Some(cid));
                let is_ctor = m.name == "__construct";
                let ret = match &m.ret {
                    Some(t) => self.resolve_type(t, Some(cid)),
                    None => Type::Void,
                };
                self.type_params.clear();
                if type_params.len() > 4 {
                    self.error("at most 4 type parameters are supported", m.name_span);
                }
                if is_ctor && matches!(kind, ClassKind::Interface | ClassKind::Enum) {
                    self.error("interfaces and enums cannot have constructors", m.name_span);
                }
                if m.is_abstract && matches!(kind, ClassKind::Class | ClassKind::Enum) {
                    self.error(
                        format!("`{class_name}` must be declared `abstract` to have abstract methods"),
                        m.name_span,
                    );
                }
                if m.is_static && kind == ClassKind::Interface {
                    self.error("interface methods cannot be static", m.name_span);
                }
                if is_ctor && ret != Type::Void {
                    self.error("constructors cannot declare a return type", m.name_span);
                }
                if is_ctor && m.is_static {
                    self.error("constructors cannot be static", m.name_span);
                }
                let fid = self.add_func(FuncInfo {
                    name: format!("{class_name}::{}", m.name),
                    type_params,
                    is_abstract: m.is_abstract,
                    params: params.clone(),
                    ret: ret.clone(),
                    body: Body::Function(m),
                    module: self.module,
                    class: Some(cid),
                    // A `static` constructor was reported above; its body is checked as a constructor.
                    is_static: m.is_static && !is_ctor,
                    kind: if is_ctor { FuncKind::Ctor } else { FuncKind::Method },
                    span: m.name_span,
                });
                let ann = self.annotations(&m.attrs, AnnTarget::Method);
                if let Some(r) = ann.deprecated {
                    self.deprecated_funcs.insert(fid, r);
                }
                if ann.is_override {
                    let overrides = !is_ctor
                        && self.classes[ci]
                            .methods
                            .get(&m.name)
                            .is_some_and(|old| old.owner != cid && old.vis != Visibility::Private);
                    if !overrides {
                        self.error(
                            format!("`{class_name}::{}` is marked `#[Override]` but overrides nothing", m.name),
                            m.name_span,
                        )
                        .add_note("no parent class or interface declares a method with this name");
                    }
                }
                if is_ctor {
                    if let Some(old) = self.classes[ci].ctor.filter(|f| self.func_is_final(*f)) {
                        let parent_name = self.funcs[old as usize].name.clone();
                        self.error(
                            format!("`{class_name}::__construct` overrides `{parent_name}`, which is `final`"),
                            m.name_span,
                        );
                    }
                    own_ctor = true;
                    for (p, ap) in params.iter().zip(&m.params) {
                        if let Some((vis, readonly)) = p.promote {
                            self.add_field(
                                cid,
                                FieldInfo {
                                    name: p.name.clone(),
                                    ty: p.ty.clone(),
                                    vis,
                                    readonly,
                                    owner: cid,
                                    default: None,
                                    required: !p.ty.is_nullable() && p.ty != Type::Error,
                                    span: ap.span,
                                },
                            );
                        }
                    }
                    self.classes[ci].ctor = Some(fid);
                    self.classes[ci].ctor_access = (m.visibility, cid);
                    continue;
                }
                for (p, ap) in params.iter().zip(&m.params) {
                    if p.promote.is_some() {
                        self.error("only constructor parameters can be promoted to properties", ap.span);
                    }
                }
                let existing = self.classes[ci].methods.get(&m.name).cloned();
                let slot = match existing {
                    Some(old) if old.owner == cid => {
                        self.error(format!("method `{}` is declared more than once", m.name), m.name_span);
                        continue;
                    }
                    Some(old) if old.vis != Visibility::Private => {
                        if self.func_is_final(old.func) {
                            let parent_name = self.funcs[old.func as usize].name.clone();
                            self.error(
                                format!("`{class_name}::{}` overrides `{parent_name}`, which is `final`", m.name),
                                m.name_span,
                            );
                        }
                        if old.is_static != m.is_static {
                            self.error(
                                format!("`{}` changes between static and non-static when overriding", m.name),
                                m.name_span,
                            );
                        }
                        if !self.compatible_override(fid, old.func, cid) {
                            let parent_name = self.funcs[old.func as usize].name.clone();
                            self.error(
                                format!(
                                    "`{class_name}::{}` overrides `{parent_name}` with an incompatible signature",
                                    m.name
                                ),
                                m.name_span,
                            )
                            .add_note(
                                "overriding methods must keep the same parameter types and a compatible return type",
                            );
                        }
                        old.slot
                    }
                    _ => None,
                };
                let slot = if m.is_static {
                    None
                } else {
                    match slot {
                        Some(s) => {
                            self.classes[ci].vtable[s as usize] = fid;
                            Some(s)
                        }
                        None => {
                            let c = &mut self.classes[ci];
                            c.vtable.push(fid);
                            Some(c.vtable.len() as u32 - 1)
                        }
                    }
                };
                self.classes[ci].methods.insert(
                    m.name.clone(),
                    MethodInfo { func: fid, slot, vis: m.visibility, is_static: m.is_static, owner: cid },
                );
            }
            if matches!(kind, ClassKind::Class | ClassKind::Enum) {
                self.check_implemented(cid);
            }
            if !own_ctor {
                let missing: Vec<String> = self.classes[ci]
                    .fields
                    .iter()
                    .filter(|f| f.required && f.owner == cid)
                    .map(|f| format!("${}", f.name))
                    .collect();
                if !missing.is_empty() {
                    let name = self.classes[ci].name.clone();
                    let span = self.classes[ci].span;
                    self.error(
                        format!("class `{name}` has properties that are never initialized: {}", missing.join(", ")),
                        span,
                    )
                    .add_note("give them a default value, make them nullable, or assign them in `__construct`");
                }
            }
        }
    }

    /// A concrete class must implement every abstract and interface method.
    fn check_implemented(&mut self, cid: ClassId) {
        let ci = cid as usize;
        let span = self.classes[ci].span;
        let name = self.classes[ci].name.clone();
        let mut missing = Vec::new();
        let mut methods: Vec<(String, MethodInfo)> =
            self.classes[ci].methods.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        methods.sort_by(|a, b| a.0.cmp(&b.0));
        for (mname, m) in &methods {
            if !m.is_static && self.funcs[m.func as usize].is_abstract {
                missing.push(self.funcs[m.func as usize].name.clone());
            }
            let _ = mname;
        }
        let mut supers: Vec<ClassId> = self.classes[ci].supers.iter().copied().collect();
        supers.sort();
        for s in supers {
            if self.classes[s as usize].kind != ClassKind::Interface {
                continue;
            }
            let mut imethods: Vec<(String, MethodInfo)> =
                self.classes[s as usize].methods.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            imethods.sort_by(|a, b| a.0.cmp(&b.0));
            for (mname, im) in imethods {
                match self.classes[ci].methods.get(&mname).cloned() {
                    Some(cm) if cm.func == im.func => missing.push(self.funcs[im.func as usize].name.clone()),
                    Some(cm) => {
                        if cm.vis != Visibility::Public {
                            self.error(format!("`{name}::{mname}` must be public to implement an interface"), span);
                        } else if !self.compatible_override(cm.func, im.func, cid) {
                            let iname = self.funcs[im.func as usize].name.clone();
                            self.error(format!("`{name}::{mname}` does not match the signature of `{iname}`"), span);
                        }
                    }
                    None => missing.push(self.funcs[im.func as usize].name.clone()),
                }
            }
        }
        missing.sort();
        missing.dedup();
        if !missing.is_empty() {
            self.error(format!("class `{name}` does not implement {}", missing.join(", ")), span)
                .add_note("implement the methods, or declare the class `abstract`");
        }
    }

    fn add_field(&mut self, cid: ClassId, field: FieldInfo) {
        let span = field.span;
        let c = &mut self.classes[cid as usize];
        if let Some(&slot) = c.field_slots.get(&field.name) {
            let inherited = c.fields[slot as usize].owner != cid;
            let msg = if inherited {
                format!("property `${}` is already declared in a parent class", field.name)
            } else {
                format!("property `${}` is declared more than once", field.name)
            };
            self.error(msg, span);
            return;
        }
        if c.fields.len() >= MAX_FIELDS {
            if c.fields.len() == MAX_FIELDS {
                // Reported once; the class is not given more fields.
                let name = c.name.clone();
                c.fields.push(field);
                self.error(
                    format!("class `{name}` has too many properties (at most {MAX_FIELDS}, inherited ones included)"),
                    span,
                );
            }
            return;
        }
        c.field_slots.insert(field.name.clone(), c.fields.len() as u32);
        c.fields.push(field);
    }

    // ----- phase 5: signatures -------------------------------------------

    fn resolve_functions(&mut self) {
        for fid in 0..self.funcs.len() {
            let info = &self.funcs[fid];
            self.module = info.module;
            match (info.kind, info.body) {
                (FuncKind::Normal, Body::Function(d)) => {
                    self.type_params = info.type_params.clone();
                    if self.type_params.len() > 4 {
                        self.error("at most 4 type parameters are supported", d.name_span);
                    }
                    let params = self.resolve_params(&d.params, None);
                    let ret = match &d.ret {
                        Some(t) => self.resolve_type(t, None),
                        None => Type::Void,
                    };
                    self.type_params.clear();
                    let f = &mut self.funcs[fid];
                    f.params = params;
                    f.ret = ret;
                }
                (FuncKind::Template, Body::Template(m)) => {
                    let decls: Vec<&Vec<ast::Param>> = m
                        .stmts
                        .iter()
                        .filter_map(|s| match &s.kind {
                            StmtKind::Params(p) => Some(p),
                            _ => None,
                        })
                        .collect();
                    if decls.len() > 1 {
                        let span =
                            m.stmts.iter().filter(|s| matches!(s.kind, StmtKind::Params(_))).nth(1).unwrap().span;
                        self.error("a template can only have one `params(...)` declaration", span);
                    }
                    if let Some(p) = decls.first() {
                        let params = self.resolve_params(p, None);
                        self.funcs[fid].params = params;
                    }
                }
                _ => {}
            }
        }
    }

    // ----- phase 6: bodies -----------------------------------------------

    fn check_bodies(&mut self) {
        let mut fid = 0;
        while fid < self.funcs.len() {
            if !matches!(self.funcs[fid].body, Body::Done) {
                self.check_func(fid as FuncId);
            }
            fid += 1;
        }
    }

    fn check_func(&mut self, fid: FuncId) {
        let info = &self.funcs[fid as usize];
        self.module = info.module;
        let (body, kind, class, is_static, ret) = (info.body, info.kind, info.class, info.is_static, info.ret.clone());
        let params = info.params.clone();
        let file = self.input.modules[info.module].ast.file;
        let span = info.span;
        let name = info.name.clone();
        let is_abstract = info.is_abstract;
        self.type_params = info.type_params.clone();
        if let (Some(c), false) = (class, is_static) {
            let mut tps = self.classes[c as usize].type_params.clone();
            tps.append(&mut self.type_params);
            self.type_params = tps;
        }

        let mut ctx = FnCtx::new(ret.clone(), class, kind);
        let mut param_ids = Vec::new();
        let mut this = None;
        if matches!(kind, FuncKind::Method | FuncKind::Ctor) && !is_static {
            let id = ctx.declare("this", self.own_type(class.unwrap()), span);
            ctx.flow.mark(id);
            param_ids.push(id);
            this = Some(id);
        }
        for p in &params {
            let id = ctx.declare(&p.name, p.ty.clone(), p.span);
            ctx.flow.mark(id);
            param_ids.push(id);
        }
        self.ctx.push(ctx);

        let mut out = Vec::new();
        match body {
            Body::Function(_) if is_abstract => {}
            Body::Function(d) => {
                if kind == FuncKind::Ctor {
                    let this = this.unwrap();
                    for (i, p) in params.iter().enumerate() {
                        if p.promote.is_some() {
                            let slot = self.field_slot(class.unwrap(), &p.name).unwrap_or(0);
                            let this_ty = self.own_type(class.unwrap());
                            let place = HPlace {
                                base: HPlaceBase::Prop(
                                    Box::new(HExpr { kind: HExprKind::Local(this), ty: this_ty, span }),
                                    slot,
                                ),
                                path: Vec::new(),
                            };
                            let value = HExpr { kind: HExprKind::Local(param_ids[i + 1]), ty: p.ty.clone(), span };
                            out.push(HStmt::Assign(place, value));
                        }
                    }
                    self.check_ctor_init(class.unwrap(), d);
                }
                self.block(&d.body, &mut out);
            }
            Body::Template(m) | Body::Main(m) => {
                let mut warned = false;
                for s in &m.stmts {
                    match &s.kind {
                        StmtKind::Function(_)
                        | StmtKind::Class(_)
                        | StmtKind::Import(_)
                        | StmtKind::Params(_)
                        | StmtKind::Const(_) => {}
                        _ => {
                            self.note_unreachable(s, &mut warned);
                            self.stmt(s, &mut out);
                        }
                    }
                }
            }
            Body::Done => {}
        }

        let ctx = self.ctx.pop().unwrap();
        // Only plain functions: the parameters of methods, closures and templates
        // are fixed by what overrides, calls or renders them.
        let skip = if kind == FuncKind::Normal { &[][..] } else { &param_ids[..] };
        self.warn_unused_locals(&ctx, skip);
        let code_span = match body {
            Body::Function(d) => d.span,
            _ => Span::new(file, 0, u32::MAX),
        };
        self.note_scope(code_span, &ctx);
        self.type_params.clear();
        if !ctx.flow.dead && !matches!(ret, Type::Void | Type::Error) && kind != FuncKind::Template && !is_abstract {
            let msg = format!("`{name}` must return a value of type `{}` on every path", self.tn(&ret));
            self.error(msg, span);
        }
        self.hfuncs[fid as usize] = Some(HFunc {
            name,
            file,
            span,
            kind,
            params: param_ids,
            captures: Vec::new(),
            locals: ctx.locals,
            body: out,
            ret,
        });
        self.funcs[fid as usize].body = Body::Done;
    }

    /// Checks that a constructor initializes the fields that need it.
    fn check_ctor_init(&mut self, cid: ClassId, d: &FunctionDecl) {
        let assigned: Vec<&str> = d
            .body
            .iter()
            .filter_map(|s| match &s.kind {
                StmtKind::Assign { target, op: ast::AssignOp::Set, .. } => match &target.kind {
                    ExprKind::Prop { obj, name, .. } if matches!(&obj.kind, ExprKind::Var(v) if v == "this") => {
                        Some(name.as_str())
                    }
                    _ => None,
                },
                _ => None,
            })
            .collect();
        let calls_parent = d.body.iter().any(|s| {
            matches!(&s.kind, StmtKind::Expr(e) if matches!(&e.kind,
                ExprKind::StaticCall { class, name, .. } if class == "parent" && name == "__construct"))
        });
        let promoted: Vec<&str> = d.params.iter().filter(|p| p.promote.is_some()).map(|p| p.name.as_str()).collect();
        let class = &self.classes[cid as usize];
        let missing: Vec<String> = class
            .fields
            .iter()
            .filter(|f| {
                f.required
                    && f.owner == cid
                    && !assigned.contains(&f.name.as_str())
                    && !promoted.contains(&f.name.as_str())
            })
            .map(|f| format!("${}", f.name))
            .collect();
        let parent_needs_init = class.fields.iter().any(|f| f.owner != cid && f.required);
        let name = class.name.clone();
        if !missing.is_empty() {
            self.error(format!("the constructor of `{name}` does not initialize {}", missing.join(", ")), d.name_span)
                .add_note("assign every non-nullable property without a default at the top of `__construct`");
        }
        if parent_needs_init && !calls_parent {
            self.error(format!("the constructor of `{name}` must call `parent::__construct(...)`"), d.name_span)
                .add_note("the parent class has properties that its constructor initializes");
        }
    }

    pub(crate) fn field_slot(&self, cid: ClassId, name: &str) -> Option<u32> {
        let c = &self.classes[cid as usize];
        c.field_slots.get(name).copied()
    }

    fn finish(mut self) -> (HProgram, Vec<Diagnostic>) {
        let classes = self
            .classes
            .iter()
            .map(|c| HClass {
                name: c.name.clone(),
                parent: c.parent,
                fields: c
                    .fields
                    .iter()
                    .map(|f| HField {
                        name: f.name.clone(),
                        default: f.default.clone().unwrap_or(HConst::Null),
                        public: f.vis == Visibility::Public,
                    })
                    .collect(),
                vtable: c.vtable.clone(),
                supers: {
                    let mut v: Vec<ClassId> = c.supers.iter().copied().collect();
                    v.sort();
                    v
                },
                by_name: {
                    let mut v: Vec<(String, FuncId)> =
                        c.methods.iter().filter(|(_, m)| !m.is_static).map(|(n, m)| (n.clone(), m.func)).collect();
                    v.sort();
                    v
                },
                cases: c.cases.clone(),
            })
            .collect();
        let mut exc_classes = HashMap::new();
        if let Some(p) = self.prelude() {
            for (name, id) in &self.scopes[p].classes {
                exc_classes.insert(name.clone(), *id);
            }
        }
        let funcs = self
            .hfuncs
            .into_iter()
            .map(|f| {
                f.unwrap_or_else(|| HFunc {
                    name: "{unchecked}".into(),
                    file: 0,
                    span: Span::default(),
                    kind: FuncKind::Normal,
                    params: Vec::new(),
                    captures: Vec::new(),
                    locals: Vec::new(),
                    body: Vec::new(),
                    ret: Type::Void,
                })
            })
            .collect();
        let tests = std::mem::take(&mut self.tests);
        let program = HProgram { funcs, classes, main: self.main.unwrap_or(0), exc_classes, tests };
        (program, self.diags)
    }
}

/// Replaces the type parameters `names` (as `Type::Param`) in `t` by `args`.
pub(crate) fn subst_params(t: &Type, names: &[String], args: &[Type]) -> Type {
    if names.is_empty() {
        return t.clone();
    }
    match t {
        Type::Param(n) => match names.iter().position(|p| p == n) {
            Some(i) => args.get(i).cloned().unwrap_or(Type::Error),
            None => t.clone(),
        },
        Type::List(x) => Type::list(subst_params(x, names, args)),
        Type::Map(k, v) => Type::map(subst_params(k, names, args), subst_params(v, names, args)),
        Type::Func(ps, r) => Type::Func(
            ps.iter().map(|p| subst_params(p, names, args)).collect(),
            Box::new(subst_params(r, names, args)),
        ),
        Type::Union(ms) => Type::union(ms.iter().map(|m| subst_params(m, names, args)).collect()),
        Type::Class(c, xs) => Type::Class(*c, xs.iter().map(|x| subst_params(x, names, args)).collect()),
        other => other.clone(),
    }
}
