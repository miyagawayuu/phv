//! Per-function state: locals, definite assignment and type narrowing.

use super::Checker;
use crate::hir::{FuncKind, HLocal};
use crate::types::{ClassId, LocalId, Type};
use phenix_syntax::ast::{Stmt, StmtKind};
use phenix_syntax::Span;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Default)]
pub(crate) struct Flow {
    /// Definitely assigned at this point.
    pub assigned: Vec<bool>,
    /// Narrower types proven by conditions or assignments.
    pub narrowed: HashMap<LocalId, Type>,
    /// This point is unreachable (after return/throw/break/continue).
    pub dead: bool,
}

impl Flow {
    pub fn is_assigned(&self, id: LocalId) -> bool {
        self.assigned.get(id as usize).copied().unwrap_or(false)
    }

    pub fn mark(&mut self, id: LocalId) {
        let i = id as usize;
        if self.assigned.len() <= i {
            self.assigned.resize(i + 1, false);
        }
        self.assigned[i] = true;
    }

    /// Joins two control-flow paths.
    pub fn merge(a: Flow, b: Flow) -> Flow {
        if a.dead {
            return b;
        }
        if b.dead {
            return a;
        }
        let len = a.assigned.len().max(b.assigned.len());
        let assigned = (0..len).map(|i| a.is_assigned(i as LocalId) && b.is_assigned(i as LocalId)).collect();
        let mut narrowed = HashMap::new();
        for (id, ta) in &a.narrowed {
            if let Some(tb) = b.narrowed.get(id) {
                narrowed.insert(*id, Type::union(vec![ta.clone(), tb.clone()]));
            }
        }
        Flow { assigned, narrowed, dead: false }
    }
}

pub(crate) type Facts = Vec<(LocalId, Type)>;

pub(crate) struct FnCtx {
    pub locals: Vec<HLocal>,
    pub names: HashMap<String, LocalId>,
    pub flow: Flow,
    pub ret: Type,
    pub class: Option<ClassId>,
    pub kind: FuncKind,
    /// Arrow functions capture variables from the enclosing function automatically.
    pub auto_capture: bool,
    /// (inner local, outer local)
    pub captures: Vec<(LocalId, LocalId)>,
    pub loop_depth: u32,
    temps: u32,
}

impl FnCtx {
    pub fn new(ret: Type, class: Option<ClassId>, kind: FuncKind) -> FnCtx {
        FnCtx {
            locals: Vec::new(),
            names: HashMap::new(),
            flow: Flow::default(),
            ret,
            class,
            kind,
            auto_capture: false,
            captures: Vec::new(),
            loop_depth: 0,
            temps: 0,
        }
    }

    pub fn declare(&mut self, name: &str, ty: Type) -> LocalId {
        let id = self.locals.len() as LocalId;
        self.locals.push(HLocal { name: name.to_string(), ty });
        self.names.insert(name.to_string(), id);
        id
    }

    /// A hidden local used by the compiler (e.g. the subject of `?->`).
    pub fn temp(&mut self, ty: Type) -> LocalId {
        let id = self.locals.len() as LocalId;
        self.temps += 1;
        self.locals.push(HLocal { name: format!("%t{}", self.temps), ty });
        self.flow.mark(id);
        id
    }

    pub fn current_type(&self, id: LocalId) -> Type {
        self.flow.narrowed.get(&id).cloned().unwrap_or_else(|| self.locals[id as usize].ty.clone())
    }

    pub fn apply(&mut self, facts: &Facts) -> Vec<(LocalId, Option<Type>)> {
        let mut saved = Vec::new();
        for (id, ty) in facts {
            saved.push((*id, self.flow.narrowed.get(id).cloned()));
            self.flow.narrowed.insert(*id, ty.clone());
        }
        saved
    }

    pub fn restore(&mut self, saved: Vec<(LocalId, Option<Type>)>) {
        for (id, ty) in saved.into_iter().rev() {
            match ty {
                Some(t) => self.flow.narrowed.insert(id, t),
                None => self.flow.narrowed.remove(&id),
            };
        }
    }

    /// Forgets narrowings of variables a loop or `try` body may reassign.
    pub fn invalidate(&mut self, names: &HashSet<String>) {
        for n in names {
            if let Some(id) = self.names.get(n) {
                self.flow.narrowed.remove(id);
            }
        }
    }
}

/// Names of variables assigned anywhere in `stmts` (not inside closures).
pub(crate) fn assigned_vars(stmts: &[Stmt], out: &mut HashSet<String>) {
    for s in stmts {
        match &s.kind {
            StmtKind::Assign { target, .. } | StmtKind::IncDec { target, .. } => {
                if let Some(n) = base_var(target) {
                    out.insert(n);
                }
            }
            StmtKind::TypedDecl { name, .. } => {
                out.insert(name.clone());
            }
            StmtKind::If { then, elifs, els, .. } => {
                assigned_vars(then, out);
                for (_, b) in elifs {
                    assigned_vars(b, out);
                }
                if let Some(b) = els {
                    assigned_vars(b, out);
                }
            }
            StmtKind::While { body, .. } | StmtKind::DoWhile { body, .. } | StmtKind::Block(body) => {
                assigned_vars(body, out)
            }
            StmtKind::For { init, step, body, .. } => {
                assigned_vars(init, out);
                assigned_vars(step, out);
                assigned_vars(body, out);
            }
            StmtKind::Foreach { key, value, body, .. } => {
                if let Some((k, _)) = key {
                    out.insert(k.clone());
                }
                out.insert(value.0.clone());
                assigned_vars(body, out);
            }
            StmtKind::Try { body, catches, finally } => {
                assigned_vars(body, out);
                for c in catches {
                    if let Some((v, _)) = &c.var {
                        out.insert(v.clone());
                    }
                    assigned_vars(&c.body, out);
                }
                if let Some(f) = finally {
                    assigned_vars(f, out);
                }
            }
            _ => {}
        }
    }
}

fn base_var(e: &phenix_syntax::ast::Expr) -> Option<String> {
    use phenix_syntax::ast::ExprKind;
    match &e.kind {
        ExprKind::Var(n) => Some(n.clone()),
        ExprKind::Index { base, .. } => base_var(base),
        _ => None,
    }
}

impl Checker<'_> {
    pub(crate) fn cx(&mut self) -> &mut FnCtx {
        self.ctx.last_mut().unwrap()
    }

    /// Reads a variable, capturing it from enclosing functions for arrow functions.
    pub(crate) fn read_var(&mut self, name: &str, span: Span) -> Option<(LocalId, Type)> {
        let level = self.ctx.len() - 1;
        self.read_var_at(level, name, span, true)
    }

    fn read_var_at(&mut self, level: usize, name: &str, span: Span, report: bool) -> Option<(LocalId, Type)> {
        let ctx = &self.ctx[level];
        if let Some(&id) = ctx.names.get(name) {
            if !ctx.flow.is_assigned(id) {
                if report {
                    self.error(format!("variable `${name}` might not be defined here"), span)
                        .add_note("it is only assigned on some paths; assign it before the `if`/loop or in every branch");
                }
                return None;
            }
            return Some((id, ctx.current_type(id)));
        }
        if (ctx.auto_capture || name == "this") && level > 0 {
            if let Some((outer, ty)) = self.read_var_at(level - 1, name, span, false) {
                let ctx = &mut self.ctx[level];
                let id = ctx.declare(name, ty.clone());
                ctx.flow.mark(id);
                ctx.captures.push((id, outer));
                return Some((id, ty));
            }
        }
        if report {
            if name == "this" {
                self.error("`$this` is only available inside non-static methods", span);
            } else {
                self.error(format!("undefined variable `${name}`"), span);
            }
        }
        None
    }
}
