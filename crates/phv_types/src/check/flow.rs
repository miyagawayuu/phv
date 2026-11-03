//! Per-function state: locals, definite assignment and type narrowing.

use super::Checker;
use crate::hir::{FuncKind, HLocal};
use crate::types::{ClassId, LocalId, Type};
use phv_syntax::ast::{Stmt, StmtKind};
use phv_syntax::Span;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Default)]
pub(crate) struct Flow {
    /// Definitely assigned at this point.
    pub assigned: Vec<bool>,
    /// Narrower types proven by conditions or assignments.
    pub narrowed: HashMap<LocalId, Type>,
    /// Narrowed property types (`$x->p != null`), keyed by local and field
    /// slot. Cleared by any call or property assignment.
    pub prop_narrowed: HashMap<(LocalId, u32), Type>,
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
        let mut prop_narrowed = HashMap::new();
        for (k, ta) in &a.prop_narrowed {
            if let Some(tb) = b.prop_narrowed.get(k) {
                prop_narrowed.insert(*k, Type::union(vec![ta.clone(), tb.clone()]));
            }
        }
        Flow { assigned, narrowed, prop_narrowed, dead: false }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum FactKey {
    Local(LocalId),
    Prop(LocalId, u32),
}

pub(crate) type Facts = Vec<(FactKey, Type)>;

/// What `FnCtx::apply` replaced, for `restore`.
pub(crate) struct Saved {
    entries: Vec<(FactKey, Option<Type>)>,
    gen: u64,
}

pub(crate) struct FnCtx {
    pub locals: Vec<HLocal>,
    /// Where each local is first assigned or declared.
    pub spans: Vec<Span>,
    /// Whether each local is ever read (see `unused_locals`).
    pub used: Vec<bool>,
    /// Locals bound by `catch`, which can be left out instead of renamed.
    pub catch_vars: HashSet<LocalId>,
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
    /// Incremented whenever property narrowings are cleared (by a call).
    pub prop_gen: u64,
    temps: u32,
}

impl FnCtx {
    pub fn new(ret: Type, class: Option<ClassId>, kind: FuncKind) -> FnCtx {
        FnCtx {
            locals: Vec::new(),
            spans: Vec::new(),
            used: Vec::new(),
            catch_vars: HashSet::new(),
            names: HashMap::new(),
            flow: Flow::default(),
            ret,
            class,
            kind,
            auto_capture: false,
            captures: Vec::new(),
            loop_depth: 0,
            prop_gen: 0,
            temps: 0,
        }
    }

    pub fn declare(&mut self, name: &str, ty: Type, span: Span) -> LocalId {
        let id = self.locals.len() as LocalId;
        self.locals.push(HLocal { name: name.to_string(), ty });
        self.spans.push(span);
        self.used.push(false);
        self.names.insert(name.to_string(), id);
        id
    }

    /// Locals that are never read, except `$this`, names starting with `_`
    /// and those in `skip` (parameters whose signature is fixed elsewhere).
    pub fn unused_locals(&self, skip: &[LocalId]) -> Vec<LocalId> {
        (0..self.locals.len() as LocalId)
            .filter(|id| {
                let name = &self.locals[*id as usize].name;
                !self.used[*id as usize]
                    && !skip.contains(id)
                    && name != "this"
                    && !name.starts_with('_')
                    && !name.starts_with('%')
            })
            .collect()
    }

    /// A hidden local used by the compiler (e.g. the subject of `?->`).
    pub fn temp(&mut self, ty: Type) -> LocalId {
        let id = self.locals.len() as LocalId;
        self.temps += 1;
        self.locals.push(HLocal { name: format!("%t{}", self.temps), ty });
        self.spans.push(Span::default());
        self.used.push(true);
        self.flow.mark(id);
        id
    }

    pub fn current_type(&self, id: LocalId) -> Type {
        self.flow.narrowed.get(&id).cloned().unwrap_or_else(|| self.locals[id as usize].ty.clone())
    }

    pub fn apply(&mut self, facts: &Facts) -> Saved {
        let mut saved = Vec::new();
        for (key, ty) in facts {
            let old = match key {
                FactKey::Local(id) => self.flow.narrowed.insert(*id, ty.clone()),
                FactKey::Prop(id, slot) => self.flow.prop_narrowed.insert((*id, *slot), ty.clone()),
            };
            saved.push((*key, old));
        }
        Saved { entries: saved, gen: self.prop_gen }
    }

    /// Undoes `apply`. Returns true if a call happened in between; property
    /// narrowings are then dropped instead of restored.
    pub fn restore(&mut self, saved: Saved) -> bool {
        let calls = saved.gen != self.prop_gen;
        if calls {
            self.flow.prop_narrowed.clear();
        }
        for (key, ty) in saved.entries.into_iter().rev() {
            if calls && matches!(key, FactKey::Prop(..)) {
                continue;
            }
            match (key, ty) {
                (FactKey::Local(id), Some(t)) => {
                    self.flow.narrowed.insert(id, t);
                }
                (FactKey::Local(id), None) => {
                    self.flow.narrowed.remove(&id);
                }
                (FactKey::Prop(id, s), Some(t)) => {
                    self.flow.prop_narrowed.insert((id, s), t);
                }
                (FactKey::Prop(id, s), None) => {
                    self.flow.prop_narrowed.remove(&(id, s));
                }
            }
        }
        calls
    }

    /// Forgets narrowings of variables a loop or `try` body may reassign.
    pub fn invalidate(&mut self, names: &HashSet<String>) {
        self.flow.prop_narrowed.clear();
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

fn base_var(e: &phv_syntax::ast::Expr) -> Option<String> {
    use phv_syntax::ast::ExprKind;
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
        let found = self.read_var_at(level, name, span, true);
        if let Some((id, _)) = found {
            let def = self.ctx[level].spans[id as usize];
            self.note_ref(span, def);
        }
        found
    }

    fn read_var_at(&mut self, level: usize, name: &str, span: Span, report: bool) -> Option<(LocalId, Type)> {
        let ctx = &mut self.ctx[level];
        if let Some(&id) = ctx.names.get(name) {
            ctx.used[id as usize] = true;
            if !ctx.flow.is_assigned(id) {
                if report {
                    self.error(format!("variable `${name}` might not be defined here"), span).add_note(
                        "it is only assigned on some paths; assign it before the `if`/loop or in every branch",
                    );
                }
                return None;
            }
            return Some((id, ctx.current_type(id)));
        }
        if (ctx.auto_capture || name == "this") && level > 0 {
            if let Some((outer, ty)) = self.read_var_at(level - 1, name, span, false) {
                let def = self.ctx[level - 1].spans[outer as usize];
                let ctx = &mut self.ctx[level];
                let id = ctx.declare(name, ty.clone(), def);
                ctx.used[id as usize] = true;
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
