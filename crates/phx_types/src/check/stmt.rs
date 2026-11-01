use super::flow::{assigned_vars, Flow};
use super::Checker;
use crate::hir::*;
use crate::types::*;
use phx_syntax::ast::{AssignOp, BinOp, Expr, ExprKind, Stmt, StmtKind};
use phx_syntax::Span;
use std::collections::HashSet;

/// Whether `stmts` contain a `break` that leaves the loop they are the body of.
fn breaks_out(stmts: &[Stmt], nest: u32) -> bool {
    stmts.iter().any(|s| match &s.kind {
        StmtKind::Break(level) => *level > nest,
        StmtKind::If { then, elifs, els, .. } => {
            breaks_out(then, nest)
                || elifs.iter().any(|(_, b)| breaks_out(b, nest))
                || els.as_ref().is_some_and(|b| breaks_out(b, nest))
        }
        StmtKind::Block(b) => breaks_out(b, nest),
        StmtKind::Try { body, catches, .. } => {
            breaks_out(body, nest) || catches.iter().any(|c| breaks_out(&c.body, nest))
        }
        StmtKind::While { body, .. }
        | StmtKind::DoWhile { body, .. }
        | StmtKind::For { body, .. }
        | StmtKind::Foreach { body, .. } => breaks_out(body, nest + 1),
        _ => false,
    })
}

impl Checker<'_> {
    pub(crate) fn block(&mut self, stmts: &[Stmt], out: &mut Vec<HStmt>) {
        for s in stmts {
            self.stmt(s, out);
        }
    }

    pub(crate) fn stmt(&mut self, s: &Stmt, out: &mut Vec<HStmt>) {
        match &s.kind {
            StmtKind::Expr(e) => {
                let h = self.expr(e, None);
                if h.ty == Type::Never {
                    self.cx().flow.dead = true;
                }
                out.push(HStmt::Expr(h));
            }
            StmtKind::Echo(es) => {
                for e in es {
                    let h = self.expr(e, None);
                    self.check_printable(&h, "echo");
                    out.push(HStmt::Echo(h));
                }
            }
            StmtKind::HtmlEcho(es, ctx) => {
                for e in es {
                    let h = self.expr(e, None);
                    self.check_printable(&h, "echo");
                    self.check_html_context(&h, *ctx);
                    out.push(HStmt::Echo(h));
                }
            }
            StmtKind::InlineHtml(text, scripts) => out.push(HStmt::Html(text.clone(), scripts.clone())),
            StmtKind::Assign { target, op, value } => self.assign(target, *op, value, out),
            StmtKind::TypedDecl { ty, name, name_span, value } => {
                let class = self.cx().class;
                let ty = self.resolve_type(ty, class);
                if self.cx().names.contains_key(name) {
                    self.error(format!("`${name}` is already declared"), *name_span)
                        .add_note("a variable's type is fixed by its first declaration");
                    self.expr(value, None);
                    return;
                }
                let h = self.expr_to(value, &ty);
                let vt = h.ty.clone();
                let id = self.cx().declare(name, ty.clone());
                self.set_local(id, &ty, vt);
                out.push(HStmt::Assign(HPlace { base: HPlaceBase::Local(id), path: Vec::new() }, h));
            }
            StmtKind::IncDec { target, inc } => {
                let Some((place, ty)) = self.place(target) else { return };
                let (kind, one) = match (&ty, inc) {
                    (Type::Int, true) => (BinKind::AddI, HConst::Int(1)),
                    (Type::Int, false) => (BinKind::SubI, HConst::Int(1)),
                    (Type::Float, true) => (BinKind::AddF, HConst::Float(1.0)),
                    (Type::Float, false) => (BinKind::SubF, HConst::Float(1.0)),
                    (Type::Error, _) => return,
                    (t, _) => {
                        let op = if *inc { "++" } else { "--" };
                        let msg = format!("`{op}` needs an `int` or `float`, found `{}`", self.tn(t));
                        self.error(msg, target.span);
                        return;
                    }
                };
                let one = HExpr { kind: HExprKind::Const(one), ty: ty.clone(), span: s.span };
                out.push(HStmt::Compound(place, kind, one));
            }
            StmtKind::If { cond, then, elifs, els } => {
                let h = self.if_chain(cond, then, elifs, els.as_ref());
                out.push(h);
            }
            StmtKind::While { cond, body } => {
                self.invalidate_for(body, &[]);
                let entry = self.cx().flow.clone();
                let (hc, then_facts, _) = self.cond(cond);
                self.cx().apply(&then_facts);
                let hbody = self.loop_body(body);
                let infinite = matches!(cond.kind, ExprKind::Bool(true)) && !breaks_out(body, 0);
                self.cx().flow = entry;
                self.cx().flow.dead = infinite;
                out.push(HStmt::Loop { cond: Some(hc), body: hbody, step: Vec::new(), do_while: false });
            }
            StmtKind::DoWhile { body, cond } => {
                self.invalidate_for(body, &[]);
                let entry = self.cx().flow.clone();
                let hbody = self.loop_body(body);
                self.cx().flow.dead = false;
                let (hc, _, _) = self.cond(cond);
                self.cx().flow = entry;
                out.push(HStmt::Loop { cond: Some(hc), body: hbody, step: Vec::new(), do_while: true });
            }
            StmtKind::For { init, cond, step, body } => {
                for st in init {
                    self.stmt(st, out);
                }
                self.invalidate_for(body, &[]);
                self.invalidate_for(step, &[]);
                let entry = self.cx().flow.clone();
                let hc = cond.as_ref().map(|c| {
                    let (h, facts, _) = self.cond(c);
                    self.cx().apply(&facts);
                    h
                });
                let hbody = self.loop_body(body);
                self.cx().flow.dead = false;
                let mut hstep = Vec::new();
                for st in step {
                    self.stmt(st, &mut hstep);
                }
                let infinite = cond.is_none() && !breaks_out(body, 0);
                self.cx().flow = entry;
                self.cx().flow.dead = infinite;
                out.push(HStmt::Loop { cond: hc, body: hbody, step: hstep, do_while: false });
            }
            StmtKind::Foreach { subject, key, value, body } => self.foreach(subject, key.as_ref(), value, body, out),
            StmtKind::Break(level) | StmtKind::Continue(level) => {
                let is_break = matches!(s.kind, StmtKind::Break(_));
                let word = if is_break { "break" } else { "continue" };
                let depth = self.cx().loop_depth;
                if depth == 0 {
                    self.error(format!("`{word}` outside of a loop"), s.span);
                } else if *level > depth {
                    self.error(format!("cannot `{word} {level}`: only {depth} loop(s) to leave"), s.span);
                }
                out.push(if is_break { HStmt::Break(*level) } else { HStmt::Continue(*level) });
                self.cx().flow.dead = true;
            }
            StmtKind::Return(value) => {
                let ret = self.cx().ret.clone();
                let kind = self.cx().kind;
                let h = match (value, &ret) {
                    _ if kind == FuncKind::Template => {
                        self.error("templates cannot `return`", s.span);
                        None
                    }
                    (None, Type::Void) => None,
                    (None, Type::Infer) => {
                        self.cx().ret = Type::Void;
                        None
                    }
                    (None, t) => {
                        let msg = format!("missing return value: this function returns `{}`", self.tn(t));
                        self.error(msg, s.span);
                        None
                    }
                    (Some(e), Type::Void) => {
                        let msg = if kind == FuncKind::Main {
                            "the main script cannot return a value"
                        } else {
                            "this function returns `void`, so it cannot return a value"
                        };
                        self.error(msg, e.span).add_note("declare a return type, e.g. `function f(): int`");
                        self.expr(e, None);
                        None
                    }
                    (Some(e), t) => {
                        let t = t.clone();
                        Some(self.expr_to(e, &t))
                    }
                };
                out.push(HStmt::Return(h, s.span));
                self.cx().flow.dead = true;
            }
            StmtKind::Throw(e) => {
                let h = self.throwable(e);
                out.push(HStmt::Throw(h));
                self.cx().flow.dead = true;
            }
            StmtKind::Try { body, catches, finally } => {
                let mut names = HashSet::new();
                assigned_vars(body, &mut names);
                let entry = self.cx().flow.clone();
                let mut hbody = Vec::new();
                self.block(body, &mut hbody);
                let mut result = std::mem::take(&mut self.cx().flow);
                let mut hcatches = Vec::new();
                for c in catches {
                    self.cx().flow = entry.clone();
                    self.cx().invalidate(&names);
                    let exc = self.exception_class();
                    let mut classes = Vec::new();
                    for (name, span) in &c.classes {
                        match self.lookup_class(name) {
                            Some(cid) if exc.is_some_and(|e| self.is_subclass(cid, e)) => classes.push(cid),
                            Some(_) => {
                                self.error(format!("`{name}` is not an exception class"), *span);
                            }
                            None => {
                                self.error(format!("unknown class `{name}`"), *span);
                            }
                        }
                    }
                    let var = c.var.as_ref().map(|(v, span)| {
                        let ty = match self.common_ancestor(&classes) {
                            Some(c) => Type::Class(c),
                            None => Type::Error,
                        };
                        self.loop_var(v, ty, *span)
                    });
                    let mut hb = Vec::new();
                    self.block(&c.body, &mut hb);
                    let f = std::mem::take(&mut self.cx().flow);
                    result = Flow::merge(result, f);
                    hcatches.push(HCatch { classes, var, body: hb });
                }
                let mut hfinally = None;
                if let Some(f) = finally {
                    // `finally` runs after normal completion, and also when an
                    // exception escapes at any point of the `try` or `catch`.
                    for c in catches {
                        assigned_vars(&c.body, &mut names);
                    }
                    self.cx().flow = entry.clone();
                    self.cx().invalidate(&names);
                    let escape = std::mem::take(&mut self.cx().flow);
                    self.cx().flow = Flow::merge(result.clone(), escape);
                    self.cx().flow.dead = false;
                    let mut hf = Vec::new();
                    self.block(f, &mut hf);
                    if self.cx().flow.dead {
                        result.dead = true;
                    }
                    hfinally = Some(hf);
                }
                self.cx().flow = result;
                out.push(HStmt::Try { body: hbody, catches: hcatches, finally: hfinally });
            }
            StmtKind::Block(b) => self.block(b, out),
            StmtKind::Function(d) => {
                self.error("functions must be declared at the top level of a file", d.name_span);
            }
            StmtKind::Class(d) => {
                self.error("classes must be declared at the top level of a file", d.name_span);
            }
            StmtKind::Import(d) => {
                self.error("`import` must be at the top level of a file", d.span);
            }
            StmtKind::Const(d) => {
                self.error("constants must be declared at the top level of a file", d.name_span);
            }
            StmtKind::Params(_) => {
                self.error("`params(...)` must be at the top level of a template", s.span);
            }
        }
    }

    fn loop_body(&mut self, body: &[Stmt]) -> Vec<HStmt> {
        self.cx().loop_depth += 1;
        let mut out = Vec::new();
        self.block(body, &mut out);
        self.cx().loop_depth -= 1;
        out
    }

    fn invalidate_for(&mut self, body: &[Stmt], extra: &[&str]) {
        let mut names = HashSet::new();
        assigned_vars(body, &mut names);
        for e in extra {
            names.insert(e.to_string());
        }
        self.cx().invalidate(&names);
    }

    /// Declares (or reuses) a variable that a loop or `catch` binds. When an
    /// existing variable of the same name has a different type, the loop or
    /// `catch` binds a new variable that shadows it from here on.
    fn loop_var(&mut self, name: &str, ty: Type, _span: Span) -> LocalId {
        let existing = self.cx().names.get(name).copied();
        let id = match existing {
            Some(id) if self.assignable_exact(&ty, &self.ctx.last().unwrap().locals[id as usize].ty) => id,
            _ => self.cx().declare(name, ty.clone()),
        };
        let declared = self.cx().locals[id as usize].ty.clone();
        self.set_local(id, &declared, ty);
        id
    }

    /// Marks a local as assigned with a value of type `value_ty`.
    pub(crate) fn set_local(&mut self, id: LocalId, declared: &Type, value_ty: Type) {
        let cx = self.cx();
        cx.flow.mark(id);
        cx.flow.prop_narrowed.retain(|(l, _), _| *l != id);
        if value_ty != *declared && value_ty != Type::Error && value_ty != Type::Never {
            cx.flow.narrowed.insert(id, value_ty);
        } else {
            cx.flow.narrowed.remove(&id);
        }
    }

    fn if_chain(&mut self, cond: &Expr, then: &[Stmt], elifs: &[(Expr, Vec<Stmt>)], els: Option<&Vec<Stmt>>) -> HStmt {
        let (hc, then_facts, else_facts) = self.cond(cond);
        let entry = self.cx().flow.clone();
        self.cx().apply(&then_facts);
        let mut hthen = Vec::new();
        self.block(then, &mut hthen);
        let then_flow = std::mem::replace(&mut self.cx().flow, entry);
        self.cx().apply(&else_facts);
        let mut helse = Vec::new();
        if let Some(((c2, b2), rest)) = elifs.split_first() {
            helse.push(self.if_chain(c2, b2, rest, els));
        } else if let Some(b) = els {
            self.block(b, &mut helse);
        }
        let else_flow = std::mem::take(&mut self.cx().flow);
        self.cx().flow = Flow::merge(then_flow, else_flow);
        HStmt::If(hc, hthen, helse)
    }

    fn foreach(
        &mut self,
        subject: &Expr,
        key: Option<&(String, Span)>,
        value: &(String, Span),
        body: &[Stmt],
        out: &mut Vec<HStmt>,
    ) {
        let mut extra = vec![value.0.as_str()];
        if let Some((k, _)) = key {
            extra.push(k.as_str());
        }
        if let ExprKind::Range { from, to, inclusive } = &subject.kind {
            let hf = self.expr_to(from, &Type::Int);
            let ht = self.expr_to(to, &Type::Int);
            if let Some((_, span)) = key {
                self.error("a range has no keys; use `foreach ($a..$b as $i)`", *span);
            }
            self.invalidate_for(body, &extra);
            let entry = self.cx().flow.clone();
            let var = self.loop_var(&value.0, Type::Int, value.1);
            let hbody = self.loop_body(body);
            self.cx().flow = entry;
            out.push(HStmt::ForRange { var, from: hf, to: ht, inclusive: *inclusive, body: hbody });
            return;
        }
        let hs = self.expr(subject, None);
        let (kt, vt) = match &hs.ty {
            Type::List(t) => (Type::Int, (**t).clone()),
            Type::Map(k, v) => ((**k).clone(), (**v).clone()),
            Type::Error => (Type::Error, Type::Error),
            t => {
                let msg = format!("cannot iterate over `{}`", self.tn(t));
                let e = self.error(msg, subject.span);
                if t.is_nullable() {
                    e.add_note("the value might be null; check it first or use `?? []`");
                }
                (Type::Error, Type::Error)
            }
        };
        self.invalidate_for(body, &extra);
        let entry = self.cx().flow.clone();
        let k = key.map(|(name, span)| self.loop_var(name, kt, *span));
        let v = self.loop_var(&value.0, vt, value.1);
        let hbody = self.loop_body(body);
        self.cx().flow = entry;
        out.push(HStmt::Foreach { subject: hs, key: k, val: v, body: hbody });
    }

    fn assign(&mut self, target: &Expr, op: AssignOp, value: &Expr, out: &mut Vec<HStmt>) {
        if let ExprKind::Var(name) = &target.kind {
            if name == "this" {
                self.error("cannot assign to `$this`", target.span);
                return;
            }
            if op == AssignOp::Set {
                match self.cx().names.get(name).copied() {
                    Some(id) => {
                        let declared = self.cx().locals[id as usize].ty.clone();
                        let h = self.expr_to(value, &declared);
                        let vt = h.ty.clone();
                        self.set_local(id, &declared, vt);
                        out.push(HStmt::Assign(HPlace { base: HPlaceBase::Local(id), path: Vec::new() }, h));
                    }
                    None => {
                        let h = self.expr(value, None);
                        let ty = match &h.ty {
                            Type::Null => {
                                self.error(format!("cannot infer the type of `${name}` from `null`"), target.span)
                                    .add_note(format!("declare it with a type, e.g. `?string ${name} = null;`"));
                                Type::Error
                            }
                            Type::Void => {
                                self.error("this expression does not produce a value", value.span);
                                Type::Error
                            }
                            t => t.clone(),
                        };
                        let id = self.cx().declare(name, ty);
                        self.cx().flow.mark(id);
                        out.push(HStmt::Assign(HPlace { base: HPlaceBase::Local(id), path: Vec::new() }, h));
                    }
                }
                return;
            }
        }
        let Some((place, pty)) = self.place(target) else {
            self.expr(value, None);
            return;
        };
        if op != AssignOp::Set && matches!(place.path.last(), Some(HPathElem::Push)) {
            self.error("`[]` can only be used with `=`", target.span);
            return;
        }
        match op {
            AssignOp::Set => {
                let h = self.expr_to(value, &pty);
                out.push(HStmt::Assign(place, h));
            }
            AssignOp::Coalesce => {
                if !pty.is_nullable() && pty != Type::Error {
                    let msg = format!("`??=` needs a nullable target, but this is `{}`", self.tn(&pty));
                    self.error(msg, target.span);
                }
                // A variable narrowed to `null` can still receive a value of its declared type.
                let pty = match (&place.base, place.path.is_empty()) {
                    (HPlaceBase::Local(id), true) => self.cx().locals[*id as usize].ty.clone(),
                    _ => pty,
                };
                let h = self.expr_to(value, &pty);
                if let (HPlaceBase::Local(id), true) = (&place.base, place.path.is_empty()) {
                    if !h.ty.is_nullable() {
                        self.set_local(*id, &pty, pty.without_null());
                    }
                }
                out.push(HStmt::CoalesceAssign(place, h));
            }
            AssignOp::Concat => {
                let h = self.expr(value, None);
                if pty != Type::Str && pty != Type::Error {
                    let msg = format!("`.=` needs a `string` target, but this is `{}`", self.tn(&pty));
                    self.error(msg, target.span);
                    return;
                }
                self.check_stringable(&h, "append");
                match (&place.base, place.path.is_empty()) {
                    (HPlaceBase::Local(id), true) => out.push(HStmt::Append(*id, h)),
                    _ => out.push(HStmt::Compound(place, BinKind::Concat, h)),
                }
            }
            _ => {
                let bin = match op {
                    AssignOp::Add => BinOp::Add,
                    AssignOp::Sub => BinOp::Sub,
                    AssignOp::Mul => BinOp::Mul,
                    AssignOp::Div => BinOp::Div,
                    AssignOp::Mod => BinOp::Mod,
                    _ => BinOp::Pow,
                };
                let h = self.expr(value, None);
                if pty == Type::Error || h.ty == Type::Error {
                    return;
                }
                let Some((kind, rty)) = self.arith_kind(bin, &pty, &h.ty) else {
                    let msg =
                        format!("cannot apply `{}=` to `{}` and `{}`", bin.symbol(), self.tn(&pty), self.tn(&h.ty));
                    self.error(msg, target.span.to(value.span));
                    return;
                };
                if !self.assignable_exact(&rty, &pty) {
                    let msg = format!(
                        "the result of `{}=` is `{}`, which cannot be stored in `{}`",
                        bin.symbol(),
                        self.tn(&rty),
                        self.tn(&pty)
                    );
                    self.error(msg, target.span.to(value.span));
                    return;
                }
                let h = if rty == Type::Float { self.widen_to_float(h) } else { h };
                out.push(HStmt::Compound(place, kind, h));
            }
        }
    }
}
