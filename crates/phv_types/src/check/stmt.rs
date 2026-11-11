use super::flow::{assigned_vars, ContinueTo, Flow};
use super::Checker;
use crate::hir::*;
use crate::types::*;
use phv_syntax::ast::{Arg, AssignOp, BinOp, Expr, ExprKind, MethodCall, Stmt, StmtKind};
use phv_syntax::Span;
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
        // A `break` in `finally` also leaves the loop (even one that ran into a `return`).
        StmtKind::Try { body, catches, finally } => {
            breaks_out(body, nest)
                || catches.iter().any(|c| breaks_out(&c.body, nest))
                || finally.as_ref().is_some_and(|f| breaks_out(f, nest))
        }
        StmtKind::While { body, .. }
        | StmtKind::DoWhile { body, .. }
        | StmtKind::For { body, .. }
        | StmtKind::Foreach { body, .. } => breaks_out(body, nest + 1),
        _ => false,
    })
}

/// A value inserted inside `<svg>`/`<math>`, inside `<select>`, into the text
/// of a raw-text element or into a comment: `Html` (a template's output, a
/// layout's body, `raw()`) was checked as HTML on its own, so it is looked at
/// when it is inserted (`__html_inside` refuses markup that browsers would
/// read in another way there, or that ends the comment; found by the 1.1.1
/// audit). Strings and numbers are escaped and need no look.
fn html_inside(h: HExpr, ctx: phv_syntax::ast::EchoContext) -> HExpr {
    use phv_syntax::ast::EchoContext;
    // `watched `: where the end tag of any raw-text element may end what a
    // browser reads (found by the 1.2 audit).
    let place = match ctx {
        EchoContext::ForeignText(place) => place,
        EchoContext::SelectText => "select",
        EchoContext::RawText(tag) => tag,
        EchoContext::Comment { bogus: false, watched: false } => "comment",
        EchoContext::Comment { bogus: false, watched: true } => "watched comment",
        EchoContext::Comment { bogus: true, watched: false } => "bogus comment",
        EchoContext::Comment { bogus: true, watched: true } => "watched bogus comment",
        _ => return h,
    };
    let plain = |t: &Type| matches!(t, Type::Int | Type::Float | Type::Bool | Type::Str | Type::Null | Type::Error);
    if h.ty.members().iter().all(plain) {
        return h;
    }
    let (ty, span) = (h.ty.clone(), h.span);
    let place = HExpr { kind: HExprKind::Const(HConst::Str(place.into())), ty: Type::Str, span };
    HExpr { kind: HExprKind::CallBuiltin("__html_inside".into(), vec![h, place]), ty, span }
}

impl Checker<'_> {
    pub(crate) fn block(&mut self, stmts: &[Stmt], out: &mut Vec<HStmt>) {
        let mut warned = false;
        for s in stmts {
            self.note_unreachable(s, &mut warned);
            self.stmt(s, out);
        }
    }

    pub(crate) fn stmt(&mut self, s: &Stmt, out: &mut Vec<HStmt>) {
        // Past the budget of large types, the rest is not checked (an error was reported).
        if self.reported_type_nodes {
            return;
        }
        self.stmt_sized(s, out);
        if super::take_too_large() {
            let msg = format!("a type here is too large (more than {} type nodes)", super::MAX_TYPE_SIZE);
            self.error(msg, s.span);
        }
    }

    fn stmt_sized(&mut self, s: &Stmt, out: &mut Vec<HStmt>) {
        if s.out.is_some() {
            let mut inner = Vec::new();
            self.stmt_here(s, &mut inner);
            out.push(HStmt::Guarded(inner));
        } else {
            self.stmt_here(s, out);
        }
    }

    fn stmt_here(&mut self, s: &Stmt, out: &mut Vec<HStmt>) {
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
                use phv_syntax::ast::EchoContext;
                for e in es {
                    let h = self.expr(e, None);
                    self.check_printable(&h, "echo");
                    self.check_html_context(&h, *ctx);
                    let h = html_inside(h, *ctx);
                    if matches!(
                        ctx,
                        EchoContext::Attr
                            | EchoContext::Url
                            | EchoContext::UrlReference
                            | EchoContext::UrlListNumber
                            | EchoContext::MetaContent
                    ) {
                        out.push(HStmt::EchoAttr(h));
                    } else {
                        out.push(HStmt::Echo(h));
                    }
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
                let id = self.cx().declare(name, ty.clone(), *name_span);
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
                let (hbody, _) = self.loop_body(body, &entry);
                let infinite = matches!(cond.kind, ExprKind::Bool(true)) && !breaks_out(body, 0);
                self.cx().flow = entry;
                self.cx().flow.dead = infinite;
                out.push(HStmt::Loop { cond: Some(hc), body: hbody, step: Vec::new(), do_while: false });
            }
            StmtKind::DoWhile { body, cond } => {
                self.invalidate_for(body, &[]);
                let entry = self.cx().flow.clone();
                let (hbody, continued) = self.loop_body(body, &entry);
                self.after_body(continued);
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
                let (hbody, continued) = self.loop_body(body, &entry);
                self.after_body(continued);
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
                if !is_break {
                    self.note_continue(*level);
                }
                out.push(if is_break { HStmt::Break(*level) } else { HStmt::Continue(*level) });
                self.cx().flow.dead = true;
            }
            StmtKind::Return(value) => {
                let ret = self.cx().ret.clone();
                let kind = self.cx().kind;
                let generator = self.cx().gen.is_some();
                let h = match (value, &ret) {
                    _ if kind == FuncKind::Template => {
                        self.error("templates cannot `return`", s.span);
                        None
                    }
                    (None, _) if generator => None,
                    (Some(e), _) if generator => {
                        self.error("a generator cannot return a value", e.span)
                            .add_note("`return;` ends it; hand values to the loop with `yield`");
                        self.expr(e, None);
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
            StmtKind::Yield { key, value } => {
                let Some((kt, vt)) = self.yield_types(s) else {
                    if let Some(k) = key {
                        self.expr(k, None);
                    }
                    self.expr(value, None);
                    return;
                };
                let hk = match key {
                    Some(k) => Some(self.expr_to(k, &kt)),
                    None => {
                        if !matches!(kt, Type::Int | Type::Error) {
                            let msg = format!("this generator's keys are `{}`: give each value a key", self.tn(&kt));
                            self.error(msg, value.span).add_note("write `yield $key => $value;`");
                        }
                        None
                    }
                };
                let hv = self.expr_to(value, &vt);
                // The loop over the generator runs until it asks for the next
                // value, and may change any property.
                self.clear_prop_facts();
                out.push(HStmt::Yield { key: hk, val: hv, from: false });
            }
            StmtKind::YieldFrom(e) => {
                let Some((kt, vt)) = self.yield_types(s) else {
                    self.expr(e, None);
                    return;
                };
                let hs = self.expr(e, None);
                let (ikt, ivt) = match &hs.ty {
                    Type::List(t) | Type::Set(t) => (Type::Int, (**t).clone()),
                    Type::Map(k, v) => ((**k).clone(), (**v).clone()),
                    Type::Error => (Type::Error, Type::Error),
                    t => match self.generator_args(t) {
                        Some(kv) => kv,
                        None => {
                            let msg = format!("cannot `yield from` a value of type `{}`", self.tn(t));
                            self.error(msg, e.span)
                                .add_note("`yield from` takes a list, a map, a set or another generator");
                            (Type::Error, Type::Error)
                        }
                    },
                };
                let mut ok = true;
                for (what, inner, outer) in [("keys", &ikt, &kt), ("values", &ivt, &vt)] {
                    if !self.assignable(inner, outer) {
                        let msg = format!(
                            "`yield from` gives {what} of type `{}`, but this generator's {what} are `{}`",
                            self.tn(inner),
                            self.tn(outer)
                        );
                        self.error(msg, e.span);
                        ok = false;
                    }
                }
                // `foreach ($e as $k => $v) yield $k => $v;`, where the keys do
                // not move the generator's own int keys on (as in PHP).
                let (ikt, ivt) = if ok { (ikt, ivt) } else { (Type::Error, Type::Error) };
                let k = self.cx().temp(ikt.clone());
                let v = self.cx().temp(ivt.clone());
                let span = e.span;
                let hk = self.coerce(HExpr { kind: HExprKind::Local(k), ty: ikt, span }, &kt);
                let hv = self.coerce(HExpr { kind: HExprKind::Local(v), ty: ivt, span }, &vt);
                self.clear_prop_facts();
                let body = vec![HStmt::Yield { key: Some(hk), val: hv, from: true }];
                out.push(HStmt::Foreach { subject: hs, key: Some(k), val: v, body });
            }
            StmtKind::Try { body, catches, finally } => {
                let mut names = HashSet::new();
                assigned_vars(body, &mut names);
                let entry = self.cx().flow.clone();
                // A `continue` in the body or a `catch` runs the `finally` first.
                let fin_depth = u32::from(finally.is_some());
                self.cx().finally_depth += fin_depth;
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
                        match self.class_ref(name, *span) {
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
                            Some(c) => self.any_args(c),
                            None => Type::Error,
                        };
                        let id = self.loop_var(v, ty, *span);
                        self.cx().catch_vars.insert(id);
                        id
                    });
                    let mut hb = Vec::new();
                    self.block(&c.body, &mut hb);
                    let f = std::mem::take(&mut self.cx().flow);
                    result = Flow::merge(result, f);
                    hcatches.push(HCatch { classes, var, body: hb });
                }
                self.cx().finally_depth -= fin_depth;
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
                    let mut written = HashSet::new();
                    assigned_vars(f, &mut written);
                    let mut ids: HashSet<LocalId> =
                        written.iter().filter_map(|n| self.cx().names.get(n).copied()).collect();
                    let gen = self.cx().prop_gen;
                    let mut hf = Vec::new();
                    self.cx().in_finally += 1;
                    self.block(f, &mut hf);
                    self.cx().in_finally -= 1;
                    ids.extend(written.iter().filter_map(|n| self.cx().names.get(n).copied()));
                    let calls = self.cx().prop_gen != gen;
                    let fin = std::mem::take(&mut self.cx().flow);
                    result = Flow::after_finally(result, &fin, &ids, calls);
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
            StmtKind::TypeAlias(d) => {
                self.error("types must be declared at the top level of a file", d.name_span);
            }
            StmtKind::Params(_) => {
                self.error("`params(...)` must be at the top level of a template", s.span);
            }
        }
    }

    /// The key and value types `yield` (or `yield from`) statement `s` hands
    /// on, or None after an error: `yield` belongs to a function (not the main
    /// script), not in a `finally` block (a generator closed early runs its
    /// `finally` blocks, and could not wait there), and not in HTML other than
    /// element text (what the loop over the generator writes meanwhile would
    /// land there).
    fn yield_types(&mut self, s: &Stmt) -> Option<(Type, Type)> {
        let cx = self.cx();
        let (gen, kind, in_finally) = (cx.gen.clone(), cx.kind, cx.in_finally);
        let Some(kv) = gen else {
            let msg = if kind == FuncKind::Main {
                "`yield` can only be used inside a function"
            } else {
                "`yield` cannot be used here"
            };
            self.error(msg, s.span);
            return None;
        };
        if in_finally > 0 {
            self.error("`yield` cannot be used in a `finally` block", s.span).add_note(
                "a generator that a loop leaves early runs its `finally` blocks, which cannot wait for the loop",
            );
            return None;
        }
        if let Some(ctx) = s.out {
            // Inside a `<select>`, the loop's HTML was checked outside one
            // (found by the 19th assessment).
            let msg = if ctx == phv_syntax::ast::EchoContext::SelectText {
                "`yield` cannot be used inside an open `<select>`"
            } else {
                "`yield` cannot be used inside a tag, an attribute, `<script>` or other HTML that is not element text"
            };
            self.error(msg, s.span).add_note("what the loop over the generator writes meanwhile would land there");
            return None;
        }
        Some(kv)
    }

    /// `[$a, $b] = $list;`, `[$first, ...$rest] = $list;` and
    /// `["id" => $id] = $map;` (nested patterns too): the value is kept in a
    /// hidden variable, and each target is assigned its element.
    fn destructure(&mut self, pattern: &Expr, value: &Expr, out: &mut Vec<HStmt>) {
        let h = self.expr(value, None);
        let is_list = matches!(pattern.kind, ExprKind::List(_));
        let ok = match &h.ty {
            Type::List(_) => is_list,
            Type::Map(..) => !is_list,
            Type::Error => true,
            _ => false,
        };
        if !ok {
            let msg = format!("cannot destructure `{}` with this pattern", self.tn(&h.ty));
            let e = self.error(msg, value.span);
            match &h.ty {
                Type::List(_) => {
                    e.add_note("a list is destructured with `[$a, $b]`");
                }
                Type::Map(..) => {
                    e.add_note("a map is destructured with `[\"key\" => $var]`");
                }
                t if t.is_nullable() => {
                    e.add_note("the value might be null; check it first");
                }
                _ => {}
            }
        }
        let name = format!("%d{}", self.cx().locals.len());
        // After an error, the targets get `Type::Error` instead of more errors.
        let ty = if ok { h.ty.clone() } else { Type::Error };
        let id = self.cx().declare(&name, ty, pattern.span);
        self.cx().flow.mark(id);
        out.push(HStmt::Assign(HPlace { base: HPlaceBase::Local(id), path: Vec::new() }, h));
        let tmp = || Box::new(Expr { kind: ExprKind::Var(name.clone()), span: pattern.span });
        let index = |k: Expr| ExprKind::Index { base: tmp(), index: Some(Box::new(k)) };
        let mut items: Vec<(&Expr, ExprKind)> = Vec::new();
        match &pattern.kind {
            ExprKind::List(xs) => {
                for (i, x) in xs.iter().enumerate() {
                    if let ExprKind::Spread(rest) = &x.kind {
                        if i + 1 != xs.len() {
                            self.error("`...` must come last in a pattern", x.span);
                        }
                        let from = Arg { name: None, value: Expr { kind: ExprKind::Int(i as i64), span: x.span } };
                        let slice = ExprKind::MethodCall(Box::new(MethodCall {
                            obj: tmp(),
                            name: "slice".into(),
                            name_span: x.span,
                            args: vec![from],
                            nullsafe: false,
                        }));
                        items.push((rest, slice));
                        continue;
                    }
                    items.push((x, index(Expr { kind: ExprKind::Int(i as i64), span: x.span })));
                }
            }
            ExprKind::Map(pairs) => {
                for (k, x) in pairs {
                    items.push((x, index(k.clone())));
                }
            }
            _ => unreachable!(),
        }
        if items.is_empty() {
            self.error("this pattern assigns nothing", pattern.span);
        }
        for (target, kind) in items {
            let value = Expr { kind, span: target.span };
            self.assign(target, AssignOp::Set, &value, out);
        }
    }

    /// Checks a loop's body from `entry` (the flow at the loop's start, after
    /// what the loop changes was forgotten). Returns the body and what is known
    /// where its `continue`s go (dead when there are none).
    fn loop_body(&mut self, body: &[Stmt], entry: &Flow) -> (Vec<HStmt>, Flow) {
        let cx = self.cx();
        cx.loop_depth += 1;
        let finally_depth = cx.finally_depth;
        let mut dead = Flow::default();
        dead.dead = true;
        cx.continues.push(ContinueTo { flow: dead, entry: entry.clone(), finally_depth });
        let mut out = Vec::new();
        self.block(body, &mut out);
        let cx = self.cx();
        cx.loop_depth -= 1;
        let to = cx.continues.pop().expect("loop");
        (out, to.flow)
    }

    /// Records what is known at a `continue` that goes to the loop `level`
    /// loops out. A `continue` that leaves a `try` runs its `finally` first,
    /// which may change anything the loop changes: the loop's entry is then
    /// joined in, which has forgotten those.
    fn note_continue(&mut self, level: u32) {
        let cx = self.cx();
        let n = cx.continues.len();
        if level == 0 || level as usize > n || cx.flow.dead {
            return;
        }
        let here = cx.flow.clone();
        let to = &mut cx.continues[n - level as usize];
        let here = if cx.finally_depth > to.finally_depth { Flow::merge(here, to.entry.clone()) } else { here };
        to.flow = Flow::merge(std::mem::take(&mut to.flow), here);
    }

    /// The flow where a `for` step or `do`-`while` condition starts: the end of
    /// the body joined with the paths that `continue`d.
    fn after_body(&mut self, continued: Flow) {
        let end = std::mem::take(&mut self.cx().flow);
        // When neither is reached, the step is never run: what the end knew is kept.
        let mut flow = if continued.dead { end } else { Flow::merge(end, continued) };
        flow.dead = false;
        self.cx().flow = flow;
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
    fn loop_var(&mut self, name: &str, ty: Type, span: Span) -> LocalId {
        let existing = self.cx().names.get(name).copied();
        let id = match existing {
            Some(id) if self.assignable_exact(&ty, &self.ctx.last().unwrap().locals[id as usize].ty) => id,
            _ => self.cx().declare(name, ty.clone(), span),
        };
        let declared = self.cx().locals[id as usize].ty.clone();
        self.set_local(id, &declared, ty);
        id
    }

    /// Marks a local as assigned with a value of type `value_ty`.
    pub(crate) fn set_local(&mut self, id: LocalId, declared: &Type, value_ty: Type) {
        let cx = self.cx();
        cx.flow.mark(id);
        cx.flow.prop_narrowed.remove_range((id, 0), (id, u32::MAX));
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
            let (hbody, _) = self.loop_body(body, &entry);
            self.cx().flow = entry;
            out.push(HStmt::ForRange { var, from: hf, to: ht, inclusive: *inclusive, body: hbody });
            return;
        }
        let hs = self.expr(subject, None);
        let generator = self.generator_args(&hs.ty);
        let (kt, vt) = match &hs.ty {
            Type::List(t) | Type::Set(t) => (Type::Int, (**t).clone()),
            Type::Map(k, v) => ((**k).clone(), (**v).clone()),
            Type::Error => (Type::Error, Type::Error),
            _ if generator.is_some() => generator.unwrap(),
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
        let (hbody, _) = self.loop_body(body, &entry);
        self.cx().flow = entry;
        out.push(HStmt::Foreach { subject: hs, key: k, val: v, body: hbody });
    }

    fn assign(&mut self, target: &Expr, op: AssignOp, value: &Expr, out: &mut Vec<HStmt>) {
        if matches!(target.kind, ExprKind::List(_) | ExprKind::Map(_)) {
            if op != AssignOp::Set {
                self.error("only `=` can assign to `[...]`", target.span);
                self.expr(value, None);
                return;
            }
            self.destructure(target, value, out);
            return;
        }
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
                        let id = self.cx().declare(name, ty, target.span);
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
