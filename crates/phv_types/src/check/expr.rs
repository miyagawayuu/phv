use super::flow::{FactKey, Facts, FnCtx};
use super::{Body, Checker, FieldInfo, FuncInfo, ParamInfo};
use crate::builtins::{php_hint, Recv};
use crate::hir::*;
use crate::types::*;
use phv_syntax::ast::{
    self, Arg, BinOp, CastKind, ClassKind, ClosureBody, Expr, ExprKind, InterpPart, MatchArm, TypeExpr, TypeExprKind,
    UnOp, Visibility,
};
use phv_syntax::{normalize_template_path, Span};
use std::collections::HashSet;

/// Inferred type parameters: a generic class's (when it is created) and then
/// the function's own, at most 4 each.
type Bindings = [Option<Type>; 8];

fn unify(param: &Type, actual: &Type, b: &mut Bindings) {
    match (param, actual) {
        // Indexes beyond the bindings (a function declared with too many type
        // parameters, already reported) are left unbound.
        (Type::Generic(i), t) => {
            let Some(slot) = b.get_mut(*i as usize) else { return };
            if slot.is_none() && !t.has_unknown() && *t != Type::Error && *t != Type::Never {
                *slot = Some(t.clone());
            }
        }
        (Type::List(p), Type::List(a)) | (Type::Set(p), Type::Set(a)) => unify(p, a, b),
        (Type::Map(pk, pv), Type::Map(ak, av)) => {
            unify(pk, ak, b);
            unify(pv, av, b);
        }
        (Type::Func(pp, pr), Type::Func(ap, ar)) => {
            for (p, a) in pp.iter().zip(ap) {
                unify(p, a, b);
            }
            unify(pr, ar, b);
        }
        (Type::Class(pc, pa), Type::Class(ac, aa)) if pc == ac => {
            for (p, a) in pa.iter().zip(aa) {
                unify(p, a, b);
            }
        }
        // `?T` (or `T|string`) given `?int`: the members that are not known
        // already decide the one unknown member.
        (Type::Union(pms), _) => {
            let unknown: Vec<&Type> = pms.iter().filter(|m| m.has_unknown()).collect();
            if let [u] = unknown[..] {
                let rest: Vec<Type> = actual.members().into_iter().filter(|m| !pms.contains(m)).collect();
                if !rest.is_empty() {
                    unify(u, &Type::union(rest), b);
                }
            }
        }
        _ => {}
    }
}

/// `t` with the inferred types put in; `Type::Error` (and noted, see
/// `note_too_large`) when that would be larger than `MAX_TYPE_SIZE`.
fn subst(t: &Type, b: &Bindings) -> Type {
    let sizes = b.each_ref().map(|x| x.as_ref().map_or(1, |t| super::type_size(t, super::MAX_TYPE_SIZE)));
    if sizes.iter().any(|&n| n > 1) {
        let size = super::replaced_size(t, |x| match x {
            Type::Generic(i) => sizes.get(*i as usize).copied(),
            _ => None,
        });
        if size > super::MAX_TYPE_SIZE {
            super::note_too_large();
            return Type::Error;
        }
    }
    subst_in(t, b)
}

fn subst_in(t: &Type, b: &Bindings) -> Type {
    match t {
        Type::Generic(i) => match b.get(*i as usize) {
            Some(bound) => bound.clone().unwrap_or(Type::Infer),
            None => Type::Error,
        },
        Type::List(x) => Type::list(subst_in(x, b)),
        Type::Set(x) => Type::set(subst_in(x, b)),
        Type::Map(k, v) => Type::map(subst_in(k, b), subst_in(v, b)),
        Type::Func(ps, r) => Type::Func(ps.iter().map(|p| subst_in(p, b)).collect(), Box::new(subst_in(r, b))),
        Type::Union(ms) => Type::union(ms.iter().map(|m| subst_in(m, b)).collect()),
        Type::Class(c, args) => Type::Class(*c, args.iter().map(|a| subst_in(a, b)).collect()),
        other => other.clone(),
    }
}

/// Whether `e` is a member/index chain containing `?->` (e.g. `$a?->b->c`).
fn in_nullsafe_chain(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::MethodCall { nullsafe: true, .. } | ExprKind::Prop { nullsafe: true, .. } => true,
        ExprKind::MethodCall { obj, .. } | ExprKind::Prop { obj, .. } => in_nullsafe_chain(obj),
        ExprKind::Index { base, .. } => in_nullsafe_chain(base),
        _ => false,
    }
}

/// Replaces type parameters (a function's own, and a class's while it is
/// created) with inference variables.
pub(super) fn instantiate(t: &Type, tps: &[Name]) -> Type {
    if !super::has_param(t) {
        return t.clone();
    }
    match t {
        Type::Param(n) => match tps.iter().position(|p| p == n) {
            Some(i) => Type::Generic(i as u8),
            None => t.clone(),
        },
        Type::List(x) => Type::list(instantiate(x, tps)),
        Type::Set(x) => Type::set(instantiate(x, tps)),
        Type::Map(k, v) => Type::map(instantiate(k, tps), instantiate(v, tps)),
        Type::Func(ps, r) => {
            Type::Func(ps.iter().map(|p| instantiate(p, tps)).collect(), Box::new(instantiate(r, tps)))
        }
        Type::Union(ms) => Type::union(ms.iter().map(|m| instantiate(m, tps)).collect()),
        Type::Class(c, args) => Type::Class(*c, args.iter().map(|a| instantiate(a, tps)).collect()),
        other => other.clone(),
    }
}

fn hconst(c: HConst, ty: Type, span: Span) -> HExpr {
    HExpr { kind: HExprKind::Const(c), ty, span }
}

fn err(span: Span) -> HExpr {
    HExpr { kind: HExprKind::Const(HConst::Null), ty: Type::Error, span }
}

/// Whether a type is, or holds, a closure: `fn(...)`, `?fn(...)`, a union with
/// one, a `list`/`map`/`set` of them, or a class with one as a type argument.
fn holds_func(t: &Type) -> bool {
    match t {
        Type::Func(..) => true,
        Type::List(t) | Type::Set(t) => holds_func(t),
        Type::Map(k, v) => holds_func(k) || holds_func(v),
        Type::Union(ms) => ms.iter().any(holds_func),
        Type::Class(_, args) => args.iter().any(holds_func),
        _ => false,
    }
}

/// Whether a built-in call with these parameters and arguments may run user
/// code: a closure among the arguments (the receiver included) can change any
/// property, so the property narrowings must be dropped after the call. Both
/// sides are looked at, so an overload that takes a closure clears even when
/// the argument's type was lost to an error. Every arm of `call_builtin` must
/// ask this (found by the 1.2.0 audit: the arm for several overloads of one
/// arity, `list->groupBy`, kept the narrowings).
fn runs_user_code(params: &[Type], args: &[HExpr]) -> bool {
    params.iter().any(holds_func) || args.iter().any(|h| holds_func(&h.ty))
}

impl<'a> Checker<'a> {
    // ----- assignability ---------------------------------------------------

    pub(crate) fn assignable(&self, from: &Type, to: &Type) -> bool {
        // A bounded `T` is its bound, with no conversion (a `T: int` is not widened to `float`).
        if let Type::Param(n) = from {
            if self.bounds.get(n).is_some_and(|b| from != to && self.assignable_exact(b, to)) {
                return true;
            }
        }
        self.assignable_inner(from, to, true)
    }

    pub(crate) fn assignable_exact(&self, from: &Type, to: &Type) -> bool {
        self.assignable_inner(from, to, false)
    }

    fn assignable_inner(&self, from: &Type, to: &Type, coerce: bool) -> bool {
        if from == to && !from.is_open() {
            return true;
        }
        match (from, to) {
            (Type::Error, _) | (_, Type::Error) | (Type::Never, _) => true,
            (Type::Void, _) | (_, Type::Void) => false,
            (_, Type::Any) => true,
            (Type::Union(ms), _) => ms.iter().all(|m| self.assignable_inner(m, to, false)),
            (_, Type::Union(ts)) => ts.iter().any(|t| self.assignable_inner(from, t, coerce)),
            (Type::Int, Type::Float) => coerce,
            // A checked URL is still HTML (not the other way round). A
            // `json_html()` value is not: its quotes would end an attribute.
            (Type::SafeUrl, Type::Html) => true,
            // Type arguments are invariant: `Box<int>` is not a `Box<any>`.
            (Type::Class(a, aa), Type::Class(b, ba)) => {
                ba.is_empty() && self.is_subclass(*a, *b)
                    || self
                        .upcast(*a, aa, *b)
                        .is_some_and(|up| up.len() == ba.len() && up.iter().zip(ba).all(|(x, y)| self.same(x, y)))
            }
            (Type::List(a), Type::List(b)) | (Type::Set(a), Type::Set(b)) => self.same(a, b),
            (Type::Map(k1, v1), Type::Map(k2, v2)) => self.same(k1, k2) && self.same(v1, v2),
            (Type::Func(p1, r1), Type::Func(p2, r2)) => {
                p1.len() == p2.len()
                    && p1.iter().zip(p2).all(|(a, b)| self.same(a, b))
                    && (self.same(r1, r2) || self.assignable_exact(r1, r2) || **r2 == Type::Void)
            }
            _ => false,
        }
    }

    fn same(&self, a: &Type, b: &Type) -> bool {
        (a == b && !a.is_open()) || *a == Type::Error || *b == Type::Error
    }

    /// Smallest type both `a` and `b` fit in. With `widen`, `int` and `float` join to `float`.
    fn join(&self, a: &Type, b: &Type, widen: bool) -> Type {
        if a == b {
            return a.clone();
        }
        // A union of two types each as large as allowed (`$c ? fn() => $a : fn() => $b`
        // grows by their sum each time) is not made.
        let union = || {
            if super::type_size(a, super::MAX_TYPE_SIZE) + super::type_size(b, super::MAX_TYPE_SIZE)
                > super::MAX_TYPE_SIZE
            {
                super::note_too_large();
                return Type::Error;
            }
            Type::union(vec![a.clone(), b.clone()])
        };
        match (a, b) {
            (Type::Never, t) | (t, Type::Never) => t.clone(),
            (Type::Error, _) | (_, Type::Error) => Type::Error,
            (Type::Int, Type::Float) | (Type::Float, Type::Int) if widen => Type::Float,
            (Type::Class(..), Type::Class(..)) => {
                let u = union();
                match self.as_single_class(&u) {
                    Some(t) => t,
                    None => u,
                }
            }
            _ if self.assignable_exact(a, b) => b.clone(),
            _ if self.assignable_exact(b, a) => a.clone(),
            _ => union(),
        }
    }

    /// A map key; for `map<any, ...>` (from `is map`) it must still be an int or string.
    fn key_to(&mut self, k: &Expr, kt: &Type) -> HExpr {
        if *kt != Type::Any {
            return self.expr_to(k, kt);
        }
        let h = self.expr(k, None);
        if !matches!(h.ty, Type::Int | Type::Str | Type::Error) {
            let msg = format!("map keys must be `int` or `string`, found `{}`", self.tn(&h.ty));
            self.error(msg, h.span);
        }
        h
    }

    pub(crate) fn expr_to(&mut self, e: &Expr, target: &Type) -> HExpr {
        let h = self.expr(e, Some(target));
        self.coerce(h, target)
    }

    pub(crate) fn coerce(&mut self, h: HExpr, target: &Type) -> HExpr {
        if target.has_unknown() {
            return h;
        }
        if h.ty == Type::Int && target.contains(&Type::Float) && !target.contains(&Type::Int) {
            let span = h.span;
            return HExpr { kind: HExprKind::IntToFloat(Box::new(h)), ty: Type::Float, span };
        }
        if self.assignable(&h.ty, target) {
            return h;
        }
        let (found, expected) = (self.tn(&h.ty), self.tn(target));
        let nullable_mismatch = h.ty.is_nullable() && self.assignable(&h.ty.without_null(), target);
        let e = self.error(format!("expected `{expected}`, found `{found}`"), h.span);
        if nullable_mismatch {
            e.add_note("the value might be null: check `!= null` first, or give a fallback with `??`");
        } else if matches!((&h.ty, target), (Type::Str, Type::Int | Type::Float)) {
            e.add_note("strings are never converted to numbers implicitly; use `->toInt()` or `->toFloat()`");
        } else if matches!((&h.ty, target), (Type::Int | Type::Float | Type::Bool, Type::Str)) {
            e.add_note("use `(string)$value` or string interpolation");
        } else if matches!((&h.ty, target), (Type::Str, Type::Html)) {
            e.add_note("use `escape($s)` to make safe HTML, or `raw($s)` if it is trusted markup");
        } else if matches!((&h.ty, target), (Type::Float, Type::Int)) {
            e.add_note("use `(int)$value` to truncate, or `round()` first");
        } else if matches!((&h.ty, target), (Type::SafeJs, Type::Html)) {
            e.add_note(
                "a `json_html()` value is only for `<script>` (declare it `SafeJs`); in an attribute, use \
                 `escape(json_encode($value))`",
            );
        } else if *target == Type::Never || h.ty.is_open() || target.is_open() {
            e.add_note(
                "`is` and `catch` do not know a generic class's type arguments (`?`): nothing can be written \
                 through such a value, and it cannot be passed on as one with known type arguments",
            );
        }
        HExpr { ty: Type::Error, ..h }
    }

    pub(crate) fn widen_to_float(&mut self, h: HExpr) -> HExpr {
        if h.ty == Type::Int {
            let span = h.span;
            HExpr { kind: HExprKind::IntToFloat(Box::new(h)), ty: Type::Float, span }
        } else {
            h
        }
    }

    fn can_access(&self, owner: ClassId, vis: Visibility) -> bool {
        let cur = self.ctx.last().and_then(|c| c.class);
        match vis {
            Visibility::Public => true,
            Visibility::Private => cur == Some(owner),
            Visibility::Protected => cur.is_some_and(|c| self.is_subclass(c, owner) || self.is_subclass(owner, c)),
        }
    }

    fn vis_word(vis: Visibility) -> &'static str {
        match vis {
            Visibility::Public => "public",
            Visibility::Protected => "protected",
            Visibility::Private => "private",
        }
    }

    pub(crate) fn check_printable(&mut self, h: &HExpr, what: &str) {
        let printable =
            |t: &Type| t.is_html() || matches!(t, Type::Str | Type::Int | Type::Float | Type::Error | Type::Never);
        match &h.ty {
            t if t.members().iter().all(printable) && !t.members().is_empty() => {}
            Type::Never => {}
            t => {
                let msg = format!("cannot {what} a value of type `{}`", self.tn(t));
                let t = t.clone();
                let e = self.error(msg, h.span);
                if t == Type::Bool {
                    e.add_note(
                        "bools are not converted to strings implicitly; use a ternary, e.g. `$ok ? \"yes\" : \"no\"`",
                    );
                } else if t.is_nullable() {
                    e.add_note("the value might be null; give a fallback with `?? \"\"`");
                } else {
                    e.add_note("use `json_encode($value)` or `dump($value)`");
                }
            }
        }
    }

    pub(crate) fn check_stringable(&mut self, h: &HExpr, what: &str) {
        let ok = |t: &Type| matches!(t, Type::Str | Type::Int | Type::Float | Type::Error | Type::Never);
        match &h.ty {
            t if t.members().iter().all(ok) && !t.members().is_empty() => {}
            Type::Never => {}
            t => {
                let msg = format!("cannot {what} a value of type `{}` to a string", self.tn(t));
                let t = t.clone();
                let e = self.error(msg, h.span);
                match t {
                    t if t.is_html() => {
                        e.add_note("join two `Html` values with `.`, or use `(string)$html` to get the raw markup")
                    }
                    Type::Bool => e.add_note("use a ternary, e.g. `$ok ? \"yes\" : \"no\"`"),
                    t if t.is_nullable() => e.add_note("the value might be null; give a fallback with `?? \"\"`"),
                    _ => e.add_note("only `string`, `int` and `float` can be joined into strings"),
                };
            }
        }
    }

    /// HTML escaping is only safe in element text and quoted attribute values.
    pub(crate) fn check_html_context(&mut self, h: &HExpr, ctx: ast::EchoContext) {
        use ast::EchoContext::*;
        let numeric = h.ty.members().iter().all(|m| matches!(m, Type::Int | Type::Float | Type::Error | Type::Never));
        let (msg, note) = match ctx {
            Attr if h.ty == Type::SafeJs => (
                "`json_html()` is for `<script>`; its quotes would end the attribute",
                "in an attribute, write `escape(json_encode($value))` (e.g. in a `data-*` attribute)",
            ),
            // `Html` is checked when it is inserted (`html_inside`).
            Text | SelectText | Attr | ForeignText | RawText(_) | Comment { .. } => return,
            Unquoted => (
                "values in attributes must be quoted",
                "write `attr=\"<?= $value ?>\"`: escaping does not protect unquoted attribute values",
            ),
            Tag => (
                "values cannot be inserted between the attributes of a tag",
                "write each attribute as `name=\"<?= $value ?>\"`",
            ),
            AfterMarkup => (
                "a value cannot follow `<` (or `</`, `<!`, `-`) here: it would be read with it",
                "it could start a tag, comment or end tag; put a space between them (`a < <?= $n ?>`)",
            ),
            // Only values checked for this context: HTML escaping (`escape()`,
            // templates) does not stop `javascript:` URLs or script code.
            Url if numeric || h.ty == Type::SafeUrl => return,
            UrlReference if numeric => return,
            UrlReference => (
                "a value cannot follow `&` at the start of a URL attribute",
                "the browser would read `&` and the value as a character reference (`&` + `#x6a;` is `j`); remove the `&`, or pass the whole URL through `safe_url()`",
            ),
            Url => (
                "a URL attribute cannot start with an unchecked value",
                "use `safe_url($url)`, which only allows http(s), mailto, tel and relative URLs (blocks `javascript:`); `escape()` does not make a URL safe",
            ),
            Script if numeric || h.ty == Type::SafeJs => return,
            Script => (
                "only `json_html()` values and numbers can be inserted into `<script>`",
                "use `json_html($value)`, which produces a JavaScript value that is safe inside a script; `escape()` does not make code safe",
            ),
            ScriptLiteral if numeric => return,
            ScriptLiteral => (
                "values cannot be inserted inside a JavaScript string, template literal, comment or regular expression",
                "insert the value where an expression goes, outside the quotes: `const message = <?= json_html($value) ?>;`",
            ),
            Style if numeric => return,
            Style => ("values cannot be inserted into CSS", "use a class name, or a `data-*` attribute"),
            Handler if numeric => return,
            Handler => (
                "values cannot be inserted into event handler attributes",
                "put the value in a `data-*` attribute and read it with addEventListener",
            ),
            ScriptUrl if numeric => return,
            ScriptUrl => (
                "values cannot be inserted into a `javascript:` (or `data:`) URL",
                "put the value in a `data-*` attribute and read it from a script",
            ),
            Srcdoc => (
                "values cannot be inserted into `srcdoc`",
                "its value is parsed as a separate HTML document, where escaping does not apply; serve the content as its own page and use `src`",
            ),
            ScriptSrc => (
                "values cannot be inserted into the URL of a `<script>`",
                "the tag gets the CSP nonce, so any URL there runs as a trusted script; write the URL in the template, and pass data with `json_html()` or a `data-*` attribute",
            ),
            // A number (`content="10"`, seconds to refresh) cannot name a URL.
            MetaContent if numeric => return,
            MetaContent => (
                "values cannot be inserted into the `content` of this `<meta>`",
                "only a `<meta>` with `name`, `property` or `itemprop` (written before `content`) takes inserted text, and others a number; `http-equiv=\"refresh\"` would redirect to a URL",
            ),
        };
        self.error(msg, h.span).add_note(note);
    }

    pub(crate) fn throwable(&mut self, x: &Expr) -> HExpr {
        let h = self.expr(x, None);
        let exc = self.exception_class();
        match &h.ty {
            Type::Class(c, _) if exc.is_some_and(|e| self.is_subclass(*c, e)) => h,
            Type::Error | Type::Never => h,
            t => {
                let msg = format!("only exceptions can be thrown, not `{}`", self.tn(t));
                self.error(msg, h.span).add_note("throw an instance of `Exception` or a subclass of it");
                err(h.span)
            }
        }
    }

    // ----- conditions and narrowing ----------------------------------------

    fn require_bool(&mut self, h: HExpr) -> HExpr {
        if matches!(h.ty, Type::Bool | Type::Error | Type::Never) {
            return h;
        }
        let msg = format!("condition must be a `bool`, found `{}`", self.tn(&h.ty));
        let ty = h.ty.clone();
        let e = self.error(msg, h.span);
        let note = match ty {
            Type::Str => "PHV has no truthiness: write `$s != \"\"` or `!$s->isEmpty()`",
            Type::Int | Type::Float => "PHV has no truthiness: write `$n != 0`",
            Type::List(_) | Type::Map(..) | Type::Set(_) => "PHV has no truthiness: write `!$xs->isEmpty()`",
            _ => "PHV has no truthiness: compare explicitly, e.g. `$x != null`",
        };
        e.add_note(note);
        HExpr { ty: Type::Error, ..h }
    }

    /// Checks a boolean condition and returns the narrowings that hold when it
    /// is true and when it is false.
    pub(crate) fn cond(&mut self, e: &Expr) -> (HExpr, Facts, Facts) {
        let span = e.span;
        let bool_expr = |kind| HExpr { kind, ty: Type::Bool, span };
        match &e.kind {
            ExprKind::Unary(UnOp::Not, x) => {
                let (h, t, f) = self.cond(x);
                (bool_expr(HExprKind::Unary(UnKind::Not, Box::new(h))), f, t)
            }
            ExprKind::Binary(BinOp::And, a, b) => {
                let (ha, ta, _) = self.cond(a);
                let saved = self.cx().apply(&ta);
                let (hb, tb, _) = self.cond(b);
                let calls = self.cx().restore(saved);
                let mut t = ta;
                if calls {
                    t.retain(|(k, _)| !matches!(k, FactKey::Prop(..)));
                }
                t.extend(tb);
                (bool_expr(HExprKind::And(Box::new(ha), Box::new(hb))), t, Vec::new())
            }
            ExprKind::Binary(BinOp::Or, a, b) => {
                let (ha, _, fa) = self.cond(a);
                let saved = self.cx().apply(&fa);
                let (hb, _, fb) = self.cond(b);
                let calls = self.cx().restore(saved);
                let mut f = fa;
                if calls {
                    f.retain(|(k, _)| !matches!(k, FactKey::Prop(..)));
                }
                f.extend(fb);
                (bool_expr(HExprKind::Or(Box::new(ha), Box::new(hb))), Vec::new(), f)
            }
            ExprKind::Binary(op @ (BinOp::Eq | BinOp::Ne), l, r)
                if matches!(l.kind, ExprKind::Null) || matches!(r.kind, ExprKind::Null) =>
            {
                let var_side = if matches!(l.kind, ExprKind::Null) { r } else { l };
                let h = self.binary(*op, l, r, None, span);
                let mut is_null = Vec::new();
                let mut not_null = Vec::new();
                if let Some((key, cur)) = self.fact_target(var_side) {
                    if cur.is_nullable() || matches!(cur, Type::Any | Type::Param(_)) {
                        is_null.push((key, Type::Null));
                    }
                    let nn = cur.without_null();
                    if nn != Type::Never {
                        not_null.push((key, nn));
                    }
                }
                if *op == BinOp::Eq {
                    (h, is_null, not_null)
                } else {
                    (h, not_null, is_null)
                }
            }
            ExprKind::Is(x, ty) => {
                let hx = self.expr(x, None);
                let test = self.resolve_test(ty);
                let mut then_facts = Vec::new();
                let mut else_facts = Vec::new();
                if let (Some((key, cur)), Some(test)) = (self.fact_target(x), test) {
                    // A test that can never succeed (or never fail) narrows nothing:
                    // `never` would be assignable to every type.
                    let then_ty = self.narrow_to(&cur, test);
                    let else_ty = self.remove_test(&cur, test);
                    if then_ty != Type::Never {
                        then_facts.push((key, then_ty));
                    }
                    if else_ty != Type::Never {
                        else_facts.push((key, else_ty));
                    }
                }
                let test = test.unwrap_or(TestKind::Null);
                (bool_expr(HExprKind::Is(Box::new(hx), test)), then_facts, else_facts)
            }
            ExprKind::Paren(x) => self.cond(x),
            _ => {
                let h = self.expr(e, Some(&Type::Bool));
                (self.require_bool(h), Vec::new(), Vec::new())
            }
        }
    }

    /// What a condition on `e` can narrow: a local variable, or a property
    /// of a local (`$this->user != null`).
    fn fact_target(&self, e: &Expr) -> Option<(FactKey, Type)> {
        let cx = self.ctx.last()?;
        match &e.kind {
            ExprKind::Var(name) => {
                let id = *cx.names.get(name)?;
                cx.flow.is_assigned(id).then(|| (FactKey::Local(id), cx.current_type(id)))
            }
            ExprKind::Prop { obj, name, nullsafe: false, .. } => {
                let ExprKind::Var(v) = &obj.kind else { return None };
                let id = *cx.names.get(v)?;
                if !cx.flow.is_assigned(id) {
                    return None;
                }
                let obj_ty = cx.current_type(id);
                let obj_ty = self.as_single_class(&obj_ty).unwrap_or(obj_ty);
                let Type::Class(c, args) = obj_ty else { return None };
                let slot = self.field_slot(c, name)?;
                let ty = match cx.flow.prop_narrowed.get(&(id, slot)) {
                    Some(t) => t.clone(),
                    None => {
                        let f = &self.classes[c as usize].fields[slot as usize];
                        self.member_type(&f.ty, c, &args, f.owner).resolve_open(true)
                    }
                };
                Some((FactKey::Prop(id, slot), ty))
            }
            ExprKind::Paren(x) => self.fact_target(x),
            _ => None,
        }
    }

    /// Forgets property narrowings: a call or assignment may have changed them.
    pub(crate) fn clear_prop_facts(&mut self) {
        if let Some(c) = self.ctx.last_mut() {
            c.flow.prop_narrowed.clear();
            c.prop_gen += 1;
        }
    }

    fn apply_prop_fact(&self, h: HExpr) -> HExpr {
        if let HExprKind::Prop(o, slot) = &h.kind {
            if let HExprKind::Local(id) = o.kind {
                if let Some(t) = self.ctx.last().and_then(|c| c.flow.prop_narrowed.get(&(id, *slot))) {
                    return HExpr { ty: t.clone(), ..h };
                }
            }
        }
        h
    }

    fn resolve_test(&mut self, ty: &TypeExpr) -> Option<TestKind> {
        let TypeExprKind::Named(n, args) = &ty.kind else {
            if let TypeExprKind::Func(..) = ty.kind {
                return Some(TestKind::Func);
            }
            self.error("`is` checks one type at a time, e.g. `$x is string`", ty.span);
            return None;
        };
        if !args.is_empty() {
            self.error(format!("`is` cannot check type arguments; use `is {n}`"), ty.span);
        }
        Some(match n.as_str() {
            "int" => TestKind::Int,
            "float" => TestKind::Float,
            "string" => TestKind::Str,
            "bool" => TestKind::Bool,
            "null" => TestKind::Null,
            "Html" => TestKind::Html,
            "list" => TestKind::List,
            "map" => TestKind::Map,
            "set" => TestKind::Set,
            other => match self.class_ref(other, ty.span) {
                Some(c) => TestKind::Class(c),
                None => {
                    self.error(format!("unknown type `{other}`"), ty.span);
                    return None;
                }
            },
        })
    }

    fn test_matches(&self, t: &Type, test: TestKind) -> bool {
        match (test, t) {
            (TestKind::Null, Type::Null)
            | (TestKind::Bool, Type::Bool)
            | (TestKind::Int, Type::Int)
            | (TestKind::Float, Type::Float)
            | (TestKind::Str, Type::Str)
            | (TestKind::Html, Type::Html | Type::SafeUrl | Type::SafeJs)
            | (TestKind::List, Type::List(_))
            | (TestKind::Map, Type::Map(..))
            | (TestKind::Set, Type::Set(_))
            | (TestKind::Func, Type::Func(..)) => true,
            (TestKind::Class(target), Type::Class(c, _)) => self.is_subclass(*c, target),
            // `Sql` values are lists at runtime.
            (TestKind::List, Type::Sql) => true,
            _ => false,
        }
    }

    /// Class `c` with unknown type arguments: what `is C` proves (type
    /// arguments are not known at run time).
    pub(crate) fn any_args(&self, c: ClassId) -> Type {
        Type::Class(c, vec![Type::Unknown; self.classes[c as usize].type_params.len()])
    }

    fn test_type(&self, test: TestKind) -> Type {
        match test {
            TestKind::Null => Type::Null,
            TestKind::Bool => Type::Bool,
            TestKind::Int => Type::Int,
            TestKind::Float => Type::Float,
            TestKind::Str => Type::Str,
            TestKind::Html => Type::Html,
            TestKind::List => Type::list(Type::Any),
            TestKind::Map => Type::map(Type::Any, Type::Any),
            TestKind::Set => Type::set(Type::Any),
            TestKind::Func => Type::Any,
            TestKind::Class(c) => self.any_args(c),
        }
    }

    fn narrow_to(&self, cur: &Type, test: TestKind) -> Type {
        if matches!(cur, Type::Any | Type::Error) {
            return self.test_type(test);
        }
        let mut out = Vec::new();
        for m in cur.members() {
            if self.test_matches(&m, test) {
                out.push(m);
                continue;
            }
            match (test, &m) {
                // A type parameter (or `any`) can hold anything: narrow to the tested type.
                (_, Type::Param(_)) | (_, Type::Any) => out.push(self.test_type(test)),
                (TestKind::Class(target), Type::Class(c, _)) => {
                    // A subclass of `c` may be (or implement) `target`.
                    let either_interface =
                        self.class_kind(target) == ClassKind::Interface || self.class_kind(*c) == ClassKind::Interface;
                    if self.is_subclass(target, *c) || either_interface {
                        out.push(self.any_args(target));
                    }
                }
                _ => {}
            }
        }
        Type::union(out)
    }

    fn remove_test(&self, cur: &Type, test: TestKind) -> Type {
        if matches!(cur, Type::Any | Type::Error) {
            return cur.clone();
        }
        Type::union(cur.members().into_iter().filter(|m| !self.test_matches(m, test)).collect())
    }

    // ----- expressions -----------------------------------------------------

    pub(crate) fn expr(&mut self, e: &Expr, expect: Option<&Type>) -> HExpr {
        // Past the budget of large types, the rest is not checked (an error was reported).
        if self.reported_type_nodes {
            return err(e.span);
        }
        let mut h = self.expr_here(e, expect);
        self.check_type_size(&mut h);
        h
    }

    /// Refuses the type of an expression that is too large on its own (made
    /// larger than `MAX_TYPE_SIZE` from the types of others), and counts a
    /// large one against `MAX_TYPE_TOTAL`.
    pub(crate) fn check_type_size(&mut self, h: &mut HExpr) {
        use super::{LARGE_TYPE, MAX_TYPE_SIZE, MAX_TYPE_TOTAL};
        let n = super::type_size(&h.ty, MAX_TYPE_SIZE);
        if super::take_too_large() || n > MAX_TYPE_SIZE {
            let msg = format!("the type of this expression is too large (more than {MAX_TYPE_SIZE} type nodes)");
            self.error(msg, h.span).add_note(
                "it is made from the types of the values it uses (a generic call or a closure can double them); \
                 declare a smaller type for one of those values, e.g. a class",
            );
            h.ty = Type::Error;
            return;
        }
        if n >= LARGE_TYPE {
            self.type_nodes += n;
            if self.type_nodes > MAX_TYPE_TOTAL && !self.reported_type_nodes {
                self.reported_type_nodes = true;
                let msg = format!(
                    "the types of the expressions in this program are too large in total (more than {MAX_TYPE_TOTAL} type nodes)"
                );
                self.error(msg, h.span).add_note(
                    "each use of a value holds a copy of its type; declare smaller types for the values used most, e.g. classes",
                );
                h.ty = Type::Error;
            }
        }
    }

    fn expr_here(&mut self, e: &Expr, expect: Option<&Type>) -> HExpr {
        let span = e.span;
        let mk = |kind, ty| HExpr { kind, ty, span };
        match &e.kind {
            ExprKind::Int(i) => hconst(HConst::Int(*i), Type::Int, span),
            ExprKind::Float(f) => hconst(HConst::Float(*f), Type::Float, span),
            ExprKind::Str(s) => hconst(HConst::Str(s.as_str().into()), Type::Str, span),
            ExprKind::Bool(b) => hconst(HConst::Bool(*b), Type::Bool, span),
            ExprKind::Null => hconst(HConst::Null, Type::Null, span),
            ExprKind::Interp(parts) => {
                let mut items = Vec::new();
                for p in parts {
                    match p {
                        InterpPart::Lit(s) => items.push(hconst(HConst::Str(s.as_str().into()), Type::Str, span)),
                        InterpPart::Expr(x) => {
                            let h = self.expr(x, None);
                            self.check_stringable(&h, "interpolate");
                            items.push(h);
                        }
                    }
                }
                mk(HExprKind::Concat(items, false), Type::Str)
            }
            ExprKind::Var(name) => match self.read_var(name, span) {
                Some((id, ty)) => mk(HExprKind::Local(id), ty),
                None => err(span),
            },
            ExprKind::Paren(x) => {
                let h = self.expr(x, expect);
                HExpr { span, ..h }
            }
            ExprKind::ClassConst { class, class_span, name, name_span } => {
                self.class_const(class, *class_span, name, *name_span, span)
            }
            ExprKind::Sql(parts) => {
                // Placeholders and identifiers are marked with private-use
                // characters; each database renders them in its own syntax
                // (`?` or `$1`, `"name"` or `` `name` ``), so a `?` written in
                // the SQL (a PostgreSQL operator, a quoted string) stays as written.
                let mut text = String::new();
                let mut params = Vec::new();
                for p in parts {
                    match p {
                        InterpPart::Lit(s) => {
                            if s.contains(['\u{E000}', '\u{E001}', '\u{E002}']) {
                                self.error("SQL text cannot contain the characters U+E000 to U+E002", span);
                            }
                            text.push_str(s)
                        }
                        InterpPart::Expr(x) => {
                            let h = self.expr(x, None);
                            let ok = h.ty.members().iter().all(|m| {
                                matches!(m, Type::Int | Type::Float | Type::Str | Type::Bool | Type::Null | Type::Error)
                            });
                            if !ok {
                                let msg = format!("`{}` cannot be used as an SQL parameter", self.tn(&h.ty));
                                self.error(msg, h.span).add_note("parameters must be int, float, string, bool or null");
                            }
                            text.push('\u{E000}');
                            params.push(h);
                        }
                    }
                }
                let query = hconst(HConst::Str(text.into()), Type::Str, span);
                let params = HExpr { kind: HExprKind::List(params), ty: Type::list(Type::Any), span };
                mk(HExprKind::List(vec![query, params]), Type::Sql)
            }
            ExprKind::Name(n) => {
                if let Some((c, t)) = self.lookup_const(n) {
                    if let Some(def) = self.const_span(n) {
                        self.note_ref(span, def);
                    }
                    return hconst(c, t, span);
                }
                if self.lookup_func(n).is_some() || self.builtin_overloads(n, None).is_some() {
                    self.error(format!("`{n}` is a function, not a value"), span)
                        .add_note(format!("call it, or wrap it in a closure: `fn($x) => {n}($x)`"));
                } else {
                    let e = self.error(format!("undefined name `{n}`"), span);
                    e.add_note("variables start with `$`");
                }
                err(span)
            }
            ExprKind::List(items) => self.list_lit(items, expect, span),
            ExprKind::Map(pairs) => self.map_lit(pairs, expect, span),
            ExprKind::Unary(UnOp::Not, _) | ExprKind::Binary(BinOp::And | BinOp::Or, ..) | ExprKind::Is(..) => {
                self.cond(e).0
            }
            ExprKind::Unary(op, x) => {
                let h = self.expr(x, None);
                match (op, &h.ty) {
                    (_, Type::Error) => err(span),
                    (UnOp::Neg, Type::Int) => mk(HExprKind::Unary(UnKind::NegI, Box::new(h)), Type::Int),
                    (UnOp::Neg, Type::Float) => mk(HExprKind::Unary(UnKind::NegF, Box::new(h)), Type::Float),
                    (UnOp::BitNot, Type::Int) => mk(HExprKind::Unary(UnKind::BitNot, Box::new(h)), Type::Int),
                    (_, t) => {
                        let sym = if *op == UnOp::Neg { "-" } else { "~" };
                        let msg = format!("cannot apply unary `{sym}` to `{}`", self.tn(t));
                        self.error(msg, span);
                        err(span)
                    }
                }
            }
            ExprKind::Binary(op, l, r) => self.binary(*op, l, r, expect, span),
            ExprKind::Ternary(c, t, f) => {
                let (hc, tf, ff) = self.cond(c);
                let saved = self.cx().apply(&tf);
                let ht = self.expr(t, expect);
                self.cx().restore(saved);
                let saved = self.cx().apply(&ff);
                let hf = self.expr(f, expect);
                self.cx().restore(saved);
                let ty = self.join(&ht.ty, &hf.ty, true);
                let ht = self.coerce(ht, &ty);
                let hf = self.coerce(hf, &ty);
                mk(HExprKind::Ternary(Box::new(hc), Box::new(ht), Box::new(hf)), ty)
            }
            ExprKind::Call { callee, args }
                if matches!(&callee.kind, ExprKind::Name(n) if n == "json_decode_as")
                    && self.lookup_func("json_decode_as").is_none() =>
            {
                self.json_decode_as(args, expect, span)
            }
            ExprKind::Call { callee, args } => self.call(callee, args, span),
            ExprKind::MethodCall { obj, name, name_span, args, nullsafe } => {
                let ho = self.expr(obj, None);
                let f = |c: &mut Self, recv| c.method_on(recv, name, *name_span, args, span);
                if !*nullsafe && in_nullsafe_chain(obj) && matches!(ho.kind, HExprKind::NullSafe { .. }) {
                    return self.extend_chain(ho, span, f);
                }
                self.nullsafe(ho, *nullsafe, span, f)
            }
            ExprKind::Prop { obj, name, name_span, nullsafe } => {
                let ho = self.expr(obj, None);
                let ho = self.through_bound(ho);
                let f = |c: &mut Self, recv: HExpr| match c.prop_access(&recv, name, *name_span) {
                    Some((slot, f)) => {
                        let ty = f.ty.resolve_open(true);
                        c.apply_prop_fact(HExpr { kind: HExprKind::Prop(Box::new(recv), slot), ty, span })
                    }
                    None => err(span),
                };
                if !*nullsafe && in_nullsafe_chain(obj) && matches!(ho.kind, HExprKind::NullSafe { .. }) {
                    return self.extend_chain(ho, span, f);
                }
                self.nullsafe(ho, *nullsafe, span, f)
            }
            ExprKind::StaticCall { class, class_span, name, name_span, args } => {
                self.static_call(class, *class_span, name, *name_span, args, span)
            }
            ExprKind::New { class, class_span, type_args, args } => {
                self.new_expr(class, *class_span, type_args, args, expect, span)
            }
            ExprKind::Index { base, index } => self.index(base, index.as_deref(), span),
            ExprKind::Closure(c) => self.closure(c, expect, span),
            ExprKind::FuncRef(call) => self.func_ref(call, expect, span),
            ExprKind::Spread(inner) => {
                self.error("`...` can only be used in a list literal or for a variadic parameter", span);
                self.expr(inner, None);
                err(span)
            }
            ExprKind::Match { subject, arms } => self.match_expr(subject.as_deref(), arms, expect, span),
            ExprKind::Cast(kind, x) => self.cast(*kind, x, span),
            ExprKind::Range { from, to, inclusive } => {
                let hf = self.expr_to(from, &Type::Int);
                let ht = self.expr_to(to, &Type::Int);
                mk(HExprKind::Range(Box::new(hf), Box::new(ht), *inclusive), Type::list(Type::Int))
            }
            ExprKind::Throw(x) => {
                let h = self.throwable(x);
                mk(HExprKind::Throw(Box::new(h)), Type::Never)
            }
        }
    }

    /// Continues a `?->` chain: like PHP, `$a?->b->c` is null when `$a` is
    /// null, so `->c` is applied inside the null check.
    fn extend_chain(&mut self, chain: HExpr, span: Span, f: impl FnOnce(&mut Self, HExpr) -> HExpr) -> HExpr {
        let HExprKind::NullSafe { tmp, obj, body } = chain.kind else { unreachable!() };
        let body = f(self, *body);
        let ty = match &body.ty {
            Type::Void => Type::Void,
            t => Type::nullable(t.clone()),
        };
        HExpr { kind: HExprKind::NullSafe { tmp, obj, body: Box::new(body) }, ty, span }
    }

    /// Handles `->` and `?->` on `recv` by calling `f` with the non-null receiver.
    fn nullsafe(
        &mut self,
        recv: HExpr,
        nullsafe: bool,
        span: Span,
        f: impl FnOnce(&mut Self, HExpr) -> HExpr,
    ) -> HExpr {
        if !nullsafe || !recv.ty.is_nullable() {
            if nullsafe && recv.ty != Type::Error {
                let msg = format!("`?->` is not needed: `{}` is never null", self.tn(&recv.ty));
                self.warn(msg, recv.span).add_note("use `->`");
            }
            return f(self, recv);
        }
        let inner = recv.ty.without_null();
        let tmp = self.cx().temp(inner.clone());
        let local = HExpr { kind: HExprKind::Local(tmp), ty: inner, span: recv.span };
        let body = f(self, local);
        let ty = match &body.ty {
            Type::Void => Type::Void,
            t => Type::nullable(t.clone()),
        };
        HExpr { kind: HExprKind::NullSafe { tmp, obj: Box::new(recv), body: Box::new(body) }, ty, span }
    }

    fn list_lit(&mut self, items: &[Expr], expect: Option<&Type>, span: Span) -> HExpr {
        // `[1, 2]` where a `set<int>` is expected is a set of its items.
        if let Some(Type::Set(t)) = expect.map(|t| t.without_null()).filter(|t| !t.has_unknown()) {
            let items = self.list_lit(items, Some(&Type::list((*t).clone())), span);
            return HExpr { kind: HExprKind::CallBuiltin("set_from_list".into(), vec![items]), ty: Type::Set(t), span };
        }
        if items.iter().any(|i| matches!(i.kind, ExprKind::Spread(_))) {
            return self.spread_list(items, expect, span);
        }
        let expected = expect.map(|t| t.without_null());
        match expected {
            Some(Type::List(t)) if !t.has_unknown() => {
                let hs = items.iter().map(|i| self.expr_to(i, &t)).collect();
                return HExpr { kind: HExprKind::List(hs), ty: Type::List(t), span };
            }
            Some(t @ Type::Map(..)) if items.is_empty() => {
                return HExpr { kind: HExprKind::Map(Vec::new()), ty: t, span };
            }
            _ => {}
        }
        if items.is_empty() {
            self.error("cannot infer the element type of `[]`", span)
                .add_note("declare the variable with a type, e.g. `list<string> $names = [];`");
            return HExpr { kind: HExprKind::List(Vec::new()), ty: Type::list(Type::Error), span };
        }
        // Passed as `any` (e.g. to json_encode): keep each value's own type.
        let to_any = matches!(expect, Some(Type::Any));
        let item_expect = if to_any { Some(&Type::Any) } else { None };
        let hs: Vec<HExpr> = items.iter().map(|i| self.expr(i, item_expect)).collect();
        let elem = hs.iter().fold(Type::Never, |acc, h| self.join(&acc, &h.ty, !to_any));
        if elem == Type::Null {
            self.error("cannot infer the element type of a list of nulls", span);
        }
        let hs = hs.into_iter().map(|h| self.coerce(h, &elem)).collect();
        HExpr { kind: HExprKind::List(hs), ty: Type::list(elem), span }
    }

    /// `[1, ...$xs, ...$ys]`: the lists are joined.
    fn spread_list(&mut self, items: &[Expr], expect: Option<&Type>, span: Span) -> HExpr {
        let known = match expect.map(|t| t.without_null()) {
            Some(Type::List(t)) if !t.has_unknown() => Some(*t),
            _ => None,
        };
        let mut parts = Vec::new();
        for (i, item) in items.iter().enumerate() {
            let ExprKind::Spread(inner) = &item.kind else {
                let h = match &known {
                    Some(t) => self.expr_to(item, t),
                    None => self.expr(item, None),
                };
                parts.push((i, h, false));
                continue;
            };
            let want = known.clone().map(Type::list);
            let h = self.expr(inner, want.as_ref());
            if !matches!(h.ty, Type::List(_) | Type::Error) {
                let msg = format!("`...` needs a list, found `{}`", self.tn(&h.ty));
                let e = self.error(msg, inner.span);
                if h.ty.is_nullable() {
                    e.add_note("the value might be null; use `...($xs ?? [])`");
                }
                parts.push((i, err(inner.span), true));
                continue;
            }
            parts.push((i, h, true));
        }
        let elem = match known {
            Some(t) => t,
            None => parts.iter().fold(Type::Never, |acc, (_, h, spread)| {
                let t = match (&h.ty, spread) {
                    (Type::List(e), true) => (**e).clone(),
                    (_, true) => Type::Error,
                    (t, false) => t.clone(),
                };
                self.join(&acc, &t, true)
            }),
        };
        let lt = Type::list(elem.clone());
        let parts = parts
            .into_iter()
            .map(|(i, h, spread)| (i, self.coerce(h, if spread { &lt } else { &elem }), spread))
            .collect();
        self.join_rest(parts, &lt, span)
    }

    fn map_lit(&mut self, pairs: &[(Expr, Expr)], expect: Option<&Type>, span: Span) -> HExpr {
        if let Some(Type::Map(k, v)) = expect.map(|t| t.without_null()).filter(|t| !t.has_unknown()) {
            let hs = pairs.iter().map(|(ke, ve)| (self.expr_to(ke, &k), self.expr_to(ve, &v))).collect();
            return HExpr { kind: HExprKind::Map(hs), ty: Type::Map(k, v), span };
        }
        let to_any = matches!(expect, Some(Type::Any));
        let item_expect = if to_any { Some(&Type::Any) } else { None };
        let hs: Vec<(HExpr, HExpr)> =
            pairs.iter().map(|(k, v)| (self.expr(k, None), self.expr(v, item_expect))).collect();
        let kt = hs.iter().fold(Type::Never, |acc, (k, _)| self.join(&acc, &k.ty, false));
        let vt = hs.iter().fold(Type::Never, |acc, (_, v)| self.join(&acc, &v.ty, !to_any));
        if !matches!(kt, Type::Int | Type::Str | Type::Error) {
            let msg = format!("map keys must all be `int` or all be `string`, found `{}`", self.tn(&kt));
            self.error(msg, span);
        }
        let hs = hs.into_iter().map(|(k, v)| (k, self.coerce(v, &vt))).collect();
        HExpr { kind: HExprKind::Map(hs), ty: Type::map(kt, vt), span }
    }

    // ----- operators -------------------------------------------------------

    pub(crate) fn arith_kind(&self, op: BinOp, l: &Type, r: &Type) -> Option<(BinKind, Type)> {
        if !l.is_numeric() || !r.is_numeric() {
            return None;
        }
        let ints = *l == Type::Int && *r == Type::Int;
        let (ik, fk) = match op {
            BinOp::Add => (BinKind::AddI, BinKind::AddF),
            BinOp::Sub => (BinKind::SubI, BinKind::SubF),
            BinOp::Mul => (BinKind::MulI, BinKind::MulF),
            BinOp::Div => (BinKind::DivF, BinKind::DivF),
            BinOp::Mod => (BinKind::ModI, BinKind::ModF),
            BinOp::Pow => (BinKind::PowI, BinKind::PowF),
            _ => return None,
        };
        if ints && ik != BinKind::DivF {
            Some((ik, Type::Int))
        } else {
            Some((fk, Type::Float))
        }
    }

    fn binary(&mut self, op: BinOp, l: &Expr, r: &Expr, expect: Option<&Type>, span: Span) -> HExpr {
        if op == BinOp::Coalesce {
            return self.coalesce(l, r, expect, span);
        }
        let hl = self.expr(l, None);
        let hr = self.expr(r, None);
        if hl.ty == Type::Error || hr.ty == Type::Error {
            return err(span);
        }
        let mk = |kind, ty| HExpr { kind, ty, span };
        let bin = |k, a: HExpr, b: HExpr, swap| HExprKind::Binary(k, Box::new(a), Box::new(b), swap);
        match op {
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod | BinOp::Pow => {
                match self.arith_kind(op, &hl.ty, &hr.ty) {
                    Some((kind, ty)) => {
                        let (hl, hr) = if ty == Type::Float {
                            (self.widen_to_float(hl), self.widen_to_float(hr))
                        } else {
                            (hl, hr)
                        };
                        mk(bin(kind, hl, hr, false), ty)
                    }
                    None => {
                        let msg = format!(
                            "cannot apply `{}` to `{}` and `{}`",
                            op.symbol(),
                            self.tn(&hl.ty),
                            self.tn(&hr.ty)
                        );
                        let (lt, rt) = (hl.ty.clone(), hr.ty.clone());
                        let e = self.error(msg, span);
                        if op == BinOp::Add && lt == Type::Str && rt == Type::Str {
                            e.add_note("use `.` to join strings");
                        } else if lt == Type::Str || rt == Type::Str {
                            e.add_note(
                                "strings are never converted to numbers implicitly; use `->toInt()` or `->toFloat()`",
                            );
                        } else if lt.is_nullable() || rt.is_nullable() {
                            e.add_note("the value might be null; check it first or use `??`");
                        }
                        err(span)
                    }
                }
            }
            BinOp::Concat => {
                if hl.ty == Type::Sql && hr.ty == Type::Sql {
                    return mk(HExprKind::CallBuiltin("sql_concat".into(), vec![hl, hr]), Type::Sql);
                }
                if hl.ty == Type::Sql || hr.ty == Type::Sql {
                    self.error("cannot join `Sql` with a plain string", span)
                        .add_note("write the extra part as `sql\"...\"` so values stay parameters");
                    return err(span);
                }
                if hl.ty.is_html() && hr.ty.is_html() {
                    return mk(HExprKind::Concat(vec![hl, hr], true), Type::Html);
                }
                if hl.ty.is_html() || hr.ty.is_html() {
                    self.error("cannot join `Html` with a plain string", span)
                        .add_note("escape the string first with `escape($s)` so the result stays safe HTML");
                    return err(span);
                }
                self.check_stringable(&hl, "join");
                self.check_stringable(&hr, "join");
                let mut items = match hl.kind {
                    HExprKind::Concat(items, false) => items,
                    _ => vec![hl],
                };
                items.push(hr);
                mk(HExprKind::Concat(items, false), Type::Str)
            }
            BinOp::Eq | BinOp::Ne => {
                if !self.comparable(&hl.ty, &hr.ty) {
                    let msg = format!("cannot compare `{}` with `{}`", self.tn(&hl.ty), self.tn(&hr.ty));
                    self.error(msg, span).add_note("values of different types are never equal");
                }
                let kind = match (op, &hl.ty, &hr.ty) {
                    (BinOp::Eq, Type::Int, Type::Int) => BinKind::EqI,
                    (BinOp::Eq, ..) => BinKind::Eq,
                    _ => BinKind::Ne,
                };
                mk(bin(kind, hl, hr, false), Type::Bool)
            }
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge | BinOp::Cmp => {
                let ints = hl.ty == Type::Int && hr.ty == Type::Int;
                let ok = (hl.ty.is_numeric() && hr.ty.is_numeric()) || (hl.ty == Type::Str && hr.ty == Type::Str);
                if !ok {
                    let msg = format!(
                        "cannot compare `{}` and `{}` with `{}`",
                        self.tn(&hl.ty),
                        self.tn(&hr.ty),
                        op.symbol()
                    );
                    self.error(msg, span).add_note("ordering works on two numbers or two strings");
                    return err(span);
                }
                let (kind, swap) = match (op, ints) {
                    (BinOp::Lt, true) => (BinKind::LtI, false),
                    (BinOp::Le, true) => (BinKind::LeI, false),
                    (BinOp::Gt, true) => (BinKind::LtI, true),
                    (BinOp::Ge, true) => (BinKind::LeI, true),
                    (BinOp::Lt, false) => (BinKind::Lt, false),
                    (BinOp::Le, false) => (BinKind::Le, false),
                    (BinOp::Gt, false) => (BinKind::Lt, true),
                    (BinOp::Ge, false) => (BinKind::Le, true),
                    _ => (BinKind::Cmp, false),
                };
                let ty = if op == BinOp::Cmp { Type::Int } else { Type::Bool };
                mk(bin(kind, hl, hr, swap), ty)
            }
            BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr => {
                if hl.ty != Type::Int || hr.ty != Type::Int {
                    let msg = format!(
                        "`{}` needs two `int` operands, found `{}` and `{}`",
                        op.symbol(),
                        self.tn(&hl.ty),
                        self.tn(&hr.ty)
                    );
                    self.error(msg, span);
                    return err(span);
                }
                let kind = match op {
                    BinOp::BitAnd => BinKind::BitAnd,
                    BinOp::BitOr => BinKind::BitOr,
                    BinOp::BitXor => BinKind::BitXor,
                    BinOp::Shl => BinKind::Shl,
                    _ => BinKind::Shr,
                };
                mk(bin(kind, hl, hr, false), Type::Int)
            }
            BinOp::And | BinOp::Or | BinOp::Coalesce => unreachable!(),
        }
    }

    fn comparable(&self, a: &Type, b: &Type) -> bool {
        if matches!(a, Type::Error | Type::Any) || matches!(b, Type::Error | Type::Any) {
            return true;
        }
        let bm = b.members();
        a.members().iter().any(|x| {
            bm.iter().any(|y| {
                x == y
                    || (x.is_numeric() && y.is_numeric())
                    || match (x, y) {
                        (Type::Class(p, _), Type::Class(q, _)) => self.is_subclass(*p, *q) || self.is_subclass(*q, *p),
                        (Type::List(_), Type::List(_))
                        | (Type::Map(..), Type::Map(..))
                        | (Type::Set(_), Type::Set(_)) => self.assignable_exact(x, y),
                        _ => false,
                    }
            })
        })
    }

    fn coalesce(&mut self, l: &Expr, r: &Expr, expect: Option<&Type>, span: Span) -> HExpr {
        let hl = match &l.kind {
            // `$m["k"] ?? $default` looks the key up without throwing.
            ExprKind::Index { base, index: Some(k) } => {
                let hb = self.expr(base, None);
                let lookup = |c: &mut Self, hb: HExpr| -> HExpr {
                    match hb.ty.clone() {
                        Type::List(t) => {
                            let hk = c.expr_to(k, &Type::Int);
                            let ty = Type::nullable(*t);
                            HExpr { kind: HExprKind::IndexOrNull(Box::new(hb), Box::new(hk)), ty, span: l.span }
                        }
                        Type::Map(kt, vt) => {
                            let hk = c.key_to(k, &kt);
                            let ty = Type::nullable(*vt);
                            HExpr { kind: HExprKind::IndexOrNull(Box::new(hb), Box::new(hk)), ty, span: l.span }
                        }
                        Type::Error => err(l.span),
                        t => {
                            let msg = format!("cannot index into `{}`", c.tn(&t));
                            c.error(msg, base.span);
                            err(l.span)
                        }
                    }
                };
                if in_nullsafe_chain(base) && matches!(hb.kind, HExprKind::NullSafe { .. }) {
                    let h = self.extend_chain(hb, l.span, lookup);
                    // `?int` inside a nullable chain is still just `?int`.
                    HExpr { ty: Type::nullable(h.ty.without_null()), ..h }
                } else {
                    lookup(self, hb)
                }
            }
            _ => {
                let h = self.expr(l, None);
                if !h.ty.is_nullable() && !matches!(h.ty, Type::Error | Type::Any) {
                    let msg = format!("the left side of `??` is never null (it is `{}`)", self.tn(&h.ty));
                    self.warn(msg, h.span).add_note("the right side is never used");
                }
                h
            }
        };
        let inner = hl.ty.without_null();
        let rexp = expect.cloned().unwrap_or_else(|| inner.clone());
        let hr = self.expr(r, Some(&rexp));
        let ty = self.join(&inner, &hr.ty, false);
        let hr = self.coerce(hr, &ty);
        HExpr { kind: HExprKind::Coalesce(Box::new(hl), Box::new(hr)), ty, span }
    }

    fn cast(&mut self, kind: CastKind, x: &Expr, span: Span) -> HExpr {
        let h = self.expr(x, None);
        let mk = |k, ty| HExpr { kind: k, ty, span };
        let name = match kind {
            CastKind::Int => "int",
            CastKind::Float => "float",
            CastKind::String => "string",
            CastKind::Bool => "bool",
        };
        match (kind, &h.ty) {
            (_, Type::Error) => err(span),
            (CastKind::Int, Type::Int) | (CastKind::Float, Type::Float) => h,
            (CastKind::Int, Type::Float) => mk(HExprKind::FloatToInt(Box::new(h)), Type::Int),
            (CastKind::Float, Type::Int) => mk(HExprKind::IntToFloat(Box::new(h)), Type::Float),
            (
                CastKind::String,
                Type::Int | Type::Float | Type::Bool | Type::Str | Type::Html | Type::SafeUrl | Type::SafeJs,
            ) => mk(HExprKind::ToStr(Box::new(h)), Type::Str),
            (CastKind::Bool, _) => {
                self.error("casting to `bool` is not supported", span)
                    .add_note("PHV has no truthiness; compare explicitly, e.g. `$n != 0`");
                err(span)
            }
            (_, t) => {
                let msg = format!("cannot cast `{}` to `{name}`", self.tn(t));
                let t = t.clone();
                let e = self.error(msg, span);
                if t == Type::Str {
                    e.add_note(format!(
                        "use `$s->to{}()`, which returns null when the string is not a number",
                        if kind == CastKind::Int { "Int" } else { "Float" }
                    ));
                }
                err(span)
            }
        }
    }

    // ----- calls -----------------------------------------------------------

    fn call(&mut self, callee: &Expr, args: &[Arg], span: Span) -> HExpr {
        if let ExprKind::Name(name) = &callee.kind {
            if name == "render" {
                return self.render(args, span);
            }
            if name == "format" && self.lookup_func(name).is_none() {
                return self.format_call(args, span);
            }
            if name == "layout" {
                self.error("`layout()` must be a statement at the top level of a page or template", span)
                    .add_note("a page has at most one layout; it wraps what the page writes after it");
                return err(span);
            }
            if let Some(fid) = self.lookup_func(name) {
                self.note_ref(callee.span, self.funcs[fid as usize].span);
                let (hargs, ret) = self.invoke(fid, args, span);
                return HExpr { kind: HExprKind::Call(fid, hargs), ty: ret, span };
            }
            if self.builtin_overloads(name, None).is_some() {
                return self.call_builtin(name, None, None, args, span);
            }
            let e = self.error(format!("undefined function `{name}`"), callee.span);
            if let Some(h) = php_hint(name) {
                e.add_note(h);
            }
            for a in args {
                self.expr(&a.value, None);
            }
            return err(span);
        }
        let hc = self.expr(callee, None);
        match hc.ty.clone() {
            Type::Func(ps, r) => {
                if args.len() != ps.len() {
                    let msg = format!("this function takes {} argument(s) but {} were given", ps.len(), args.len());
                    self.error(msg, span);
                }
                let mut hargs = Vec::new();
                for (a, p) in args.iter().zip(ps.iter()) {
                    if let Some((_, nspan)) = &a.name {
                        self.error("closures cannot be called with named arguments", *nspan);
                    }
                    hargs.push(self.expr_to(&a.value, p));
                }
                self.clear_prop_facts();
                HExpr { kind: HExprKind::CallClosure(Box::new(hc), hargs), ty: *r, span }
            }
            Type::Error => err(span),
            t => {
                let msg = format!("`{}` cannot be called", self.tn(&t));
                self.error(msg, callee.span);
                err(span)
            }
        }
    }

    /// Checks the arguments of a call to `fid` (inferring type parameters of
    /// generic functions) and returns them with the call's result type.
    pub(crate) fn invoke(&mut self, fid: FuncId, args: &[Arg], span: Span) -> (Vec<HExpr>, Type) {
        let (hargs, ret, _) = self.invoke_with(fid, &[], &[], args, span);
        (hargs, ret)
    }

    /// `invoke` for a member of a generic class: `fixed` gives the class's type
    /// parameters that are known (from the receiver's type), `infer` those to
    /// infer from the arguments (a constructor's class). Returns what was
    /// inferred for `infer` (`Infer` where nothing was).
    pub(crate) fn invoke_with(
        &mut self,
        fid: FuncId,
        fixed: &[(Name, Type)],
        infer: &[Name],
        args: &[Arg],
        span: Span,
    ) -> (Vec<HExpr>, Type, Vec<Type>) {
        let owner = self.funcs[fid as usize].class;
        // The warning's text is made only for a deprecated function.
        if let Some(reason) = self.deprecated_funcs.get(&fid).cloned() {
            let what = format!("`{}`", self.funcs[fid as usize].name);
            self.warn_deprecated(what, Some(reason), owner, span);
        }
        let info = &self.funcs[fid as usize];
        let (mut params, mut ret, fname) = (info.params.clone(), info.ret.clone(), info.name.clone());
        // The parameters to infer become variables first: the receiver's type
        // arguments, substituted next, may name the caller's own type
        // parameters, which must not be captured by a method's parameter of
        // the same name.
        let mut tps = infer.to_vec();
        tps.extend(info.type_params.iter().cloned());
        if !tps.is_empty() {
            for p in params.iter_mut() {
                p.ty = instantiate(&p.ty, &tps);
            }
            ret = instantiate(&ret, &tps);
        }
        if !fixed.is_empty() {
            let (names, tys): (Vec<Name>, Vec<Type>) = fixed.iter().cloned().unzip();
            // Through a receiver whose type arguments are not known (`is Box`),
            // parameters accept nothing that needs them, and results are `any`.
            for p in params.iter_mut() {
                p.ty = super::subst_params(&p.ty, &names, &tys).resolve_open(false);
            }
            ret = super::subst_params(&ret, &names, &tys).resolve_open(true);
        }
        let (hargs, b) = self.call_args_b(&fname, &params, args, span);
        // Bounds: the class's (for a constructor's inferred arguments), then the function's own.
        let mut bounds = vec![None; infer.len()];
        if let Some(cb) = owner.filter(|_| !infer.is_empty()).and_then(|c| self.class_bounds.get(&c)) {
            bounds = cb.clone();
        }
        // `new Box(...)` whose class arguments come from the expected type.
        if let Some(c) = owner.filter(|_| infer.is_empty() && !fixed.is_empty()) {
            if self.funcs[fid as usize].kind == FuncKind::Ctor {
                if let Some(cb) = self.class_bounds.get(&c).cloned() {
                    let (names, tys): (Vec<Name>, Vec<Type>) = fixed.iter().cloned().unzip();
                    self.check_bounds(&format!("`{}`", self.class_names[c as usize]), &names, &cb, &tys, span);
                }
            }
        }
        let own = self.func_bounds.get(&fid).cloned().unwrap_or_else(|| vec![None; tps.len() - infer.len()]);
        bounds.extend(own);
        if bounds.iter().any(Option::is_some) {
            let actual: Vec<Type> =
                (0..tps.len()).map(|i| b.get(i).cloned().flatten().unwrap_or(Type::Infer)).collect();
            let bounds: Vec<Option<Type>> = if fixed.is_empty() {
                bounds
            } else {
                let (names, tys): (Vec<Name>, Vec<Type>) = fixed.iter().cloned().unzip();
                bounds.into_iter().map(|t| t.map(|t| super::subst_params(&t, &names, &tys))).collect()
            };
            self.check_bounds(&format!("`{fname}`"), &tps, &bounds, &actual, span);
        }
        let mut ret = subst(&ret, &b).resolve_open(true);
        if ret.has_unknown() {
            self.error(format!("cannot infer the type parameters of `{fname}` from its arguments"), span);
            ret = Type::Error;
        }
        self.clear_prop_facts();
        let inferred = (0..infer.len()).map(|i| b.get(i).cloned().flatten().unwrap_or(Type::Infer)).collect();
        (hargs, ret, inferred)
    }

    /// An argument of type `Child<int>` given for a parameter `Base<T>`: seen as
    /// the `Base` it is, so `T` can be inferred.
    fn as_param_class(&self, actual: &Type, param: &Type) -> Option<Type> {
        let (Type::Class(pc, pa), Type::Class(ac, aa)) = (param, actual) else { return None };
        if pa.is_empty() || pc == ac {
            return None;
        }
        self.upcast(*ac, aa, *pc).map(|up| Type::Class(*pc, up))
    }

    fn arg_to(&mut self, e: &Expr, p: &Type, b: &mut Bindings) -> HExpr {
        let expected = subst(p, b);
        let h = self.expr(e, Some(&expected));
        match self.as_param_class(&h.ty, p) {
            Some(seen) => unify(p, &seen, b),
            None => unify(p, &h.ty, b),
        }
        let target = subst(p, b);
        if target.has_unknown() {
            h
        } else {
            self.coerce(h, &target)
        }
    }

    fn call_args(&mut self, fname: &str, params: &[ParamInfo], args: &[Arg], span: Span) -> Vec<HExpr> {
        self.call_args_b(fname, params, args, span).0
    }

    fn call_args_b(&mut self, fname: &str, params: &[ParamInfo], args: &[Arg], span: Span) -> (Vec<HExpr>, Bindings) {
        let mut b: Bindings = Default::default();
        let mut slots: Vec<Option<HExpr>> = vec![None; params.len()];
        let mut reported_count = false;
        // Arguments that need the other arguments to infer their type (`[]`,
        // closures without parameter types) are checked last.
        let deferred = |a: &Arg| match &a.value.kind {
            ExprKind::List(items) => items.is_empty(),
            ExprKind::Map(items) => items.is_empty(),
            ExprKind::Closure(c) => c.params.iter().any(|p| p.ty.is_none()),
            _ => false,
        };
        let mut order: Vec<usize> = (0..args.len()).filter(|i| !deferred(&args[*i])).collect();
        order.extend((0..args.len()).filter(|i| deferred(&args[*i])));
        let positional = args.iter().take_while(|a| a.name.is_none()).count();
        // `int ...$xs`: the arguments from its position on, as pieces of the list.
        let rest = params.last().filter(|p| p.variadic).map(|_| params.len() - 1);
        let mut rest_parts: Vec<(usize, HExpr, bool)> = Vec::new();
        for i in order {
            let a = &args[i];
            let i = if a.name.is_none() && i >= positional { usize::MAX } else { i };
            if let ExprKind::Spread(inner) = &a.value.kind {
                if a.name.is_some() || i == usize::MAX || rest.is_none_or(|r| i < r) {
                    self.error("`...` can only pass arguments to a variadic parameter", a.value.span)
                        .add_note("a variadic parameter is declared like `int ...$numbers`");
                    self.expr(inner, None);
                    continue;
                }
                let ty = params[params.len() - 1].ty.clone();
                rest_parts.push((i, self.arg_to(inner, &ty, &mut b), true));
                continue;
            }
            match &a.name {
                None => {
                    if i == usize::MAX {
                        self.error("positional arguments cannot follow named arguments", a.value.span);
                        continue;
                    }
                    if let Some(r) = rest.filter(|r| i >= *r) {
                        let Type::List(elem) = params[r].ty.clone() else { unreachable!() };
                        rest_parts.push((i, self.arg_to(&a.value, &elem, &mut b), false));
                        continue;
                    }
                    if i >= params.len() {
                        if !reported_count {
                            reported_count = true;
                            let msg =
                                format!("`{fname}` takes {} argument(s) but {} were given", params.len(), args.len());
                            self.error(msg, a.value.span);
                        }
                        self.expr(&a.value, None);
                        continue;
                    }
                    let ty = params[i].ty.clone();
                    slots[i] = Some(self.arg_to(&a.value, &ty, &mut b));
                }
                Some((n, nspan)) => match params.iter().position(|p| &p.name == n) {
                    Some(j) if params[j].variadic => {
                        self.error(format!("`{n}` is variadic: pass its values as positional arguments"), *nspan);
                        self.expr(&a.value, None);
                    }
                    None => {
                        self.error(format!("`{fname}` has no parameter named `{n}`"), *nspan);
                        self.expr(&a.value, None);
                    }
                    Some(j) if slots[j].is_some() => {
                        self.error(format!("argument `{n}` is given more than once"), *nspan);
                    }
                    Some(j) => {
                        let ty = params[j].ty.clone();
                        slots[j] = Some(self.arg_to(&a.value, &ty, &mut b));
                    }
                },
            }
        }
        let mut out = Vec::new();
        if let Some(r) = rest {
            rest_parts.sort_by_key(|(i, ..)| *i);
            let ty = subst(&params[r].ty, &b);
            slots[r] = Some(self.join_rest(rest_parts, &ty, span));
        }
        for (j, slot) in slots.into_iter().enumerate() {
            match slot {
                Some(h) => out.push(h),
                None => match &params[j].default {
                    Some(d) => out.push(hconst(d.clone(), subst(&params[j].ty, &b), span)),
                    None => {
                        let msg = format!("missing argument `${}` for `{fname}`", params[j].name);
                        self.error(msg, span);
                        out.push(err(span));
                    }
                },
            }
        }
        (out, b)
    }

    /// The list a variadic parameter receives: single arguments are gathered
    /// into list literals and joined with spread lists.
    fn join_rest(&mut self, parts: Vec<(usize, HExpr, bool)>, ty: &Type, span: Span) -> HExpr {
        let mut pieces: Vec<HExpr> = Vec::new();
        let mut single: Vec<HExpr> = Vec::new();
        let list = |items: Vec<HExpr>| HExpr { kind: HExprKind::List(items), ty: ty.clone(), span };
        for (_, h, spread) in parts {
            if spread {
                if !single.is_empty() {
                    pieces.push(list(std::mem::take(&mut single)));
                }
                pieces.push(h);
            } else {
                single.push(h);
            }
        }
        if !single.is_empty() || pieces.is_empty() {
            pieces.push(list(single));
        }
        // Joined in pairs, so many spreads nest only logarithmically deep.
        while pieces.len() > 1 {
            let mut next = Vec::with_capacity(pieces.len().div_ceil(2));
            let mut it = pieces.into_iter();
            while let Some(a) = it.next() {
                next.push(match it.next() {
                    Some(b) => {
                        HExpr { kind: HExprKind::CallBuiltin("list_concat".into(), vec![a, b]), ty: ty.clone(), span }
                    }
                    None => a,
                });
            }
            pieces = next;
        }
        pieces.pop().unwrap()
    }

    /// `User $u = json_decode_as($text);`: the type the value is expected to have
    /// is described for the runtime, which checks the JSON against it and builds
    /// the value (objects included).
    fn json_decode_as(&mut self, args: &[Arg], expect: Option<&Type>, span: Span) -> HExpr {
        let [Arg { name: None, value }] = args else {
            self.error("`json_decode_as()` takes the JSON text", span);
            for a in args {
                self.expr(&a.value, None);
            }
            return err(span);
        };
        let text = self.expr_to(value, &Type::Str);
        let Some(ty) = expect.filter(|t| !t.has_unknown() && !matches!(t, Type::Any | Type::Error)).cloned() else {
            self.error("`json_decode_as()` needs to know the type to decode into", span)
                .add_note("declare it, e.g. `User $u = json_decode_as($text);`, or return it from a function with that return type");
            return err(span);
        };
        let mut defs: Vec<(ClassId, String)> = Vec::new();
        let mut defaults: Vec<HConst> = Vec::new();
        let shape = match self.json_shape(&ty, &mut defs, &mut defaults) {
            Ok(s) => s,
            Err(msg) => {
                self.error(format!("`json_decode_as()` cannot decode into `{}`: {msg}", self.tn(&ty)), span);
                return err(span);
            }
        };
        let mut desc = shape;
        for (cid, fields) in defs {
            desc.push_str(&format!("\nc{cid} {fields}"));
        }
        let d = hconst(HConst::Str(desc.as_str().into()), Type::Str, span);
        let list_ty = Type::List(Box::new(Type::Any));
        let defaults = defaults.into_iter().map(|c| hconst(c, Type::Any, span)).collect();
        let defaults = HExpr { kind: HExprKind::List(defaults), ty: list_ty, span };
        HExpr { kind: HExprKind::CallBuiltin("__json_as".into(), vec![text, d, defaults]), ty, span }
    }

    /// The description of a type for `__json_as`: `i f s b a`, `?X`, `lX`,
    /// `ti` / `ts` (set of int or string), `msX` / `miX` (map with string or int keys), `eN` / `nN` (enum N, by
    /// value or, for a pure enum, by case name), `cN` (class N, whose public
    /// properties are listed in `defs` as `name:X`, `name!:X` when required, or
    /// `name=K:X` when a constructor parameter gives the default `defaults[K]`).
    fn json_shape(
        &self,
        t: &Type,
        defs: &mut Vec<(ClassId, String)>,
        defaults: &mut Vec<HConst>,
    ) -> Result<String, String> {
        Ok(match t {
            Type::Int => "i".into(),
            Type::Float => "f".into(),
            Type::Str => "s".into(),
            Type::Bool => "b".into(),
            Type::Any => "a".into(),
            t if t.is_nullable() && !matches!(t.without_null(), Type::Union(_)) => {
                format!("?{}", self.json_shape(&t.without_null(), defs, defaults)?)
            }
            Type::List(e) => format!("l{}", self.json_shape(e, defs, defaults)?),
            Type::Set(e) => format!("t{}", if **e == Type::Int { "i" } else { "s" }),
            Type::Map(k, v) => {
                let key = if **k == Type::Int { "i" } else { "s" };
                format!("m{key}{}", self.json_shape(v, defs, defaults)?)
            }
            Type::Class(cid, args) => {
                let ci = &self.classes[*cid as usize];
                let name = &self.class_names[*cid as usize];
                if ci.kind == ClassKind::Enum {
                    return Ok(format!("{}{cid}", if ci.backing.is_some() { "e" } else { "n" }));
                }
                if ci.kind != ClassKind::Class {
                    return Err(format!("`{name}` is not a plain class (interfaces, abstract classes and enums cannot be built from JSON)"));
                }
                if !args.is_empty() {
                    return Err(format!("`{name}` is generic"));
                }
                // Decoding fills the properties without calling the constructor,
                // so it must not skip a check the constructor makes.
                if ci.ctor.is_some() {
                    let (vis, owner) = ci.ctor_access;
                    if vis != Visibility::Public {
                        return Err(format!("`{name}` has a non-public constructor"));
                    }
                    let runs_code = self.classes[owner as usize]
                        .decl
                        .and_then(|d| d.methods.iter().find(|m| m.name == "__construct"))
                        .is_some_and(|m| !m.body.is_empty());
                    if runs_code {
                        return Err(format!(
                            "the constructor of `{}` runs code, which decoding would skip (a class decoded from JSON may only promote its constructor's parameters)",
                            self.class_names[owner as usize]
                        ));
                    }
                }
                if !defs.iter().any(|(c, _)| c == cid) {
                    defs.push((*cid, String::new()));
                    let mut fields = Vec::new();
                    for f in ci.fields.clone() {
                        if f.vis != Visibility::Public {
                            return Err(format!(
                                "`{name}` has the non-public property `${}`; JSON fills public properties only",
                                f.name
                            ));
                        }
                        let param = ci
                            .ctor
                            .and_then(|fid| self.funcs[fid as usize].params.iter().find(|p| p.name == f.name))
                            .filter(|p| p.promote.is_some());
                        // Without a constructor parameter, a readonly property can
                        // only ever have its default: JSON must not set it either.
                        if f.readonly && param.is_none() {
                            return Err(format!(
                                "`{name}` has the readonly property `${}`, which is not a constructor parameter and so cannot be set from JSON",
                                f.name
                            ));
                        }
                        let param_default = param.and_then(|p| p.default.clone());
                        let mark = match param_default {
                            Some(c) if f.required => {
                                defaults.push(c);
                                format!("={}", defaults.len() - 1)
                            }
                            _ if f.required => "!".into(),
                            _ => String::new(),
                        };
                        fields.push(format!("{}{mark}:{}", f.name, self.json_shape(&f.ty, defs, defaults)?));
                    }
                    if let Some(d) = defs.iter_mut().find(|(c, _)| c == cid) {
                        d.1 = fields.join(",");
                    }
                }
                format!("c{cid}")
            }
            other => return Err(format!("`{}` has no JSON form", self.tn(other))),
        })
    }

    /// `format("{} has {:.2} points", $name, $score)`: the format string is read
    /// when the program is checked, so the number of values and each value's
    /// type are checked against its placeholders.
    fn format_call(&mut self, args: &[Arg], span: Span) -> HExpr {
        use phv_syntax::fmtspec::{parse_spec, pieces, Piece};
        let Some(Arg { name: None, value: Expr { kind: ExprKind::Str(fmt), span: fspan } }) = args.first() else {
            self.error("`format()` needs a format string literal first", span)
                .add_note("for example `format(\"{} items\", $n)`; a literal lets PHV check the values");
            for a in args {
                self.expr(&a.value, None);
            }
            return err(span);
        };
        let fspan = *fspan;
        let parts_of = match pieces(fmt) {
            Ok(p) => p,
            Err(e) => {
                self.error(format!("format string: {e}"), fspan);
                for a in &args[1..] {
                    self.expr(&a.value, None);
                }
                return err(span);
            }
        };
        let values = &args[1..];
        let wanted = parts_of.iter().filter(|p| matches!(p, Piece::Value(_))).count();
        if values.len() != wanted {
            let msg = format!("the format string has {wanted} placeholder(s) but {} value(s) were given", values.len());
            self.error(msg, span);
        }
        for a in values {
            if let Some((_, nspan)) = &a.name {
                self.error("`format()` takes its values in order, without names", *nspan);
            }
        }
        let mut parts = Vec::new();
        let mut given = values.iter();
        for p in parts_of {
            match p {
                Piece::Text(t) => parts.push(hconst(HConst::Str(t.as_str().into()), Type::Str, fspan)),
                Piece::Value(spec) => {
                    let Some(a) = given.next() else { break };
                    let h = self.expr(&a.value, None);
                    let sp = parse_spec(&spec).unwrap_or_default();
                    let int_only = matches!(sp.kind, Some('x' | 'X' | 'o' | 'b'));
                    let numeric = sp.precision.is_some() || sp.grouping || sp.zero || sp.kind == Some('e');
                    if h.ty != Type::Error {
                        if int_only && h.ty != Type::Int {
                            let msg = format!("`{{:{spec}}}` needs an `int`, found `{}`", self.tn(&h.ty));
                            self.error(msg, h.span);
                        } else if numeric && !matches!(h.ty, Type::Int | Type::Float) {
                            let msg = format!("`{{:{spec}}}` needs an `int` or a `float`, found `{}`", self.tn(&h.ty));
                            self.error(msg, h.span);
                        } else {
                            self.check_stringable(&h, "format");
                        }
                    }
                    let spec_c = hconst(HConst::Str(spec.as_str().into()), Type::Str, h.span);
                    let hspan = h.span;
                    parts.push(HExpr {
                        kind: HExprKind::CallBuiltin("__fmt".into(), vec![h, spec_c]),
                        ty: Type::Str,
                        span: hspan,
                    });
                }
            }
        }
        for extra in given {
            self.expr(&extra.value, None);
        }
        if parts.is_empty() {
            return hconst(HConst::Str("".into()), Type::Str, span);
        }
        HExpr { kind: HExprKind::Concat(parts, false), ty: Type::Str, span }
    }

    fn call_builtin(
        &mut self,
        name: &str,
        recv_kind: Option<Recv>,
        recv: Option<HExpr>,
        args: &[Arg],
        span: Span,
    ) -> HExpr {
        // Borrowed from the signatures parsed once per process.
        let overloads: Vec<(&'static [Type], &'static Type, &'static str)> = self
            .builtin_overloads(name, recv_kind)
            .map(|v| v.iter().map(|b| (b.params.as_slice(), &b.ret, b.sig.imp)).collect())
            .unwrap_or_default();
        for a in args {
            if let Some((_, nspan)) = &a.name {
                self.error("built-in functions do not take named arguments", *nspan);
            }
        }
        let offset = recv.is_some() as usize;
        let nargs = args.len() + offset;
        let cands: Vec<&(&[Type], &Type, &str)> = overloads.iter().filter(|o| o.0.len() == nargs).collect();
        let display = if recv.is_some() { format!("->{name}()") } else { format!("{name}()") };
        if cands.is_empty() {
            let mut counts: Vec<usize> = overloads.iter().map(|o| o.0.len() - offset).collect();
            counts.sort();
            counts.dedup();
            let counts: Vec<String> = counts.iter().map(|c| c.to_string()).collect();
            let msg = format!("`{display}` takes {} argument(s) but {} were given", counts.join(" or "), args.len());
            self.error(msg, span);
            for a in args {
                self.expr(&a.value, None);
            }
            return err(span);
        }
        if cands.len() == 1 {
            let (params, ret, imp) = *cands[0];
            let mut b: Bindings = Default::default();
            let mut out = Vec::new();
            if let Some(r) = recv {
                unify(&params[0], &r.ty, &mut b);
                let p0 = subst(&params[0], &b);
                if !p0.has_unknown() && !self.assignable_exact(&r.ty, &p0) {
                    let msg = format!("`{display}` is not available on `{}`", self.tn(&r.ty));
                    self.error(msg, span);
                    return err(span);
                }
                out.push(r);
            }
            for (i, a) in args.iter().enumerate() {
                let p = &params[i + offset];
                let expected = subst(p, &b);
                let h = self.expr(&a.value, Some(&expected));
                unify(p, &h.ty, &mut b);
                let target = subst(p, &b);
                let h = if target.has_unknown() { h } else { self.coerce(h, &target) };
                out.push(h);
            }
            let ret = subst(ret, &b);
            let ret = if ret.has_unknown() { Type::Error } else { ret };
            if runs_user_code(params, &out) {
                self.clear_prop_facts();
            }
            return HExpr { kind: HExprKind::CallBuiltin(imp.to_string(), out), ty: ret, span };
        }
        // Several overloads with the same arity: pick by argument types.
        let mut hargs: Vec<HExpr> = recv.into_iter().collect();
        for a in args {
            hargs.push(self.expr(&a.value, None));
        }
        if hargs.iter().any(|h| h.ty == Type::Error) {
            return err(span);
        }
        for exact in [true, false] {
            for (params, ret, imp) in &cands {
                let mut b: Bindings = Default::default();
                for (p, h) in params.iter().zip(&hargs) {
                    unify(p, &h.ty, &mut b);
                }
                let fits = params.iter().zip(&hargs).enumerate().all(|(i, (p, h))| {
                    let t = subst(p, &b);
                    // The receiver is never converted.
                    if exact || (i == 0 && offset == 1) {
                        self.assignable_exact(&h.ty, &t)
                    } else {
                        self.assignable(&h.ty, &t)
                    }
                });
                if fits {
                    let out: Vec<HExpr> = params
                        .iter()
                        .zip(hargs)
                        .map(|(p, h)| {
                            let t = subst(p, &b);
                            self.coerce(h, &t)
                        })
                        .collect();
                    let ret = subst(ret, &b);
                    if runs_user_code(params, &out) {
                        self.clear_prop_facts();
                    }
                    return HExpr { kind: HExprKind::CallBuiltin(imp.to_string(), out), ty: ret, span };
                }
            }
        }
        let types: Vec<String> = hargs.iter().map(|h| self.tn(&h.ty)).collect();
        let msg = format!("no version of `{display}` accepts ({})", types.join(", "));
        let sigs: Vec<String> =
            cands.iter().map(|(p, r, _)| self.tn(&Type::Func(p.to_vec(), Box::new((*r).clone())))).collect();
        self.error(msg, span).add_note(format!("available: {}", sigs.join(", ")));
        err(span)
    }

    /// A value of a bounded type parameter (`T: Shape`) is used through its bound.
    pub(crate) fn through_bound(&self, h: HExpr) -> HExpr {
        match &h.ty {
            Type::Param(n) => match self.bounds.get(n) {
                Some(b) => HExpr { ty: b.clone(), ..h },
                None => h,
            },
            _ => h,
        }
    }

    fn method_on(&mut self, recv: HExpr, name: &str, name_span: Span, args: &[Arg], span: Span) -> HExpr {
        let recv = self.through_bound(recv);
        self.note_site(name_span, &recv.ty, false);
        let recv = match self.as_single_class(&recv.ty) {
            Some(t) => HExpr { ty: t, ..recv },
            None => recv,
        };
        let recv_kind = match &recv.ty {
            Type::Str => Some(Recv::Str),
            Type::List(_) => Some(Recv::List),
            Type::Map(..) => Some(Recv::Map),
            Type::Set(_) => Some(Recv::Set),
            _ => None,
        };
        if let Some(kind) = recv_kind {
            if self.builtin_overloads(name, Some(kind)).is_none() {
                let msg = format!("`{}` has no method `{name}`", self.tn(&recv.ty));
                self.error(msg, name_span);
                return err(span);
            }
            return self.call_builtin(name, Some(kind), Some(recv), args, span);
        }
        match recv.ty.clone() {
            Type::Class(c, cargs) => {
                let Some(m) = self.classes[c as usize].methods.get(name).cloned() else {
                    let cname = self.class_names[c as usize].clone();
                    let has_prop = self.field_slot(c, name).is_some();
                    let e = self.error(format!("class `{cname}` has no method `{name}`"), name_span);
                    if has_prop {
                        e.add_note(format!("`{name}` is a property; read it with `->{name}` (no parentheses)"));
                    }
                    return err(span);
                };
                self.note_ref(name_span, self.funcs[m.func as usize].span);
                let fname = self.funcs[m.func as usize].name.clone();
                if !self.can_access(m.owner, m.vis) {
                    self.error(format!("method `{fname}` is {}", Self::vis_word(m.vis)), name_span);
                }
                if m.is_static {
                    self.error(format!("`{fname}` is static; call it as `{fname}(...)`"), name_span);
                    return err(span);
                }
                let fixed = self.receiver_args(c, &cargs, m.owner);
                let (rest, ret, _) = self.invoke_with(m.func, &fixed, &[], args, span);
                let mut hargs = vec![recv];
                hargs.extend(rest);
                let kind = if self.class_kind(c) == ClassKind::Interface {
                    HExprKind::CallNamed(name.to_string(), hargs)
                } else {
                    HExprKind::CallVirtual(m.slot.unwrap_or(0), hargs)
                };
                HExpr { kind, ty: ret, span }
            }
            Type::Error => err(span),
            t if t.is_nullable() => {
                let msg = format!("cannot call `->{name}()` on `{}` because it might be null", self.tn(&t));
                self.error(msg, recv.span).add_note("check `!= null` first, or use `?->`");
                err(span)
            }
            t => {
                let msg = format!("`{}` has no methods", self.tn(&t));
                let e = self.error(msg, name_span);
                if let Type::Param(n) = &t {
                    e.add_note(format!("give `{n}` a bound to use its methods, e.g. `<{n}: SomeInterface>`"));
                }
                err(span)
            }
        }
    }

    /// Resolves `$obj->name` to a field slot, checking visibility.
    fn prop_access(&mut self, obj: &HExpr, name: &str, name_span: Span) -> Option<(u32, FieldInfo)> {
        self.note_site(name_span, &obj.ty, false);
        let ty = self.as_single_class(&obj.ty).unwrap_or_else(|| obj.ty.clone());
        match ty {
            Type::Class(c, cargs) => {
                let cname = self.class_names[c as usize].clone();
                let Some(slot) = self.field_slot(c, name) else {
                    let has_method = self.classes[c as usize].methods.contains_key(name);
                    let e = self.error(format!("class `{cname}` has no property `${name}`"), name_span);
                    if has_method {
                        e.add_note(format!("`{name}` is a method; call it with `->{name}()`"));
                    }
                    return None;
                };
                let mut f = self.classes[c as usize].fields[slot as usize].clone();
                f.ty = self.member_type(&f.ty, c, &cargs, f.owner);
                let reason = self.deprecated_fields.get(&(f.owner, f.name.clone())).cloned();
                self.warn_deprecated(format!("property `{cname}::${name}`"), reason, Some(f.owner), name_span);
                self.note_ref(name_span, f.span);
                if !self.can_access(f.owner, f.vis) {
                    self.error(format!("property `{cname}::${name}` is {}", Self::vis_word(f.vis)), name_span);
                }
                Some((slot, f))
            }
            Type::Error => None,
            t if t.is_nullable() => {
                let msg = format!("cannot read `->{name}` on `{}` because it might be null", self.tn(&t));
                self.error(msg, obj.span).add_note("check `!= null` first, or use `?->`");
                None
            }
            t => {
                let msg = format!("`{}` has no properties", self.tn(&t));
                self.error(msg, name_span);
                None
            }
        }
    }

    /// The target of an assignment.
    pub(crate) fn place(&mut self, e: &Expr) -> Option<(HPlace, Type)> {
        match &e.kind {
            ExprKind::Var(name) => {
                if name == "this" {
                    self.error("cannot assign to `$this`", e.span);
                    return None;
                }
                let (id, ty) = self.read_var(name, e.span)?;
                Some((HPlace { base: HPlaceBase::Local(id), path: Vec::new() }, ty))
            }
            ExprKind::Paren(x) => self.place(x),
            ExprKind::Prop { obj, name, name_span, nullsafe } => {
                self.clear_prop_facts();
                if *nullsafe {
                    self.error("cannot assign through `?->`", e.span);
                    return None;
                }
                let ho = self.expr(obj, None);
                let in_own_ctor = matches!(ho.kind, HExprKind::Local(0)) && self.cx().kind == FuncKind::Ctor;
                let (slot, f) = self.prop_access(&ho, name, *name_span)?;
                if f.readonly && !(in_own_ctor && self.cx().class == Some(f.owner)) {
                    self.error(format!("cannot modify readonly property `${name}`"), e.span)
                        .add_note("readonly properties can only be set in the constructor of their class");
                }
                // Written through: a type argument that is not known accepts nothing.
                let ty = f.ty.resolve_open(false);
                Some((HPlace { base: HPlaceBase::Prop(Box::new(ho), slot), path: Vec::new() }, ty))
            }
            ExprKind::Index { base, index } => {
                let (mut p, bty) = self.place(base)?;
                if matches!(p.path.last(), Some(HPathElem::Push)) {
                    self.error("`[]` must be the last part of an assignment target", e.span);
                    return None;
                }
                match bty {
                    Type::List(t) => {
                        match index {
                            Some(k) => {
                                let hk = self.expr_to(k, &Type::Int);
                                p.path.push(HPathElem::Index(hk, None));
                            }
                            None => p.path.push(HPathElem::Push),
                        }
                        Some((p, *t))
                    }
                    Type::Map(kt, vt) => match index {
                        Some(k) => {
                            let hk = self.key_to(k, &kt);
                            // `$m["a"]["b"] = 1` creates `$m["a"]` when it is missing.
                            let create = match &*vt {
                                Type::List(_) => Some(HConst::EmptyList),
                                Type::Map(..) => Some(HConst::EmptyMap),
                                Type::Set(_) => Some(HConst::EmptySet),
                                _ => None,
                            };
                            p.path.push(HPathElem::Index(hk, create));
                            Some((p, *vt))
                        }
                        None => {
                            self.error("cannot append to a map with `[]`; give a key: `$m[$key] = $value`", e.span);
                            None
                        }
                    },
                    // `$s[] = $x` adds `$x` to the set.
                    Type::Set(t) => match index {
                        None => {
                            p.path.push(HPathElem::Push);
                            Some((p, *t))
                        }
                        Some(_) => {
                            self.error("a set has no keys; add an element with `$s[] = $value`", e.span);
                            None
                        }
                    },
                    Type::Error => None,
                    t => {
                        let msg = format!("cannot index into `{}`", self.tn(&t));
                        self.error(msg, base.span);
                        None
                    }
                }
            }
            _ => {
                self.error("cannot assign to this expression", e.span);
                None
            }
        }
    }

    fn index(&mut self, base: &Expr, index: Option<&Expr>, span: Span) -> HExpr {
        let Some(k) = index else {
            self.error("`[]` without a key can only be used to append: `$xs[] = $value;`", span);
            return err(span);
        };
        let hb = self.expr(base, None);
        if in_nullsafe_chain(base) && matches!(hb.kind, HExprKind::NullSafe { .. }) {
            return self.extend_chain(hb, span, |c, recv| c.index_on(recv, base.span, k, span));
        }
        self.index_on(hb, base.span, k, span)
    }

    fn index_on(&mut self, hb: HExpr, base_span: Span, k: &Expr, span: Span) -> HExpr {
        let mk = |hb, hk, ty| HExpr { kind: HExprKind::Index(Box::new(hb), Box::new(hk)), ty, span };
        match hb.ty.clone() {
            Type::List(t) => {
                let hk = self.expr_to(k, &Type::Int);
                mk(hb, hk, *t)
            }
            Type::Map(kt, vt) => {
                let hk = self.key_to(k, &kt);
                mk(hb, hk, *vt)
            }
            Type::Set(_) => {
                self.error("a set cannot be indexed", span).add_note("use `$s->has($value)`");
                err(span)
            }
            Type::Error => err(span),
            Type::Str => {
                self.error("strings cannot be indexed", span).add_note("use `$s->slice($i, 1)`");
                err(span)
            }
            t => {
                let msg = format!("cannot index into `{}`", self.tn(&t));
                let e = self.error(msg, base_span);
                if t.is_nullable() {
                    e.add_note("the value might be null; check it first");
                }
                err(span)
            }
        }
    }

    fn static_call(
        &mut self,
        class: &str,
        class_span: Span,
        name: &str,
        name_span: Span,
        args: &[Arg],
        span: Span,
    ) -> HExpr {
        let cur = self.cx().class;
        if class == "parent" {
            let Some(parent) = cur.and_then(|c| self.classes[c as usize].parent) else {
                self.error("`parent` can only be used in a class that extends another class", class_span);
                return err(span);
            };
            let this = self.read_var("this", class_span);
            let target = if name == "__construct" {
                match self.classes[parent as usize].ctor {
                    Some(f) => Some((f, false)),
                    None => {
                        self.error("the parent class has no constructor", span);
                        return err(span);
                    }
                }
            } else {
                self.classes[parent as usize].methods.get(name).map(|m| (m.func, m.is_static))
            };
            let Some((fid, is_static)) = target else {
                self.error(format!("the parent class has no method `{name}`"), span);
                return err(span);
            };
            if self.func_is_abstract(fid) {
                self.error(format!("cannot call abstract method `parent::{name}()`"), span);
                return err(span);
            }
            let mut hargs = Vec::new();
            let mut fixed = Vec::new();
            if !is_static {
                let Some((tid, tty)) = this else { return err(span) };
                if let Type::Class(c, cargs) = &tty {
                    let owner = self.funcs[fid as usize].class.unwrap_or(parent);
                    fixed = self.receiver_args(*c, cargs, owner);
                }
                hargs.push(HExpr { kind: HExprKind::Local(tid), ty: tty, span: class_span });
            }
            let (rest, ret, _) = self.invoke_with(fid, &fixed, &[], args, span);
            hargs.extend(rest);
            return HExpr { kind: HExprKind::Call(fid, hargs), ty: ret, span };
        }
        let cid = if class == "self" {
            match cur {
                Some(c) => c,
                None => {
                    self.error("`self` can only be used inside a class", class_span);
                    return err(span);
                }
            }
        } else {
            match self.class_ref(class, class_span) {
                Some(c) => c,
                None => {
                    self.error(format!("unknown class `{class}`"), class_span);
                    return err(span);
                }
            }
        };
        if self.class_kind(cid) == ClassKind::Enum && !self.classes[cid as usize].methods.contains_key(name) {
            if let Some(h) = self.enum_static(cid, name, args, span) {
                return h;
            }
        }
        let reason = self.deprecated_classes.get(&cid).cloned();
        self.warn_deprecated(format!("class `{}`", self.class_names[cid as usize]), reason, Some(cid), class_span);
        self.note_site(name_span, &Type::class(cid), true);
        let Some(m) = self.classes[cid as usize].methods.get(name).cloned() else {
            let cname = self.class_names[cid as usize].clone();
            self.error(format!("class `{cname}` has no method `{name}`"), name_span);
            return err(span);
        };
        self.note_ref(name_span, self.funcs[m.func as usize].span);
        let fname = self.funcs[m.func as usize].name.clone();
        if !m.is_static {
            self.error(format!("`{fname}` is not static; call it on an object with `->{name}()`"), span);
            return err(span);
        }
        if !self.can_access(m.owner, m.vis) {
            self.error(format!("method `{fname}` is {}", Self::vis_word(m.vis)), span);
        }
        let (hargs, ret) = self.invoke(m.func, args, span);
        HExpr { kind: HExprKind::Call(m.func, hargs), ty: ret, span }
    }

    /// `E::cases()`, `E::from($v)` and `E::tryFrom($v)`.
    fn enum_static(&mut self, cid: ClassId, name: &str, args: &[Arg], span: Span) -> Option<HExpr> {
        let cty = Type::class(cid);
        match name {
            "cases" => {
                if !args.is_empty() {
                    self.error("`cases()` takes no arguments", span);
                }
                Some(HExpr { kind: HExprKind::EnumCases(cid), ty: Type::list(cty), span })
            }
            "from" | "tryFrom" => {
                let cname = self.class_names[cid as usize].clone();
                let Some(backing) = self.classes[cid as usize].backing.clone() else {
                    self.error(format!("`{cname}` is a pure enum; only backed enums have `{name}()`"), span);
                    return Some(err(span));
                };
                if args.len() != 1 {
                    self.error(format!("`{cname}::{name}()` takes 1 argument"), span);
                    return Some(err(span));
                }
                let h = self.expr_to(&args[0].value, &backing);
                let is_try = name == "tryFrom";
                let ty = if is_try { Type::nullable(cty) } else { cty };
                Some(HExpr { kind: HExprKind::EnumFrom(cid, Box::new(h), is_try), ty, span })
            }
            _ => None,
        }
    }

    /// `Suit::Hearts`, `Config::MAX`, `self::LIMIT`.
    fn class_const(&mut self, class: &str, class_span: Span, name: &str, name_span: Span, span: Span) -> HExpr {
        let cur = self.ctx.last().and_then(|c| c.class);
        let cid = match class {
            "self" | "static" => cur,
            "parent" => cur.and_then(|c| self.classes[c as usize].parent),
            other => self.class_ref(other, class_span),
        };
        let Some(cid) = cid else {
            self.error(format!("unknown class `{class}`"), class_span);
            return err(span);
        };
        self.note_site(name_span, &Type::class(cid), true);
        if let Some(def) = self.member_const_span(cid, name) {
            self.note_ref(name_span, def);
        }
        if let Some(&idx) = self.classes[cid as usize].case_index.get(name) {
            return HExpr { kind: HExprKind::EnumCase(cid, idx), ty: Type::class(cid), span };
        }
        if let Some((c, t)) = self.classes[cid as usize].consts.get(name).cloned() {
            return hconst(c, t, span);
        }
        let cname = self.class_names[cid as usize].clone();
        self.error(format!("`{cname}` has no constant or case named `{name}`"), name_span);
        err(span)
    }

    fn new_expr(
        &mut self,
        class: &str,
        class_span: Span,
        type_args: &[TypeExpr],
        args: &[Arg],
        expect: Option<&Type>,
        span: Span,
    ) -> HExpr {
        let cid = if class == "self" { self.cx().class } else { self.class_ref(class, class_span) };
        let Some(cid) = cid else {
            let e = self.error(format!("unknown class `{class}`"), class_span);
            if class == "stdClass" {
                e.add_note("use a map, e.g. `[\"name\" => $name]`, or declare a class");
            }
            for a in args {
                self.expr(&a.value, None);
            }
            return err(span);
        };
        let reason = self.deprecated_classes.get(&cid).cloned();
        self.warn_deprecated(format!("class `{}`", self.class_names[cid as usize]), reason, Some(cid), class_span);
        let tps = self.classes[cid as usize].type_params.clone();
        // Type arguments: written (`new Box<int>()`), from the expected type
        // (`$b: Box<int> = new Box()`), or inferred from the constructor's arguments.
        let written = if type_args.is_empty() && !tps.is_empty() {
            expect.map(|e| e.members()).unwrap_or_default().into_iter().find_map(|m| match m {
                Type::Class(c, a) if c == cid && !a.iter().any(|t| t.has_unknown()) => Some(a),
                _ => None,
            })
        } else {
            let self_class = self.ctx.last().and_then(|c| c.class);
            Some(self.class_args(cid, type_args, self_class, class_span))
        };
        let what = match self.class_kind(cid) {
            ClassKind::Abstract => Some("an abstract class"),
            ClassKind::Interface => Some("an interface"),
            ClassKind::Enum => Some("an enum"),
            ClassKind::Class => None,
        };
        if let Some(what) = what {
            let cname = self.class_names[cid as usize].clone();
            let e = self.error(format!("cannot create an instance of `{cname}`: it is {what}"), class_span);
            if what == "an enum" {
                e.add_note(format!("use a case, e.g. `{cname}::Name`"));
            }
            return err(span);
        }
        let (vis, owner) = self.classes[cid as usize].ctor_access;
        if self.classes[cid as usize].ctor.is_some() && !self.can_access(owner, vis) {
            let cname = self.class_names[cid as usize].clone();
            let word = Self::vis_word(vis);
            self.error(format!("the constructor of `{cname}` is {word}"), class_span)
                .add_note(format!("create it with one of its static methods, e.g. `{cname}::open(...)`"));
        }
        let cname = self.class_names[cid as usize].clone();
        let cannot_infer = |me: &mut Self| {
            me.error(format!("cannot infer the type arguments of `{cname}`"), class_span)
                .add_note(format!("write them, e.g. `new {cname}<int>(...)`"));
            vec![Type::Error; tps.len()]
        };
        match self.classes[cid as usize].ctor {
            Some(fid) => {
                // The constructor may be inherited: its types are in its own class's terms.
                let owner = self.funcs[fid as usize].class.unwrap_or(cid);
                let (hargs, targs) = match written {
                    Some(targs) => {
                        let fixed = self.receiver_args(cid, &targs, owner);
                        let (hargs, _, _) = self.invoke_with(fid, &fixed, &[], args, span);
                        (hargs, targs)
                    }
                    None => {
                        // Inferred as the class's own parameters, then seen through the owner.
                        let Type::Class(_, own) = self.own_type(cid) else { unreachable!() };
                        let fixed = self.receiver_args(cid, &own, owner);
                        let (hargs, _, inferred) = self.invoke_with(fid, &fixed, &tps, args, span);
                        let targs =
                            if inferred.iter().any(|t| t.has_unknown()) { cannot_infer(self) } else { inferred };
                        (hargs, targs)
                    }
                };
                HExpr { kind: HExprKind::New(cid, Some(fid), hargs), ty: Type::Class(cid, targs), span }
            }
            None => {
                if !args.is_empty() {
                    self.error(format!("`{cname}` has no constructor, so it takes no arguments"), span);
                }
                let targs = match written {
                    Some(t) => t,
                    None => cannot_infer(self),
                };
                HExpr { kind: HExprKind::New(cid, None, Vec::new()), ty: Type::Class(cid, targs), span }
            }
        }
    }

    fn render(&mut self, args: &[Arg], span: Span) -> HExpr {
        let path = match args.first() {
            Some(Arg { name: None, value: Expr { kind: ExprKind::Str(p), .. } }) => p.clone(),
            _ => {
                self.error("`render()` needs a template path as a string literal first", span)
                    .add_note("literal paths let PHV type-check templates before the program runs");
                return err(span);
            }
        };
        let key = normalize_template_path(&path);
        let Some(&mi) = self.input.templates.get(&key) else {
            self.error(format!("template `{key}` was not found"), args[0].value.span);
            return err(span);
        };
        self.note_ref(args[0].value.span, Span::new(self.input.modules[mi].ast.file, 0, 0));
        let fid = self.template_funcs[&mi];
        let params = self.funcs[fid as usize].params.clone();
        let hargs = self.call_args(&format!("template `{key}`"), &params, &args[1..], span);
        // The template may call methods that change properties.
        self.clear_prop_facts();
        HExpr { kind: HExprKind::Call(fid, hargs), ty: Type::Html, span }
    }

    // ----- closures and match ----------------------------------------------

    fn closure(&mut self, c: &ast::Closure, expect: Option<&Type>, span: Span) -> HExpr {
        let (exp_params, exp_ret) = match expect.map(|t| t.without_null()) {
            Some(Type::Func(ps, r)) if ps.len() == c.params.len() => (Some(ps), Some(*r)),
            _ => (None, None),
        };
        let class = self.cx().class;
        let mut param_infos = Vec::new();
        for (i, p) in c.params.iter().enumerate() {
            let ty = match &p.ty {
                Some(t) => self.resolve_type(t, class),
                None => match exp_params.as_ref().map(|ps| ps[i].clone()) {
                    Some(t) if !t.has_unknown() => t,
                    Some(_) => Type::Error,
                    None => {
                        self.error(format!("cannot infer the type of parameter `${}`", p.name), p.span)
                            .add_note(format!("add a type, e.g. `fn(int ${}) => ...`", p.name));
                        Type::Error
                    }
                },
            };
            if p.default.is_some() {
                self.error("closure parameters cannot have default values", p.span);
            }
            if p.promote.is_some() {
                self.error("closure parameters cannot be promoted", p.span);
            }
            if p.variadic {
                self.error("closure parameters cannot be variadic", p.span)
                    .add_note("take a list instead, e.g. `fn(list<int> $xs) => ...`");
            }
            param_infos.push(ParamInfo {
                name: p.name.clone(),
                ty,
                default: None,
                promote: None,
                variadic: false,
                span: p.span,
            });
        }
        let declared_ret = c.ret.as_ref().map(|t| self.resolve_type(t, class));
        let ret_known = declared_ret.or_else(|| exp_ret.filter(|r| !r.has_unknown()));

        let mut ctx = FnCtx::new(ret_known.clone().unwrap_or(Type::Infer), class, FuncKind::Closure);
        ctx.auto_capture = c.arrow;
        for (u, uspan) in &c.uses {
            if let Some((outer, ty)) = self.read_var(u, *uspan) {
                let id = ctx.declare(u, ty, *uspan);
                ctx.flow.mark(id);
                ctx.captures.push((id, outer));
            }
        }
        let mut param_ids = Vec::new();
        for p in &param_infos {
            let id = ctx.declare(&p.name, p.ty.clone(), p.span);
            ctx.flow.mark(id);
            param_ids.push(id);
        }
        self.ctx.push(ctx);
        let mut body = Vec::new();
        let ret = match &c.body {
            ClosureBody::Expr(e) => {
                let h = match &ret_known {
                    Some(r) if *r != Type::Void => self.expr_to(e, r),
                    _ => self.expr(e, None),
                };
                let rt = ret_known.clone().unwrap_or_else(|| h.ty.clone());
                if rt == Type::Void {
                    body.push(HStmt::Expr(h));
                } else {
                    body.push(HStmt::Return(Some(h), span));
                }
                rt
            }
            ClosureBody::Block(stmts) => {
                if ret_known.is_none() {
                    self.cx().ret = Type::Void;
                }
                self.block(stmts, &mut body);
                let rt = self.cx().ret.clone();
                if !self.cx().flow.dead && !matches!(rt, Type::Void | Type::Error) && !self.reported_type_nodes {
                    let msg = format!("this closure must return a value of type `{}` on every path", self.tn(&rt));
                    self.error(msg, span);
                }
                rt
            }
        };
        let ctx = self.ctx.pop().unwrap();
        self.warn_unused_locals(&ctx, &param_ids);
        self.note_scope(span, &ctx);
        let file = self.input.modules[self.module].ast.file;
        let param_tys: Vec<Type> = param_infos.iter().map(|p| p.ty.clone()).collect();
        let fid = self.add_func(FuncInfo {
            name: "{closure}".into(),
            type_params: Vec::new(),
            is_abstract: false,
            params: param_infos,
            ret: ret.clone(),
            body: Body::Done,
            module: self.module,
            class,
            is_static: true,
            kind: FuncKind::Closure,
            span,
        });
        let captures = ctx.captures.iter().map(|(inner, _)| *inner).collect();
        let cap_values = ctx
            .captures
            .iter()
            .map(|(inner, outer)| HExpr {
                kind: HExprKind::Local(*outer),
                ty: ctx.locals[*inner as usize].ty.clone(),
                span,
            })
            .collect();
        self.hfuncs[fid as usize] = Some(HFunc {
            name: "{closure}".into(),
            file,
            span,
            kind: FuncKind::Closure,
            params: param_ids,
            captures,
            locals: ctx.locals,
            body,
            ret: ret.clone(),
        });
        HExpr { kind: HExprKind::Closure(fid, cap_values), ty: Type::Func(param_tys, Box::new(ret)), span }
    }

    /// `f(...)`, `$obj->method(...)`, `Cls::method(...)`: a closure that calls
    /// the function with its own arguments (and the receiver, evaluated once).
    fn func_ref(&mut self, call: &Expr, expect: Option<&Type>, span: Span) -> HExpr {
        let Some((params, recv)) = self.func_ref_target(call, expect, span) else { return err(span) };
        let class = self.cx().class;
        let mut ctx = FnCtx::new(Type::Infer, class, FuncKind::Closure);
        let mut captures = Vec::new();
        let mut cap_values = Vec::new();
        if let Some(h) = recv {
            let id = ctx.declare("%recv", h.ty.clone(), span);
            ctx.flow.mark(id);
            captures.push(id);
            cap_values.push(h);
        }
        let mut param_ids = Vec::new();
        let mut args = Vec::new();
        for (i, t) in params.iter().enumerate() {
            let name = format!("%a{i}");
            let id = ctx.declare(&name, t.clone(), span);
            ctx.flow.mark(id);
            param_ids.push(id);
            args.push(Arg { name: None, value: Expr { kind: ExprKind::Var(name), span } });
        }
        let kind = match &call.kind {
            ExprKind::Call { callee, .. } => ExprKind::Call { callee: callee.clone(), args },
            ExprKind::MethodCall { name, name_span, .. } => ExprKind::MethodCall {
                obj: Box::new(Expr { kind: ExprKind::Var("%recv".into()), span }),
                name: name.clone(),
                name_span: *name_span,
                args,
                nullsafe: false,
            },
            ExprKind::StaticCall { class, class_span, name, name_span, .. } => ExprKind::StaticCall {
                class: class.clone(),
                class_span: *class_span,
                name: name.clone(),
                name_span: *name_span,
                args,
            },
            _ => unreachable!(),
        };
        self.ctx.push(ctx);
        let h = self.expr(&Expr { kind, span: call.span }, None);
        let ret = h.ty.clone();
        let body = if ret == Type::Void { vec![HStmt::Expr(h)] } else { vec![HStmt::Return(Some(h), span)] };
        let ctx = self.ctx.pop().unwrap();
        let file = self.input.modules[self.module].ast.file;
        let param_infos = params
            .iter()
            .enumerate()
            .map(|(i, t)| ParamInfo {
                name: format!("%a{i}"),
                ty: t.clone(),
                default: None,
                promote: None,
                variadic: false,
                span,
            })
            .collect();
        let fid = self.add_func(FuncInfo {
            name: "{closure}".into(),
            type_params: Vec::new(),
            is_abstract: false,
            params: param_infos,
            ret: ret.clone(),
            body: Body::Done,
            module: self.module,
            class,
            is_static: true,
            kind: FuncKind::Closure,
            span,
        });
        self.hfuncs[fid as usize] = Some(HFunc {
            name: "{closure}".into(),
            file,
            span,
            kind: FuncKind::Closure,
            params: param_ids,
            captures,
            locals: ctx.locals,
            body,
            ret: ret.clone(),
        });
        HExpr { kind: HExprKind::Closure(fid, cap_values), ty: Type::Func(params, Box::new(ret)), span }
    }

    /// The parameter types of the function `call` names, and the receiver of a method.
    fn func_ref_target(
        &mut self,
        call: &Expr,
        expect: Option<&Type>,
        span: Span,
    ) -> Option<(Vec<Type>, Option<HExpr>)> {
        let wrap = "write a closure instead, e.g. `fn(string $s): string => ...`";
        let user = |this: &mut Self, fid: FuncId, fixed: &[(Name, Type)]| -> Option<Vec<Type>> {
            let info = &this.funcs[fid as usize];
            let name = info.name.clone();
            if !info.type_params.is_empty() {
                this.error(format!("generic function `{name}` cannot be used as a value"), span).add_note(wrap);
                return None;
            }
            if info.params.iter().any(|p| p.variadic) {
                this.error(format!("`{name}` is variadic and cannot be used as a value"), span).add_note(wrap);
                return None;
            }
            let (names, tys): (Vec<Name>, Vec<Type>) = fixed.iter().cloned().unzip();
            Some(info.params.iter().map(|p| super::subst_params(&p.ty, &names, &tys).resolve_open(false)).collect())
        };
        match &call.kind {
            ExprKind::Call { callee, .. } => {
                let ExprKind::Name(n) = &callee.kind else {
                    self.error("only functions, methods and static methods can be used with `(...)`", span)
                        .add_note("a closure is already a value");
                    return None;
                };
                if n == "render" {
                    self.error("`render` cannot be used as a value", span);
                    return None;
                }
                if let Some(fid) = self.lookup_func(n) {
                    return Some((user(self, fid, &[])?, None));
                }
                let Some(overloads) = self.builtin_overloads(n, None) else {
                    self.error(format!("undefined function `{n}`"), callee.span);
                    return None;
                };
                let all: Vec<Vec<Type>> = overloads.iter().map(|o| o.params.clone()).collect();
                let fits: Vec<&Vec<Type>> = match expect.map(|t| t.without_null()) {
                    Some(Type::Func(ps, _)) => all
                        .iter()
                        .filter(|o| o.len() == ps.len() && o.iter().zip(&ps).all(|(a, b)| b.has_unknown() || a == b))
                        .collect(),
                    _ => all.iter().collect(),
                };
                match fits.as_slice() {
                    [one] if !one.iter().any(|t| t.has_unknown()) => Some(((*one).clone(), None)),
                    [_] => {
                        self.error(format!("`{n}` is generic and cannot be used as a value"), span).add_note(wrap);
                        None
                    }
                    _ => {
                        self.error(format!("`{n}` has several signatures, so it cannot be used as a value here"), span)
                            .add_note(wrap);
                        None
                    }
                }
            }
            ExprKind::MethodCall { obj, name, name_span, nullsafe, .. } => {
                if *nullsafe {
                    self.error("`?->` cannot be used with `(...)`", span).add_note("check the value for null first");
                    return None;
                }
                let h = self.expr(obj, None);
                let h = match self.as_single_class(&h.ty) {
                    Some(t) => HExpr { ty: t, ..h },
                    None => h,
                };
                match h.ty.clone() {
                    Type::Class(c, cargs) => {
                        let Some(m) = self.classes[c as usize].methods.get(name).cloned() else {
                            let cname = self.class_names[c as usize].clone();
                            self.error(format!("class `{cname}` has no method `{name}`"), *name_span);
                            return None;
                        };
                        if m.is_static {
                            let fname = self.funcs[m.func as usize].name.clone();
                            self.error(format!("`{fname}` is static; use `{fname}(...)`"), *name_span);
                            return None;
                        }
                        let fixed = self.receiver_args(c, &cargs, m.owner);
                        Some((user(self, m.func, &fixed)?, Some(h)))
                    }
                    Type::Error => None,
                    t @ (Type::Str | Type::List(_) | Type::Map(..) | Type::Set(_)) => {
                        let msg = format!("methods of `{}` cannot be used with `(...)`", self.tn(&t));
                        self.error(msg, span).add_note(wrap);
                        None
                    }
                    t => {
                        let msg = if t.is_nullable() {
                            format!("cannot use `->{name}(...)` on `{}` because it might be null", self.tn(&t))
                        } else {
                            format!("`{}` has no methods", self.tn(&t))
                        };
                        self.error(msg, obj.span);
                        None
                    }
                }
            }
            ExprKind::StaticCall { class, class_span, name, name_span, .. } => {
                let cid = match class.as_str() {
                    "self" => self.cx().class,
                    "parent" => {
                        self.error("`parent::` cannot be used with `(...)`", *class_span)
                            .add_note("write a closure that calls the parent's method");
                        return None;
                    }
                    other => self.class_ref(other, *class_span),
                };
                let Some(cid) = cid else {
                    self.error(format!("unknown class `{class}`"), *class_span);
                    return None;
                };
                match self.classes[cid as usize].methods.get(name).cloned() {
                    Some(m) if m.is_static => Some((user(self, m.func, &[])?, None)),
                    Some(_) => {
                        self.error(format!("`{class}::{name}` is not static"), *name_span)
                            .add_note(format!("use `$object->{name}(...)`"));
                        None
                    }
                    None => {
                        self.error(format!("`{class}` has no static method `{name}`"), *name_span);
                        None
                    }
                }
            }
            _ => unreachable!(),
        }
    }

    fn match_expr(&mut self, subject: Option<&Expr>, arms: &[MatchArm], expect: Option<&Type>, span: Span) -> HExpr {
        let hs = subject.map(|s| self.expr(s, None));
        let mut harms = Vec::new();
        let mut default: Option<HExpr> = None;
        let mut ty = Type::Never;
        // `match (true)`: an arm's body sees what its condition proves, and the
        // later arms (and `default`) what the earlier conditions disprove.
        let mut disproved = Vec::new();
        for arm in arms {
            if arm.conds.is_empty() {
                if default.is_some() {
                    self.error("`match` has more than one `default` arm", arm.span);
                }
                let hb = self.expr(&arm.body, expect);
                ty = self.join(&ty, &hb.ty, true);
                default = Some(hb);
                continue;
            }
            let mut conds = Vec::new();
            let mut proved = None;
            let mut falses = Vec::new();
            for c in &arm.conds {
                let hc = match &hs {
                    Some(s) => {
                        let sty = s.ty.clone();
                        let h = self.expr(c, Some(&sty));
                        if !self.comparable(&sty, &h.ty) {
                            let msg = format!(
                                "this arm can never match: `{}` is compared with `{}`",
                                self.tn(&sty),
                                self.tn(&h.ty)
                            );
                            self.error(msg, h.span);
                        }
                        h
                    }
                    None => {
                        let (h, t, f) = self.cond(c);
                        if arm.conds.len() == 1 {
                            proved = Some(t);
                        }
                        falses.push(f);
                        h
                    }
                };
                conds.push(hc);
            }
            let saved = proved.map(|t| self.cx().apply(&t));
            let hb = self.expr(&arm.body, expect);
            if let Some(saved) = saved {
                self.cx().restore(saved);
            }
            for f in falses {
                disproved.push(self.cx().apply(&f));
            }
            ty = self.join(&ty, &hb.ty, true);
            harms.push((conds, hb));
        }
        for saved in disproved.into_iter().rev() {
            self.cx().restore(saved);
        }
        if let (Some(s), None) = (&hs, &default) {
            self.check_exhaustive(&s.ty, &harms, span);
        }
        let harms = harms.into_iter().map(|(c, b)| (c, self.coerce(b, &ty))).collect();
        let default = default.map(|d| Box::new(self.coerce(d, &ty)));
        let kind = HExprKind::Match { subject: hs.map(Box::new), arms: harms, default };
        HExpr { kind, ty, span }
    }

    /// A `match` on an enum or a `bool` without `default` must have an arm for
    /// every value (and for `null` when the subject may be null).
    fn check_exhaustive(&mut self, sty: &Type, arms: &[(Vec<HExpr>, HExpr)], span: Span) {
        const MAX_NAMED: usize = 5;
        let conds = || arms.iter().flat_map(|(c, _)| c.iter()).map(|h| &h.kind);
        let mut missing: Vec<String> = Vec::new();
        match sty.without_null() {
            Type::Class(cid, _) if self.class_kind(cid) == ClassKind::Enum => {
                let covered: HashSet<u32> = conds()
                    .filter_map(|k| match k {
                        HExprKind::EnumCase(c, j) if *c == cid => Some(*j),
                        _ => None,
                    })
                    .collect();
                let cname = &self.class_names[cid as usize];
                let cases = &self.classes[cid as usize].cases;
                let count = cases.len() - covered.iter().filter(|j| (**j as usize) < cases.len()).count();
                // Only the first few are named, so the search stops soon.
                for (i, (case, _)) in cases.iter().enumerate() {
                    if missing.len() == MAX_NAMED {
                        break;
                    }
                    if !covered.contains(&(i as u32)) {
                        missing.push(format!("`{cname}::{case}`"));
                    }
                }
                if count > missing.len() {
                    missing.push(format!("{} more cases", count - missing.len()));
                }
            }
            Type::Bool => {
                for b in [true, false] {
                    if !conds().any(|k| matches!(k, HExprKind::Const(HConst::Bool(x)) if *x == b)) {
                        missing.push(format!("`{b}`"));
                    }
                }
            }
            _ => return,
        }
        if sty.is_nullable() && !conds().any(|k| matches!(k, HExprKind::Const(HConst::Null))) {
            missing.push("`null`".into());
        }
        if missing.is_empty() {
            return;
        }
        let list = match missing.as_slice() {
            [one] => one.clone(),
            [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
            [] => unreachable!(),
        };
        self.error(format!("`match` does not handle {list}"), span).add_note("add an arm for each, or a `default` arm");
    }
}
