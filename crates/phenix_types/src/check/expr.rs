use super::flow::{Facts, FnCtx};
use super::{Body, Checker, FieldInfo, FuncInfo, ParamInfo};
use crate::builtins::{php_hint, Recv};
use crate::hir::*;
use crate::types::*;
use phenix_syntax::ast::{self, Arg, BinOp, CastKind, ClosureBody, Expr, ExprKind, InterpPart, MatchArm, TypeExpr, TypeExprKind, UnOp, Visibility};
use phenix_syntax::{normalize_template_path, Span};

type Bindings = [Option<Type>; 4];

fn unify(param: &Type, actual: &Type, b: &mut Bindings) {
    match (param, actual) {
        (Type::Generic(i), t) => {
            let slot = &mut b[*i as usize];
            if slot.is_none() && !t.has_unknown() && *t != Type::Error && *t != Type::Never {
                *slot = Some(t.clone());
            }
        }
        (Type::List(p), Type::List(a)) => unify(p, a, b),
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
        _ => {}
    }
}

fn subst(t: &Type, b: &Bindings) -> Type {
    match t {
        Type::Generic(i) => b[*i as usize].clone().unwrap_or(Type::Infer),
        Type::List(x) => Type::list(subst(x, b)),
        Type::Map(k, v) => Type::map(subst(k, b), subst(v, b)),
        Type::Func(ps, r) => Type::Func(ps.iter().map(|p| subst(p, b)).collect(), Box::new(subst(r, b))),
        Type::Union(ms) => Type::union(ms.iter().map(|m| subst(m, b)).collect()),
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

fn hconst(c: HConst, ty: Type, span: Span) -> HExpr {
    HExpr { kind: HExprKind::Const(c), ty, span }
}

fn err(span: Span) -> HExpr {
    HExpr { kind: HExprKind::Const(HConst::Null), ty: Type::Error, span }
}

impl<'a> Checker<'a> {
    // ----- assignability ---------------------------------------------------

    pub(crate) fn assignable(&self, from: &Type, to: &Type) -> bool {
        self.assignable_inner(from, to, true)
    }

    pub(crate) fn assignable_exact(&self, from: &Type, to: &Type) -> bool {
        self.assignable_inner(from, to, false)
    }

    fn assignable_inner(&self, from: &Type, to: &Type, coerce: bool) -> bool {
        if from == to {
            return true;
        }
        match (from, to) {
            (Type::Error, _) | (_, Type::Error) | (Type::Never, _) => true,
            (Type::Void, _) | (_, Type::Void) => false,
            (_, Type::Any) => true,
            (Type::Union(ms), _) => ms.iter().all(|m| self.assignable_inner(m, to, false)),
            (_, Type::Union(ts)) => ts.iter().any(|t| self.assignable_inner(from, t, coerce)),
            (Type::Int, Type::Float) => coerce,
            (Type::Class(a), Type::Class(b)) => self.is_subclass(*a, *b),
            (Type::List(a), Type::List(b)) => self.same(a, b),
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
        a == b || *a == Type::Error || *b == Type::Error
    }

    /// Smallest type both `a` and `b` fit in. With `widen`, `int` and `float` join to `float`.
    fn join(&self, a: &Type, b: &Type, widen: bool) -> Type {
        if a == b {
            return a.clone();
        }
        match (a, b) {
            (Type::Never, t) | (t, Type::Never) => t.clone(),
            (Type::Error, _) | (_, Type::Error) => Type::Error,
            (Type::Int, Type::Float) | (Type::Float, Type::Int) if widen => Type::Float,
            (Type::Class(x), Type::Class(y)) => match self.common_ancestor(&[*x, *y]) {
                Some(c) => Type::Class(c),
                None => Type::union(vec![a.clone(), b.clone()]),
            },
            _ if self.assignable_exact(a, b) => b.clone(),
            _ if self.assignable_exact(b, a) => a.clone(),
            _ => Type::union(vec![a.clone(), b.clone()]),
        }
    }

    pub(crate) fn expr_to(&mut self, e: &Expr, target: &Type) -> HExpr {
        let h = self.expr(e, Some(target));
        self.coerce(h, target)
    }

    pub(crate) fn coerce(&mut self, h: HExpr, target: &Type) -> HExpr {
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
        }
        HExpr { ty: Type::Error, ..h }
    }

    pub(crate) fn to_float(&mut self, h: HExpr) -> HExpr {
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
        let printable = |t: &Type| matches!(t, Type::Str | Type::Int | Type::Float | Type::Html | Type::Error | Type::Never);
        match &h.ty {
            t if t.members().iter().all(printable) && t.members().len() > 0 => {}
            Type::Never => {}
            t => {
                let msg = format!("cannot {what} a value of type `{}`", self.tn(t));
                let t = t.clone();
                let e = self.error(msg, h.span);
                if t == Type::Bool {
                    e.add_note("bools are not converted to strings implicitly; use a ternary, e.g. `$ok ? \"yes\" : \"no\"`");
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
            t if t.members().iter().all(ok) && t.members().len() > 0 => {}
            Type::Never => {}
            t => {
                let msg = format!("cannot {what} a value of type `{}` to a string", self.tn(t));
                let t = t.clone();
                let e = self.error(msg, h.span);
                match t {
                    Type::Html => e.add_note("join two `Html` values with `.`, or use `(string)$html` to get the raw markup"),
                    Type::Bool => e.add_note("use a ternary, e.g. `$ok ? \"yes\" : \"no\"`"),
                    t if t.is_nullable() => e.add_note("the value might be null; give a fallback with `?? \"\"`"),
                    _ => e.add_note("only `string`, `int` and `float` can be joined into strings"),
                };
            }
        }
    }

    pub(crate) fn throwable(&mut self, x: &Expr) -> HExpr {
        let h = self.expr(x, None);
        let exc = self.exception_class();
        match &h.ty {
            Type::Class(c) if exc.is_some_and(|e| self.is_subclass(*c, e)) => h,
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
            Type::Str => "Phenix has no truthiness: write `$s != \"\"` or `!$s->isEmpty()`",
            Type::Int | Type::Float => "Phenix has no truthiness: write `$n != 0`",
            Type::List(_) | Type::Map(..) => "Phenix has no truthiness: write `!$xs->isEmpty()`",
            _ => "Phenix has no truthiness: compare explicitly, e.g. `$x != null`",
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
                self.cx().restore(saved);
                let mut t = ta;
                t.extend(tb);
                (bool_expr(HExprKind::And(Box::new(ha), Box::new(hb))), t, Vec::new())
            }
            ExprKind::Binary(BinOp::Or, a, b) => {
                let (ha, _, fa) = self.cond(a);
                let saved = self.cx().apply(&fa);
                let (hb, _, fb) = self.cond(b);
                self.cx().restore(saved);
                let mut f = fa;
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
                if let ExprKind::Var(name) = &var_side.kind {
                    if let Some(&id) = self.cx().names.get(name) {
                        if self.cx().flow.is_assigned(id) {
                            let cur = self.cx().current_type(id);
                            is_null.push((id, Type::Null));
                            not_null.push((id, cur.without_null()));
                        }
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
                if let (ExprKind::Var(name), Some(test)) = (&x.kind, test) {
                    if let Some(&id) = self.cx().names.get(name) {
                        let cur = self.cx().current_type(id);
                        then_facts.push((id, self.narrow_to(&cur, test)));
                        else_facts.push((id, self.remove_test(&cur, test)));
                    }
                }
                let test = test.unwrap_or(TestKind::Null);
                (bool_expr(HExprKind::Is(Box::new(hx), test)), then_facts, else_facts)
            }
            _ => {
                let h = self.expr(e, Some(&Type::Bool));
                (self.require_bool(h), Vec::new(), Vec::new())
            }
        }
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
            self.error(format!("`is` cannot check element types; use `is {n}`"), ty.span);
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
            other => match self.lookup_class(other) {
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
            | (TestKind::Html, Type::Html)
            | (TestKind::List, Type::List(_))
            | (TestKind::Map, Type::Map(..))
            | (TestKind::Func, Type::Func(..)) => true,
            (TestKind::Class(target), Type::Class(c)) => self.is_subclass(*c, target),
            _ => false,
        }
    }

    fn test_type(test: TestKind) -> Type {
        match test {
            TestKind::Null => Type::Null,
            TestKind::Bool => Type::Bool,
            TestKind::Int => Type::Int,
            TestKind::Float => Type::Float,
            TestKind::Str => Type::Str,
            TestKind::Html => Type::Html,
            TestKind::List => Type::list(Type::Any),
            TestKind::Map => Type::map(Type::Any, Type::Any),
            TestKind::Func => Type::Error,
            TestKind::Class(c) => Type::Class(c),
        }
    }

    fn narrow_to(&self, cur: &Type, test: TestKind) -> Type {
        if matches!(cur, Type::Any | Type::Error) {
            return Self::test_type(test);
        }
        let mut out = Vec::new();
        for m in cur.members() {
            if self.test_matches(&m, test) {
                out.push(m);
            } else if let (TestKind::Class(target), Type::Class(c)) = (test, &m) {
                if self.is_subclass(target, *c) {
                    out.push(Type::Class(target));
                }
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
        let span = e.span;
        let mk = |kind, ty| HExpr { kind, ty, span };
        match &e.kind {
            ExprKind::Int(i) => hconst(HConst::Int(*i), Type::Int, span),
            ExprKind::Float(f) => hconst(HConst::Float(*f), Type::Float, span),
            ExprKind::Str(s) => hconst(HConst::Str(s.clone()), Type::Str, span),
            ExprKind::Bool(b) => hconst(HConst::Bool(*b), Type::Bool, span),
            ExprKind::Null => hconst(HConst::Null, Type::Null, span),
            ExprKind::Interp(parts) => {
                let mut items = Vec::new();
                for p in parts {
                    match p {
                        InterpPart::Lit(s) => items.push(hconst(HConst::Str(s.clone()), Type::Str, span)),
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
            ExprKind::Name(n) => {
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
                let f = |c: &mut Self, recv: HExpr| match c.prop_access(&recv, name, *name_span) {
                    Some((slot, f)) => HExpr { kind: HExprKind::Prop(Box::new(recv), slot), ty: f.ty, span },
                    None => err(span),
                };
                if !*nullsafe && in_nullsafe_chain(obj) && matches!(ho.kind, HExprKind::NullSafe { .. }) {
                    return self.extend_chain(ho, span, f);
                }
                self.nullsafe(ho, *nullsafe, span, f)
            }
            ExprKind::StaticCall { class, class_span, name, args } => self.static_call(class, *class_span, name, args, span),
            ExprKind::New { class, class_span, args } => self.new_expr(class, *class_span, args, span),
            ExprKind::Index { base, index } => self.index(base, index.as_deref(), span),
            ExprKind::Closure(c) => self.closure(c, expect, span),
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
        let expected = expect.map(|t| t.without_null());
        match expected {
            Some(Type::List(t)) => {
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

    fn map_lit(&mut self, pairs: &[(Expr, Expr)], expect: Option<&Type>, span: Span) -> HExpr {
        if let Some(Type::Map(k, v)) = expect.map(|t| t.without_null()) {
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
                        let (hl, hr) = if ty == Type::Float { (self.to_float(hl), self.to_float(hr)) } else { (hl, hr) };
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
                            e.add_note("strings are never converted to numbers implicitly; use `->toInt()` or `->toFloat()`");
                        } else if lt.is_nullable() || rt.is_nullable() {
                            e.add_note("the value might be null; check it first or use `??`");
                        }
                        err(span)
                    }
                }
            }
            BinOp::Concat => {
                if hl.ty == Type::Html && hr.ty == Type::Html {
                    return mk(HExprKind::Concat(vec![hl, hr], true), Type::Html);
                }
                if hl.ty == Type::Html || hr.ty == Type::Html {
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
                        (Type::Class(p), Type::Class(q)) => self.is_subclass(*p, *q) || self.is_subclass(*q, *p),
                        (Type::List(_), Type::List(_)) | (Type::Map(..), Type::Map(..)) => self.assignable_exact(x, y),
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
                            let hk = c.expr_to(k, &kt);
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
            (CastKind::String, Type::Int | Type::Float | Type::Bool | Type::Str | Type::Html) => {
                mk(HExprKind::ToStr(Box::new(h)), Type::Str)
            }
            (CastKind::Bool, _) => {
                self.error("casting to `bool` is not supported", span)
                    .add_note("Phenix has no truthiness; compare explicitly, e.g. `$n != 0`");
                err(span)
            }
            (_, t) => {
                let msg = format!("cannot cast `{}` to `{name}`", self.tn(t));
                let t = t.clone();
                let e = self.error(msg, span);
                if t == Type::Str {
                    e.add_note(format!("use `$s->to{}()`, which returns null when the string is not a number",
                        if kind == CastKind::Int { "Int" } else { "Float" }));
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
            if let Some(fid) = self.lookup_func(name) {
                let info = &self.funcs[fid as usize];
                let (params, ret, fname) = (info.params.clone(), info.ret.clone(), info.name.clone());
                let hargs = self.call_args(&fname, &params, args, span);
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

    fn call_args(&mut self, fname: &str, params: &[ParamInfo], args: &[Arg], span: Span) -> Vec<HExpr> {
        let mut slots: Vec<Option<HExpr>> = vec![None; params.len()];
        let mut seen_named = false;
        let mut reported_count = false;
        for (i, a) in args.iter().enumerate() {
            match &a.name {
                None => {
                    if seen_named {
                        self.error("positional arguments cannot follow named arguments", a.value.span);
                    }
                    if i >= params.len() {
                        if !reported_count {
                            reported_count = true;
                            let msg = format!("`{fname}` takes {} argument(s) but {} were given", params.len(), args.len());
                            self.error(msg, a.value.span);
                        }
                        self.expr(&a.value, None);
                        continue;
                    }
                    let ty = params[i].ty.clone();
                    slots[i] = Some(self.expr_to(&a.value, &ty));
                }
                Some((n, nspan)) => {
                    seen_named = true;
                    match params.iter().position(|p| &p.name == n) {
                        None => {
                            self.error(format!("`{fname}` has no parameter named `{n}`"), *nspan);
                            self.expr(&a.value, None);
                        }
                        Some(j) if slots[j].is_some() => {
                            self.error(format!("argument `{n}` is given more than once"), *nspan);
                        }
                        Some(j) => {
                            let ty = params[j].ty.clone();
                            slots[j] = Some(self.expr_to(&a.value, &ty));
                        }
                    }
                }
            }
        }
        let mut out = Vec::new();
        for (j, slot) in slots.into_iter().enumerate() {
            match slot {
                Some(h) => out.push(h),
                None => match &params[j].default {
                    Some(d) => out.push(hconst(d.clone(), params[j].ty.clone(), span)),
                    None => {
                        let msg = format!("missing argument `${}` for `{fname}`", params[j].name);
                        self.error(msg, span);
                        out.push(err(span));
                    }
                },
            }
        }
        out
    }

    fn call_builtin(&mut self, name: &str, recv_kind: Option<Recv>, recv: Option<HExpr>, args: &[Arg], span: Span) -> HExpr {
        let overloads: Vec<(Vec<Type>, Type, &'static str)> = self
            .builtin_overloads(name, recv_kind)
            .map(|v| v.iter().map(|b| (b.params.clone(), b.ret.clone(), b.sig.imp)).collect())
            .unwrap_or_default();
        for a in args {
            if let Some((_, nspan)) = &a.name {
                self.error("built-in functions do not take named arguments", *nspan);
            }
        }
        let offset = recv.is_some() as usize;
        let nargs = args.len() + offset;
        let cands: Vec<&(Vec<Type>, Type, &str)> = overloads.iter().filter(|o| o.0.len() == nargs).collect();
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
            let (params, ret, imp) = cands[0].clone();
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
            let ret = subst(&ret, &b);
            let ret = if ret.has_unknown() { Type::Error } else { ret };
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
                    let out = params
                        .iter()
                        .zip(hargs)
                        .map(|(p, h)| {
                            let t = subst(p, &b);
                            self.coerce(h, &t)
                        })
                        .collect();
                    let ret = subst(ret, &b);
                    return HExpr { kind: HExprKind::CallBuiltin(imp.to_string(), out), ty: ret, span };
                }
            }
        }
        let types: Vec<String> = hargs.iter().map(|h| self.tn(&h.ty)).collect();
        let msg = format!("no version of `{display}` accepts ({})", types.join(", "));
        let sigs: Vec<String> = cands.iter().map(|(p, r, _)| self.tn(&Type::Func(p.clone(), Box::new(r.clone())))).collect();
        self.error(msg, span).add_note(format!("available: {}", sigs.join(", ")));
        err(span)
    }

    fn method_on(&mut self, recv: HExpr, name: &str, name_span: Span, args: &[Arg], span: Span) -> HExpr {
        let recv = match self.as_single_class(&recv.ty) {
            Some(t) => HExpr { ty: t, ..recv },
            None => recv,
        };
        let recv_kind = match &recv.ty {
            Type::Str => Some(Recv::Str),
            Type::List(_) => Some(Recv::List),
            Type::Map(..) => Some(Recv::Map),
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
            Type::Class(c) => {
                let Some(m) = self.classes[c as usize].methods.get(name).cloned() else {
                    let cname = self.class_names[c as usize].clone();
                    let has_prop = self.classes[c as usize].fields.iter().any(|f| f.name == name);
                    let e = self.error(format!("class `{cname}` has no method `{name}`"), name_span);
                    if has_prop {
                        e.add_note(format!("`{name}` is a property; read it with `->{name}` (no parentheses)"));
                    }
                    return err(span);
                };
                let info = &self.funcs[m.func as usize];
                let (params, ret, fname) = (info.params.clone(), info.ret.clone(), info.name.clone());
                if !self.can_access(m.owner, m.vis) {
                    self.error(format!("method `{fname}` is {}", Self::vis_word(m.vis)), name_span);
                }
                if m.is_static {
                    self.error(format!("`{fname}` is static; call it as `{fname}(...)`"), name_span);
                    return err(span);
                }
                let mut hargs = vec![recv];
                hargs.extend(self.call_args(&fname, &params, args, span));
                let kind = HExprKind::CallVirtual(m.slot.unwrap_or(0), hargs);
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
                self.error(msg, name_span);
                err(span)
            }
        }
    }

    /// Resolves `$obj->name` to a field slot, checking visibility.
    fn prop_access(&mut self, obj: &HExpr, name: &str, name_span: Span) -> Option<(u32, FieldInfo)> {
        let ty = self.as_single_class(&obj.ty).unwrap_or_else(|| obj.ty.clone());
        match ty {
            Type::Class(c) => {
                let cname = self.class_names[c as usize].clone();
                let Some(slot) = self.field_slot(c, name) else {
                    let has_method = self.classes[c as usize].methods.contains_key(name);
                    let e = self.error(format!("class `{cname}` has no property `${name}`"), name_span);
                    if has_method {
                        e.add_note(format!("`{name}` is a method; call it with `->{name}()`"));
                    }
                    return None;
                };
                let f = self.classes[c as usize].fields[slot as usize].clone();
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
            ExprKind::Prop { obj, name, name_span, nullsafe } => {
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
                Some((HPlace { base: HPlaceBase::Prop(Box::new(ho), slot), path: Vec::new() }, f.ty))
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
                                p.path.push(HPathElem::Index(hk));
                            }
                            None => p.path.push(HPathElem::Push),
                        }
                        Some((p, *t))
                    }
                    Type::Map(kt, vt) => match index {
                        Some(k) => {
                            let hk = self.expr_to(k, &kt);
                            p.path.push(HPathElem::Index(hk));
                            Some((p, *vt))
                        }
                        None => {
                            self.error("cannot append to a map with `[]`; give a key: `$m[$key] = $value`", e.span);
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
                let hk = self.expr_to(k, &kt);
                mk(hb, hk, *vt)
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

    fn static_call(&mut self, class: &str, class_span: Span, name: &str, args: &[Arg], span: Span) -> HExpr {
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
            let info = &self.funcs[fid as usize];
            let (params, ret, fname) = (info.params.clone(), info.ret.clone(), info.name.clone());
            let mut hargs = Vec::new();
            if !is_static {
                let Some((tid, tty)) = this else { return err(span) };
                hargs.push(HExpr { kind: HExprKind::Local(tid), ty: tty, span: class_span });
            }
            hargs.extend(self.call_args(&fname, &params, args, span));
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
            match self.lookup_class(class) {
                Some(c) => c,
                None => {
                    self.error(format!("unknown class `{class}`"), class_span);
                    return err(span);
                }
            }
        };
        let Some(m) = self.classes[cid as usize].methods.get(name).cloned() else {
            let cname = self.class_names[cid as usize].clone();
            self.error(format!("class `{cname}` has no method `{name}`"), span);
            return err(span);
        };
        let info = &self.funcs[m.func as usize];
        let (params, ret, fname) = (info.params.clone(), info.ret.clone(), info.name.clone());
        if !m.is_static {
            self.error(format!("`{fname}` is not static; call it on an object with `->{name}()`"), span);
            return err(span);
        }
        if !self.can_access(m.owner, m.vis) {
            self.error(format!("method `{fname}` is {}", Self::vis_word(m.vis)), span);
        }
        let hargs = self.call_args(&fname, &params, args, span);
        HExpr { kind: HExprKind::Call(m.func, hargs), ty: ret, span }
    }

    fn new_expr(&mut self, class: &str, class_span: Span, args: &[Arg], span: Span) -> HExpr {
        let cid = if class == "self" { self.cx().class } else { self.lookup_class(class) };
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
        let ty = Type::Class(cid);
        match self.classes[cid as usize].ctor {
            Some(fid) => {
                let info = &self.funcs[fid as usize];
                let (params, fname) = (info.params.clone(), info.name.clone());
                let hargs = self.call_args(&fname, &params, args, span);
                HExpr { kind: HExprKind::New(cid, Some(fid), hargs), ty, span }
            }
            None => {
                if !args.is_empty() {
                    let cname = self.class_names[cid as usize].clone();
                    self.error(format!("`{cname}` has no constructor, so it takes no arguments"), span);
                }
                HExpr { kind: HExprKind::New(cid, None, Vec::new()), ty, span }
            }
        }
    }

    fn render(&mut self, args: &[Arg], span: Span) -> HExpr {
        let path = match args.first() {
            Some(Arg { name: None, value: Expr { kind: ExprKind::Str(p), .. } }) => p.clone(),
            _ => {
                self.error("`render()` needs a template path as a string literal first", span)
                    .add_note("literal paths let Phenix type-check templates before the program runs");
                return err(span);
            }
        };
        let key = normalize_template_path(&path);
        let Some(&mi) = self.input.templates.get(&key) else {
            self.error(format!("template `{key}` was not found"), args[0].value.span);
            return err(span);
        };
        let fid = self.template_funcs[&mi];
        let params = self.funcs[fid as usize].params.clone();
        let hargs = self.call_args(&format!("template `{key}`"), &params, &args[1..], span);
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
            param_infos.push(ParamInfo { name: p.name.clone(), ty, default: None, promote: None });
        }
        let declared_ret = c.ret.as_ref().map(|t| self.resolve_type(t, class));
        let ret_known = declared_ret.or_else(|| exp_ret.filter(|r| !r.has_unknown()));

        let mut ctx = FnCtx::new(ret_known.clone().unwrap_or(Type::Infer), class, FuncKind::Closure);
        ctx.auto_capture = c.arrow;
        for (u, uspan) in &c.uses {
            if let Some((outer, ty)) = self.read_var(u, *uspan) {
                let id = ctx.declare(u, ty);
                ctx.flow.mark(id);
                ctx.captures.push((id, outer));
            }
        }
        let mut param_ids = Vec::new();
        for p in &param_infos {
            let id = ctx.declare(&p.name, p.ty.clone());
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
                if !self.cx().flow.dead && !matches!(rt, Type::Void | Type::Error) {
                    let msg = format!("this closure must return a value of type `{}` on every path", self.tn(&rt));
                    self.error(msg, span);
                }
                rt
            }
        };
        let ctx = self.ctx.pop().unwrap();
        let file = self.input.modules[self.module].ast.file;
        let param_tys: Vec<Type> = param_infos.iter().map(|p| p.ty.clone()).collect();
        let fid = self.add_func(FuncInfo {
            name: "{closure}".into(),
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
        });
        HExpr { kind: HExprKind::Closure(fid, cap_values), ty: Type::Func(param_tys, Box::new(ret)), span }
    }

    fn match_expr(&mut self, subject: Option<&Expr>, arms: &[MatchArm], expect: Option<&Type>, span: Span) -> HExpr {
        let hs = subject.map(|s| self.expr(s, None));
        let mut harms = Vec::new();
        let mut default: Option<HExpr> = None;
        let mut ty = Type::Never;
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
            for c in &arm.conds {
                let hc = match &hs {
                    Some(s) => {
                        let sty = s.ty.clone();
                        let h = self.expr(c, Some(&sty));
                        if !self.comparable(&sty, &h.ty) {
                            let msg = format!("this arm can never match: `{}` is compared with `{}`", self.tn(&sty), self.tn(&h.ty));
                            self.error(msg, h.span);
                        }
                        h
                    }
                    None => self.cond(c).0,
                };
                conds.push(hc);
            }
            let hb = self.expr(&arm.body, expect);
            ty = self.join(&ty, &hb.ty, true);
            harms.push((conds, hb));
        }
        let harms = harms.into_iter().map(|(c, b)| (c, self.coerce(b, &ty))).collect();
        let default = default.map(|d| Box::new(self.coerce(d, &ty)));
        let kind = HExprKind::Match { subject: hs.map(Box::new), arms: harms, default };
        HExpr { kind, ty, span }
    }
}
