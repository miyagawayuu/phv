//! Queries over a checked program, for editor support.

use crate::hir::*;
use phv_syntax::Span;

struct Finder<'a> {
    file: u32,
    offset: u32,
    best: Option<&'a HExpr>,
}

impl<'a> Finder<'a> {
    fn expr(&mut self, e: &'a HExpr) {
        let s = e.span;
        if s.file != self.file || s.start > self.offset || s.end < self.offset {
            // Children can still be inside (spans of desugared nodes may be wider).
        } else if self.best.is_none_or(|b| (s.end - s.start) <= (b.span.end - b.span.start)) {
            self.best = Some(e);
        }
        use HExprKind::*;
        match &e.kind {
            Const(_) | Local(_) | EnumCase(..) | EnumCases(_) => {}
            Unary(_, x)
            | IntToFloat(x)
            | FloatToInt(x)
            | ToStr(x)
            | Prop(x, _)
            | Is(x, _)
            | Throw(x)
            | EnumFrom(_, x, _) => self.expr(x),
            Binary(_, a, b, _)
            | And(a, b)
            | Or(a, b)
            | Coalesce(a, b)
            | Index(a, b)
            | IndexOrNull(a, b)
            | Range(a, b, _) => {
                self.expr(a);
                self.expr(b);
            }
            Ternary(a, b, c) => {
                self.expr(a);
                self.expr(b);
                self.expr(c);
            }
            Concat(xs, _)
            | List(xs)
            | Call(_, xs)
            | CallVirtual(_, xs)
            | CallBuiltin(_, xs)
            | CallNamed(_, xs)
            | New(_, _, xs)
            | Closure(_, xs) => {
                for x in xs {
                    self.expr(x);
                }
            }
            Map(ps) => {
                for (k, v) in ps {
                    self.expr(k);
                    self.expr(v);
                }
            }
            NullSafe { obj, body, .. } => {
                self.expr(obj);
                self.expr(body);
            }
            CallClosure(c, xs) => {
                self.expr(c);
                for x in xs {
                    self.expr(x);
                }
            }
            Match { subject, arms, default } => {
                if let Some(s) = subject {
                    self.expr(s);
                }
                for (cs, b) in arms {
                    for c in cs {
                        self.expr(c);
                    }
                    self.expr(b);
                }
                if let Some(d) = default {
                    self.expr(d);
                }
            }
        }
    }

    fn place(&mut self, p: &'a HPlace) {
        if let HPlaceBase::Prop(o, _) = &p.base {
            self.expr(o);
        }
        for el in &p.path {
            if let HPathElem::Index(k, _) = el {
                self.expr(k);
            }
        }
    }

    fn stmts(&mut self, ss: &'a [HStmt]) {
        for s in ss {
            self.stmt(s);
        }
    }

    fn stmt(&mut self, s: &'a HStmt) {
        match s {
            HStmt::Expr(e) | HStmt::Echo(e) | HStmt::EchoAttr(e) | HStmt::Throw(e) => self.expr(e),
            HStmt::Html(..) | HStmt::Break(_) | HStmt::Continue(_) => {}
            HStmt::Guarded(b) => self.stmts(b),
            HStmt::Assign(p, e) | HStmt::Compound(p, _, e) | HStmt::CoalesceAssign(p, e) => {
                self.place(p);
                self.expr(e);
            }
            HStmt::Append(_, e) => self.expr(e),
            HStmt::If(c, a, b) => {
                self.expr(c);
                self.stmts(a);
                self.stmts(b);
            }
            HStmt::Loop { cond, body, step, .. } => {
                if let Some(c) = cond {
                    self.expr(c);
                }
                self.stmts(body);
                self.stmts(step);
            }
            HStmt::ForRange { from, to, body, .. } => {
                self.expr(from);
                self.expr(to);
                self.stmts(body);
            }
            HStmt::Foreach { subject, body, .. } => {
                self.expr(subject);
                self.stmts(body);
            }
            HStmt::Return(e, _) => {
                if let Some(e) = e {
                    self.expr(e);
                }
            }
            HStmt::Try { body, catches, finally } => {
                self.stmts(body);
                for c in catches {
                    self.stmts(&c.body);
                }
                if let Some(f) = finally {
                    self.stmts(f);
                }
            }
        }
    }
}

/// The innermost expression at `offset` in `file`: its span and type.
pub fn type_at(prog: &HProgram, file: u32, offset: u32) -> Option<(Span, String)> {
    let mut f = Finder { file, offset, best: None };
    for func in &prog.funcs {
        f.stmts(&func.body);
    }
    let names: Vec<String> = prog.classes.iter().map(|c| c.name.clone()).collect();
    f.best.map(|e| (e.span, e.ty.display(&names).to_string()))
}
