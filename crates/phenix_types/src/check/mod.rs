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
use crate::types::*;
use flow::FnCtx;
use phenix_syntax::ast::{self, ClassDecl, ClassKind, ExprKind, FunctionDecl, StmtKind, TypeExpr, TypeExprKind, Visibility};
use phenix_syntax::{parse_type_str, Diagnostic, Span};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ModuleKind {
    /// Built-in declarations visible everywhere (exception classes).
    Prelude,
    /// `std.*` modules.
    Std,
    /// A module imported by path.
    User,
    /// A `.phx.html` template used through `render(...)`.
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
    c.collect();
    c.resolve_imports();
    let order = c.order_classes();
    c.resolve_classes(&order);
    c.resolve_functions();
    c.check_bodies();
    c.finish()
}

#[derive(Default)]
struct Scope {
    funcs: HashMap<String, FuncId>,
    classes: HashMap<String, ClassId>,
    consts: HashMap<String, (HConst, Type)>,
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
    parent: Option<ClassId>,
    /// Directly implemented (or, for interfaces, extended) interfaces.
    ifaces: Vec<ClassId>,
    /// Itself, ancestors and all interfaces; filled when the class is resolved.
    supers: HashSet<ClassId>,
    fields: Vec<FieldInfo>,
    methods: HashMap<String, MethodInfo>,
    vtable: Vec<FuncId>,
    ctor: Option<FuncId>,
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
}

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
        }
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
            return Some(*c);
        }
        self.prelude().and_then(|p| self.scopes[p].classes.get(name).copied())
    }

    pub(crate) fn lookup_func(&self, name: &str) -> Option<FuncId> {
        if let Some(f) = self.scopes[self.module].funcs.get(name) {
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
            return Some(c.clone());
        }
        self.prelude().and_then(|p| self.scopes[p].consts.get(name).cloned())
    }

    pub(crate) fn class_kind(&self, c: ClassId) -> ClassKind {
        self.classes[c as usize].kind
    }

    pub(crate) fn func_is_abstract(&self, f: FuncId) -> bool {
        self.funcs[f as usize].is_abstract
    }

    /// The value of a `const` declaration: a scalar literal.
    fn literal_const(&mut self, e: &ast::Expr, ty: Option<&TypeExpr>, self_class: Option<ClassId>) -> Option<(HConst, Type)> {
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

    /// For a union made only of classes, their common ancestor as a class type.
    pub(crate) fn as_single_class(&self, t: &Type) -> Option<Type> {
        let Type::Union(ms) = t else { return None };
        let ids: Option<Vec<ClassId>> = ms.iter().map(|m| if let Type::Class(c) = m { Some(*c) } else { None }).collect();
        self.common_ancestor(&ids?).map(Type::Class)
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
                        Some(c) => simple(Type::Class(c)),
                        None => {
                            self.error("`self` can only be used inside a class", t.span);
                            return Type::Error;
                        }
                    },
                    "mixed" => {
                        self.error("`mixed` does not exist in Phenix", t.span)
                            .add_note("use `any` and narrow it with `is` before use");
                        return Type::Error;
                    }
                    "array" => {
                        self.error("`array` does not exist in Phenix", t.span)
                            .add_note("use `list<T>` for sequences or `map<K, V>` for key/value data");
                        return Type::Error;
                    }
                    "callable" | "Closure" => {
                        self.error(format!("`{n}` does not exist in Phenix"), t.span)
                            .add_note("use a function type such as `fn(int): string`");
                        return Type::Error;
                    }
                    other => match self.lookup_class(other) {
                        Some(c) => simple(Type::Class(c)),
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
            if out.iter().any(|o| o.name == p.name) {
                self.error(format!("duplicate parameter `${}`", p.name), p.span);
            }
            out.push(ParamInfo { name: p.name.clone(), ty, default, promote: p.promote });
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
                            self.error("templates cannot declare functions; put them in a module and import it", d.name_span);
                            continue;
                        }
                        if self.is_builtin_name(&d.name) {
                            self.error(format!("`{}` is a built-in function and cannot be redefined", d.name), d.name_span);
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
                            self.error("templates cannot declare classes; put them in a module and import it", d.name_span);
                            continue;
                        }
                        let cid = self.classes.len() as ClassId;
                        self.classes.push(ClassInfo {
                            name: d.name.clone(),
                            decl: Some(d),
                            kind: d.kind,
                            module: mi,
                            parent: None,
                            ifaces: Vec::new(),
                            supers: HashSet::new(),
                            fields: Vec::new(),
                            methods: HashMap::new(),
                            vtable: Vec::new(),
                            ctor: None,
                            consts: HashMap::new(),
                            cases: Vec::new(),
                            backing: None,
                            span: d.name_span,
                        });
                        self.class_names.push(d.name.clone());
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
                        let Some(value) = self.literal_const(&d.value, d.ty.as_ref(), None) else { continue };
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
                for (name, span) in &d.names {
                    let f = self.scopes[target].export_funcs.get(name).copied();
                    let c = self.scopes[target].export_classes.get(name).copied();
                    if let Some(k) = self.scopes[target].export_consts.get(name).cloned() {
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
                match self.lookup_class(pname) {
                    Some(p) => self.classes[cid].parent = Some(p),
                    None => {
                        self.error(format!("unknown parent class `{pname}`"), *pspan);
                    }
                }
            }
            for (iname, ispan) in &decl.interfaces {
                match self.lookup_class(iname) {
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

    /// Whether method `sub` can stand in for `base` (same parameters, compatible return).
    fn compatible_override(&self, sub: FuncId, base: FuncId) -> bool {
        let (a, b) = (&self.funcs[sub as usize], &self.funcs[base as usize]);
        a.params.len() == b.params.len()
            && a.params.iter().zip(&b.params).all(|(x, y)| x.ty == y.ty)
            && self.assignable(&a.ret, &b.ret)
    }

    fn resolve_classes(&mut self, order: &[ClassId]) {
        for &cid in order {
            let ci = cid as usize;
            let Some(decl) = self.classes[ci].decl else { continue };
            self.module = self.classes[ci].module;
            let kind = decl.kind;
            let class_name = self.classes[ci].name.clone();
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
                let parent = &self.classes[p as usize];
                let (fields, methods, vtable, ctor, consts) = (
                    parent.fields.clone(),
                    parent.methods.clone(),
                    parent.vtable.clone(),
                    parent.ctor,
                    parent.consts.clone(),
                );
                let c = &mut self.classes[ci];
                c.fields = fields;
                c.methods = methods;
                c.vtable = vtable;
                c.ctor = ctor;
                c.consts = consts;
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
                if kind == ClassKind::Interface {
                    for (name, m) in self.classes[i as usize].methods.clone() {
                        self.classes[ci].methods.entry(name).or_insert(m);
                    }
                }
            }
            // Classes get a vtable slot for every interface method they do not
            // define yet, so abstract classes can call them on `$this`.
            if kind != ClassKind::Interface {
                let mut ifs: Vec<ClassId> = supers
                    .iter()
                    .copied()
                    .filter(|s| self.classes[*s as usize].kind == ClassKind::Interface)
                    .collect();
                ifs.sort();
                for i in ifs {
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
                };
                self.classes[ci].fields.push(name_field);
                if let Some(b) = &backing {
                    let value_field = FieldInfo {
                        name: "value".into(),
                        ty: b.clone(),
                        vis: Visibility::Public,
                        readonly: true,
                        owner: cid,
                        default: Some(HConst::Null),
                        required: false,
                    };
                    self.classes[ci].fields.push(value_field);
                }
                for case in &decl.cases {
                    let value = match (&backing, &case.value) {
                        (Some(b), Some(v)) => self.const_default(v, &b.clone()).unwrap_or(HConst::Null),
                        (Some(_), None) => {
                            self.error(format!("case `{}` of a backed enum needs a value", case.name), case.span);
                            HConst::Null
                        }
                        (None, Some(v)) => {
                            self.error("cases of a pure enum cannot have values; add a backing type, e.g. `enum E: string`", v.span);
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
                let Some(value) = self.literal_const(&c.value, c.ty.as_ref(), Some(cid)) else { continue };
                let own_dup = self.classes[ci].consts.contains_key(&c.name)
                    && decl.consts.iter().filter(|o| o.name == c.name).count() > 1;
                if own_dup {
                    self.error(format!("constant `{}` is declared more than once", c.name), c.name_span);
                }
                self.classes[ci].consts.insert(c.name.clone(), value);
            }
            for prop in &decl.props {
                let ty = self.resolve_type(&prop.ty, Some(cid));
                let default = prop.default.as_ref().and_then(|d| self.const_default(d, &ty));
                let required = !ty.is_nullable() && default.is_none() && ty != Type::Error;
                self.add_field(cid, FieldInfo {
                    name: prop.name.clone(),
                    ty,
                    vis: prop.visibility,
                    readonly: prop.readonly,
                    owner: cid,
                    default,
                    required,
                }, prop.span);
            }
            let mut own_ctor = false;
            for m in &decl.methods {
                self.type_params = m.type_params.iter().map(|(n, _)| n.clone()).collect();
                let params = self.resolve_params(&m.params, Some(cid));
                let is_ctor = m.name == "__construct";
                let ret = match &m.ret {
                    Some(t) => self.resolve_type(t, Some(cid)),
                    None => Type::Void,
                };
                let type_params = std::mem::take(&mut self.type_params);
                if type_params.len() > 4 {
                    self.error("at most 4 type parameters are supported", m.name_span);
                }
                if is_ctor && matches!(kind, ClassKind::Interface | ClassKind::Enum) {
                    self.error("interfaces and enums cannot have constructors", m.name_span);
                }
                if m.is_abstract && matches!(kind, ClassKind::Class | ClassKind::Enum) {
                    self.error(format!("`{class_name}` must be declared `abstract` to have abstract methods"), m.name_span);
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
                    is_static: m.is_static,
                    kind: if is_ctor { FuncKind::Ctor } else { FuncKind::Method },
                    span: m.name_span,
                });
                if is_ctor {
                    own_ctor = true;
                    for (p, ap) in params.iter().zip(&m.params) {
                        if let Some((vis, readonly)) = p.promote {
                            self.add_field(cid, FieldInfo {
                                name: p.name.clone(),
                                ty: p.ty.clone(),
                                vis,
                                readonly,
                                owner: cid,
                                default: None,
                                required: !p.ty.is_nullable() && p.ty != Type::Error,
                            }, ap.span);
                        }
                    }
                    self.classes[ci].ctor = Some(fid);
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
                        if old.is_static != m.is_static {
                            self.error(format!("`{}` changes between static and non-static when overriding", m.name), m.name_span);
                        }
                        if !self.compatible_override(fid, old.func) {
                            let parent_name = self.funcs[old.func as usize].name.clone();
                            self.error(format!("`{class_name}::{}` overrides `{parent_name}` with an incompatible signature", m.name), m.name_span)
                                .add_note("overriding methods must keep the same parameter types and a compatible return type");
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
                        } else if !self.compatible_override(cm.func, im.func) {
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

    fn add_field(&mut self, cid: ClassId, field: FieldInfo, span: Span) {
        let c = &mut self.classes[cid as usize];
        if let Some(existing) = c.fields.iter().find(|f| f.name == field.name) {
            let inherited = existing.owner != cid;
            let msg = if inherited {
                format!("property `${}` is already declared in a parent class", field.name)
            } else {
                format!("property `${}` is declared more than once", field.name)
            };
            self.error(msg, span);
            return;
        }
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
                        let span = m.stmts.iter().filter(|s| matches!(s.kind, StmtKind::Params(_))).nth(1).unwrap().span;
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

        let mut ctx = FnCtx::new(ret.clone(), class, kind);
        let mut param_ids = Vec::new();
        let mut this = None;
        if matches!(kind, FuncKind::Method | FuncKind::Ctor) && !is_static {
            let id = ctx.declare("this", Type::Class(class.unwrap()));
            ctx.flow.mark(id);
            param_ids.push(id);
            this = Some(id);
        }
        for p in &params {
            let id = ctx.declare(&p.name, p.ty.clone());
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
                            let this_ty = Type::Class(class.unwrap());
                            let place = HPlace {
                                base: HPlaceBase::Prop(Box::new(HExpr { kind: HExprKind::Local(this), ty: this_ty, span }), slot),
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
                for s in &m.stmts {
                    match &s.kind {
                        StmtKind::Function(_)
                        | StmtKind::Class(_)
                        | StmtKind::Import(_)
                        | StmtKind::Params(_)
                        | StmtKind::Const(_) => {}
                        _ => self.stmt(s, &mut out),
                    }
                }
            }
            Body::Done => {}
        }

        let ctx = self.ctx.pop().unwrap();
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
        let promoted: Vec<&str> =
            d.params.iter().filter(|p| p.promote.is_some()).map(|p| p.name.as_str()).collect();
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
            self.error(
                format!("the constructor of `{name}` does not initialize {}", missing.join(", ")),
                d.name_span,
            )
            .add_note("assign every non-nullable property without a default at the top of `__construct`");
        }
        if parent_needs_init && !calls_parent {
            self.error(format!("the constructor of `{name}` must call `parent::__construct(...)`"), d.name_span)
                .add_note("the parent class has properties that its constructor initializes");
        }
    }

    pub(crate) fn field_slot(&self, cid: ClassId, name: &str) -> Option<u32> {
        self.classes[cid as usize].fields.iter().position(|f| f.name == name).map(|i| i as u32)
    }

    fn finish(self) -> (HProgram, Vec<Diagnostic>) {
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
                })
            })
            .collect();
        let program = HProgram { funcs, classes, main: self.main.unwrap_or(0), exc_classes };
        (program, self.diags)
    }
}
