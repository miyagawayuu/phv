//! Compiles typed HIR into register bytecode.
//!
//! Locals live in fixed registers (captures, then parameters, then the rest);
//! temporaries are allocated above them like a stack.

use phenix_syntax::SourceMap;
use phenix_types::hir::*;
use phenix_types::types::Type;
use phenix_vm::bytecode::*;
use std::collections::HashMap;

pub fn compile(h: &HProgram, sources: &SourceMap) -> Program {
    let mut pc = ProgCtx {
        consts: Vec::new(),
        str_consts: HashMap::new(),
        builtins: Vec::new(),
        builtin_ids: HashMap::new(),
        methods: Vec::new(),
        method_ids: HashMap::new(),
    };
    let funcs = h.funcs.iter().map(|f| FnC::compile(&mut pc, f)).collect();
    for c in &h.classes {
        for (name, _) in &c.by_name {
            pc.method(name);
        }
    }
    let classes = h
        .classes
        .iter()
        .map(|c| Class {
            name: c.name.clone(),
            parent: c.parent,
            fields: c
                .fields
                .iter()
                .map(|f| Field { name: f.name.clone(), default: to_const(&f.default), public: f.public })
                .collect(),
            vtable: c.vtable.clone(),
            supers: c.supers.clone(),
            by_name: c.by_name.iter().map(|(n, f)| (pc.method_ids[n], *f)).collect(),
            cases: c.cases.iter().map(|(n, v)| (n.clone(), to_const(v))).collect(),
        })
        .collect();
    let files = sources
        .files
        .iter()
        .map(|f| FileInfo { name: f.name.clone(), line_starts: f.line_starts().to_vec() })
        .collect();
    Program {
        funcs,
        classes,
        consts: pc.consts,
        main: h.main,
        builtin_names: pc.builtins,
        method_names: pc.methods,
        files,
        exc_classes: h.exc_classes.clone(),
    }
}

fn to_const(c: &HConst) -> Const {
    match c {
        HConst::Null => Const::Null,
        HConst::Bool(b) => Const::Bool(*b),
        HConst::Int(i) => Const::Int(*i),
        HConst::Float(f) => Const::Float(*f),
        HConst::Str(s) => Const::Str(s.clone()),
        HConst::EmptyList => Const::EmptyList,
        HConst::EmptyMap => Const::EmptyMap,
    }
}

struct ProgCtx {
    consts: Vec<Const>,
    str_consts: HashMap<String, u32>,
    builtins: Vec<String>,
    builtin_ids: HashMap<String, u32>,
    methods: Vec<String>,
    method_ids: HashMap<String, u32>,
}

impl ProgCtx {
    fn str_const(&mut self, s: &str) -> u32 {
        if let Some(&i) = self.str_consts.get(s) {
            return i;
        }
        let i = self.consts.len() as u32;
        self.consts.push(Const::Str(s.to_string()));
        self.str_consts.insert(s.to_string(), i);
        i
    }

    fn constant(&mut self, c: Const) -> u32 {
        if let Const::Str(s) = &c {
            return self.str_const(s);
        }
        self.consts.push(c);
        self.consts.len() as u32 - 1
    }

    fn method(&mut self, name: &str) -> u32 {
        if let Some(&i) = self.method_ids.get(name) {
            return i;
        }
        let i = self.methods.len() as u32;
        self.methods.push(name.to_string());
        self.method_ids.insert(name.to_string(), i);
        i
    }

    fn builtin(&mut self, name: &str) -> u32 {
        if let Some(&i) = self.builtin_ids.get(name) {
            return i;
        }
        let i = self.builtins.len() as u32;
        self.builtins.push(name.to_string());
        self.builtin_ids.insert(name.to_string(), i);
        i
    }
}

struct LoopCtx {
    breaks: Vec<usize>,
    continues: Vec<usize>,
    /// `tries.len()` when the loop started.
    tries: usize,
}

/// An enclosing `try` statement while its body or catches are compiled.
struct TryCtx {
    /// Exception handlers of this statement that are currently installed.
    active: u32,
    finally: Option<Vec<HStmt>>,
}

struct FnC<'a> {
    pc: &'a mut ProgCtx,
    code: Vec<Instr>,
    offsets: Vec<u32>,
    reg_of: Vec<Reg>,
    num_locals: u16,
    next: u16,
    max: u16,
    loops: Vec<LoopCtx>,
    tries: Vec<TryCtx>,
    off: u32,
}

impl<'a> FnC<'a> {
    fn compile(pc: &'a mut ProgCtx, f: &HFunc) -> Function {
        // Register layout: captures, params, then the remaining locals.
        let mut reg_of = vec![Reg::MAX; f.locals.len()];
        let mut next: u16 = 0;
        for id in f.captures.iter().chain(f.params.iter()) {
            reg_of[*id as usize] = next;
            next += 1;
        }
        for r in reg_of.iter_mut() {
            if *r == Reg::MAX {
                *r = next;
                next += 1;
            }
        }
        let mut c = FnC {
            pc,
            code: Vec::new(),
            offsets: Vec::new(),
            reg_of,
            num_locals: next,
            next,
            max: next,
            loops: Vec::new(),
            tries: Vec::new(),
            off: f.span.start,
        };
        match f.kind {
            FuncKind::Template => {
                c.emit(Instr::PushOut);
                c.block(&f.body);
                let r = c.alloc();
                c.emit(Instr::PopOutHtml(r));
                c.emit(Instr::Ret(r));
            }
            _ => {
                c.block(&f.body);
                c.emit(Instr::RetNull);
            }
        }
        Function {
            native: native_sig(f),
            name: f.name.clone(),
            file: f.file,
            num_params: (f.captures.len() + f.params.len()) as u16,
            num_regs: c.max,
            code: c.code,
            offsets: c.offsets,
        }
    }

    // ----- helpers -------------------------------------------------------

    fn emit(&mut self, i: Instr) -> usize {
        self.code.push(i);
        self.offsets.push(self.off);
        self.code.len() - 1
    }

    fn here(&self) -> u32 {
        self.code.len() as u32
    }

    fn alloc(&mut self) -> Reg {
        self.alloc_n(1)
    }

    fn alloc_n(&mut self, n: usize) -> Reg {
        let r = self.next;
        self.next = self.next.checked_add(n as u16).expect("function needs too many registers");
        self.max = self.max.max(self.next);
        r
    }

    fn is_local(&self, r: Reg) -> bool {
        r < self.num_locals
    }

    fn reg(&self, id: u32) -> Reg {
        self.reg_of[id as usize]
    }

    fn patch(&mut self, at: usize, target: u32) {
        match &mut self.code[at] {
            Instr::Jump(t)
            | Instr::JumpIfFalse(_, t)
            | Instr::JumpIfTrue(_, t)
            | Instr::JumpIfNull(_, t)
            | Instr::JumpIfNotNull(_, t)
            | Instr::JumpIfNotLtI(_, _, t)
            | Instr::JumpIfNotLeI(_, _, t)
            | Instr::JumpIfNotLtIImm(_, _, t)
            | Instr::JumpIfNotLeIImm(_, _, t) => *t = target,
            Instr::IterNext { end, .. } => *end = target,
            Instr::TryBegin { catch, .. } => *catch = target,
            other => panic!("cannot patch {other:?}"),
        }
    }

    fn patch_all(&mut self, list: Vec<usize>, target: u32) {
        for at in list {
            self.patch(at, target);
        }
    }

    fn load_const(&mut self, dst: Reg, c: &HConst) {
        match c {
            HConst::Null => self.emit(Instr::Null(dst)),
            HConst::Bool(b) => self.emit(Instr::Bool(dst, *b)),
            HConst::Int(i) => self.emit(Instr::Int(dst, *i)),
            other => {
                let idx = self.pc.constant(to_const(other));
                self.emit(Instr::Const(dst, idx))
            }
        };
    }

    /// Returns a register holding `e`: the local itself for variables,
    /// otherwise a fresh temporary.
    fn operand(&mut self, e: &HExpr) -> Reg {
        if let HExprKind::Local(id) = e.kind {
            return self.reg(id);
        }
        let t = self.alloc();
        self.expr_into(e, t);
        t
    }

    fn args(&mut self, args: &[HExpr]) -> Reg {
        let start = self.alloc_n(args.len());
        for (i, a) in args.iter().enumerate() {
            self.expr_into(a, start + i as u16);
        }
        start
    }

    // ----- expressions ---------------------------------------------------

    fn expr_into(&mut self, e: &HExpr, dst: Reg) {
        let mark = self.next;
        // Expressions that write `dst` before they are done reading their
        // operands go through a temporary when `dst` is a variable.
        let multi = matches!(
            e.kind,
            HExprKind::And(..)
                | HExprKind::Or(..)
                | HExprKind::Coalesce(..)
                | HExprKind::Ternary(..)
                | HExprKind::NullSafe { .. }
                | HExprKind::Match { .. }
                | HExprKind::New(..)
        );
        let t = if multi && self.is_local(dst) { self.alloc() } else { dst };
        self.expr_raw(e, t);
        self.off = e.span.start;
        if t != dst {
            self.emit(Instr::Move(dst, t));
        }
        self.next = mark;
    }

    fn expr_raw(&mut self, e: &HExpr, dst: Reg) {
        let off = e.span.start;
        match &e.kind {
            HExprKind::Const(c) => {
                self.off = off;
                self.load_const(dst, c);
            }
            HExprKind::Local(id) => {
                let r = self.reg(*id);
                if r != dst {
                    self.off = off;
                    self.emit(Instr::Move(dst, r));
                }
            }
            HExprKind::Unary(k, x) => {
                let r = self.operand(x);
                self.off = off;
                self.emit(match k {
                    UnKind::NegI => Instr::NegI(dst, r),
                    UnKind::NegF => Instr::NegF(dst, r),
                    UnKind::Not => Instr::Not(dst, r),
                    UnKind::BitNot => Instr::BitNot(dst, r),
                });
            }
            HExprKind::Binary(k @ (BinKind::AddI | BinKind::SubI), a, b, false) if small_int(b).is_some() => {
                let ra = self.operand(a);
                let n = small_int(b).unwrap();
                self.off = off;
                self.emit(if *k == BinKind::AddI { Instr::AddIImm(dst, ra, n) } else { Instr::SubIImm(dst, ra, n) });
            }
            HExprKind::Binary(k, a, b, swap) => {
                let ra = self.operand(a);
                let rb = self.operand(b);
                let (x, y) = if *swap { (rb, ra) } else { (ra, rb) };
                self.off = off;
                self.emit(bin_instr(*k, dst, x, y));
            }
            HExprKind::And(a, b) | HExprKind::Or(a, b) => {
                let is_and = matches!(e.kind, HExprKind::And(..));
                self.expr_into(a, dst);
                let j =
                    if is_and { self.emit(Instr::JumpIfFalse(dst, 0)) } else { self.emit(Instr::JumpIfTrue(dst, 0)) };
                self.expr_into(b, dst);
                let end = self.here();
                self.patch(j, end);
            }
            HExprKind::Coalesce(a, b) => {
                self.expr_into(a, dst);
                let j = self.emit(Instr::JumpIfNotNull(dst, 0));
                self.expr_into(b, dst);
                let end = self.here();
                self.patch(j, end);
            }
            HExprKind::Ternary(c, t, f) => {
                let falses = self.cond_false(c);
                self.expr_into(t, dst);
                let j = self.emit(Instr::Jump(0));
                let else_at = self.here();
                self.patch_all(falses, else_at);
                self.expr_into(f, dst);
                let end = self.here();
                self.patch(j, end);
            }
            HExprKind::Concat(items, html) => {
                let start = self.args(items);
                self.off = off;
                self.emit(Instr::Concat(dst, start, items.len() as u16, *html));
            }
            HExprKind::IntToFloat(x) | HExprKind::FloatToInt(x) | HExprKind::ToStr(x) => {
                let r = self.operand(x);
                self.off = off;
                self.emit(match e.kind {
                    HExprKind::IntToFloat(_) => Instr::IntToFloat(dst, r),
                    HExprKind::FloatToInt(_) => Instr::FloatToInt(dst, r),
                    _ => Instr::ToStr(dst, r),
                });
            }
            HExprKind::List(items) => {
                let start = self.args(items);
                self.off = off;
                self.emit(Instr::NewList(dst, start, items.len() as u16));
            }
            HExprKind::Map(pairs) => {
                let start = self.alloc_n(pairs.len() * 2);
                for (i, (k, v)) in pairs.iter().enumerate() {
                    self.expr_into(k, start + 2 * i as u16);
                    self.expr_into(v, start + 2 * i as u16 + 1);
                }
                self.off = off;
                self.emit(Instr::NewMap(dst, start, pairs.len() as u16));
            }
            HExprKind::Range(a, b, incl) => {
                let ra = self.operand(a);
                let rb = self.operand(b);
                self.off = off;
                self.emit(Instr::Range(dst, ra, rb, *incl));
            }
            HExprKind::Index(b, k) | HExprKind::IndexOrNull(b, k) => {
                let rb = self.operand(b);
                let rk = self.operand(k);
                self.off = off;
                if matches!(e.kind, HExprKind::Index(..)) {
                    self.emit(Instr::Index(dst, rb, rk));
                } else {
                    self.emit(Instr::IndexOrNull(dst, rb, rk));
                }
            }
            HExprKind::Prop(o, slot) => {
                let ro = self.operand(o);
                self.off = off;
                self.emit(Instr::GetProp(dst, ro, *slot as u16));
            }
            HExprKind::NullSafe { tmp, obj, body } => {
                let t = self.reg(*tmp);
                self.expr_into(obj, t);
                let j = self.emit(Instr::JumpIfNull(t, 0));
                self.expr_into(body, dst);
                let j_end = self.emit(Instr::Jump(0));
                let null_at = self.here();
                self.patch(j, null_at);
                self.emit(Instr::Null(dst));
                let end = self.here();
                self.patch(j_end, end);
            }
            HExprKind::Call(fid, args) => {
                let start = self.args(args);
                self.off = off;
                self.emit(Instr::Call { dst, func: *fid, args: start, argc: args.len() as u16 });
            }
            HExprKind::CallVirtual(slot, args) => {
                let start = self.args(args);
                self.off = off;
                self.emit(Instr::CallVirtual { dst, slot: *slot, args: start, argc: args.len() as u16 });
            }
            HExprKind::CallBuiltin(name, args) => {
                let start = self.args(args);
                let id = self.pc.builtin(name);
                self.off = off;
                self.emit(Instr::CallBuiltin { dst, id, args: start, argc: args.len() as u16 });
            }
            HExprKind::CallNamed(name, args) => {
                let start = self.args(args);
                let id = self.pc.method(name);
                self.off = off;
                self.emit(Instr::CallNamed { dst, name: id, args: start, argc: args.len() as u16 });
            }
            HExprKind::EnumCase(class, idx) => {
                self.off = off;
                self.emit(Instr::EnumCase(dst, *class, *idx));
            }
            HExprKind::EnumCases(class) => {
                self.off = off;
                self.emit(Instr::EnumCases(dst, *class));
            }
            HExprKind::EnumFrom(class, v, try_) => {
                let r = self.operand(v);
                self.off = off;
                self.emit(Instr::EnumFrom { dst, class: *class, val: r, try_: *try_ });
            }
            HExprKind::CallClosure(callee, args) => {
                let rc = self.operand(callee);
                let start = self.args(args);
                self.off = off;
                self.emit(Instr::CallClosure { dst, callee: rc, args: start, argc: args.len() as u16 });
            }
            HExprKind::New(cid, ctor, args) => {
                self.off = off;
                self.emit(Instr::NewObject(dst, *cid));
                if let Some(ctor) = ctor {
                    let start = self.alloc_n(args.len() + 1);
                    self.emit(Instr::Move(start, dst));
                    for (i, a) in args.iter().enumerate() {
                        self.expr_into(a, start + 1 + i as u16);
                    }
                    let scratch = self.alloc();
                    self.off = off;
                    self.emit(Instr::Call { dst: scratch, func: *ctor, args: start, argc: args.len() as u16 + 1 });
                }
            }
            HExprKind::Closure(fid, caps) => {
                let start = self.args(caps);
                self.off = off;
                self.emit(Instr::MakeClosure { dst, func: *fid, caps: start, n: caps.len() as u16 });
            }
            HExprKind::Match { subject, arms, default } => {
                let s = subject.as_ref().map(|s| {
                    let t = self.alloc();
                    self.expr_into(s, t);
                    (t, s.ty.clone())
                });
                let mut arm_jumps: Vec<Vec<usize>> = Vec::new();
                for (conds, _) in arms {
                    let mut jumps = Vec::new();
                    for c in conds {
                        let mark = self.next;
                        match &s {
                            Some((sr, sty)) => {
                                let rc = self.operand(c);
                                let t = self.alloc();
                                let kind =
                                    if *sty == Type::Int && c.ty == Type::Int { BinKind::EqI } else { BinKind::Eq };
                                self.emit(bin_instr(kind, t, *sr, rc));
                                jumps.push(self.emit(Instr::JumpIfTrue(t, 0)));
                            }
                            None => {
                                let rc = self.operand(c);
                                jumps.push(self.emit(Instr::JumpIfTrue(rc, 0)));
                            }
                        }
                        self.next = mark;
                    }
                    arm_jumps.push(jumps);
                }
                let mut ends = Vec::new();
                match default {
                    Some(d) => {
                        self.expr_into(d, dst);
                        ends.push(self.emit(Instr::Jump(0)));
                    }
                    None => {
                        let r = match &s {
                            Some((sr, _)) => *sr,
                            None => {
                                let t = self.alloc();
                                self.emit(Instr::Bool(t, true));
                                t
                            }
                        };
                        self.off = off;
                        self.emit(Instr::MatchError(r));
                    }
                }
                for ((_, body), jumps) in arms.iter().zip(arm_jumps) {
                    let at = self.here();
                    self.patch_all(jumps, at);
                    self.expr_into(body, dst);
                    ends.push(self.emit(Instr::Jump(0)));
                }
                let end = self.here();
                self.patch_all(ends, end);
            }
            HExprKind::Is(x, test) => {
                let r = self.operand(x);
                self.off = off;
                self.emit(match test {
                    TestKind::Class(c) => Instr::InstanceOf(dst, r, *c),
                    TestKind::Null => Instr::IsType(dst, r, TypeTag::Null),
                    TestKind::Bool => Instr::IsType(dst, r, TypeTag::Bool),
                    TestKind::Int => Instr::IsType(dst, r, TypeTag::Int),
                    TestKind::Float => Instr::IsType(dst, r, TypeTag::Float),
                    TestKind::Str => Instr::IsType(dst, r, TypeTag::Str),
                    TestKind::Html => Instr::IsType(dst, r, TypeTag::Html),
                    TestKind::List => Instr::IsType(dst, r, TypeTag::List),
                    TestKind::Map => Instr::IsType(dst, r, TypeTag::Map),
                    TestKind::Func => Instr::IsType(dst, r, TypeTag::Closure),
                });
            }
            HExprKind::Throw(x) => {
                let r = self.operand(x);
                self.off = off;
                self.emit(Instr::Throw(r));
            }
        }
    }

    /// Emits code that jumps when `e` is false; returns the jumps to patch.
    fn cond_false(&mut self, e: &HExpr) -> Vec<usize> {
        let mark = self.next;
        let out = match &e.kind {
            HExprKind::And(a, b) => {
                let mut f = self.cond_false(a);
                f.extend(self.cond_false(b));
                f
            }
            HExprKind::Or(a, b) => {
                let t = self.cond_true(a);
                let f = self.cond_false(b);
                let at = self.here();
                self.patch_all(t, at);
                f
            }
            HExprKind::Unary(UnKind::Not, x) => self.cond_true(x),
            HExprKind::Const(HConst::Bool(true)) => Vec::new(),
            HExprKind::Binary(k @ (BinKind::LtI | BinKind::LeI), a, b, false) if small_int(b).is_some() => {
                let ra = self.operand(a);
                let n = small_int(b).unwrap();
                self.off = e.span.start;
                let i = if *k == BinKind::LtI {
                    Instr::JumpIfNotLtIImm(ra, n, 0)
                } else {
                    Instr::JumpIfNotLeIImm(ra, n, 0)
                };
                vec![self.emit(i)]
            }
            HExprKind::Binary(k @ (BinKind::LtI | BinKind::LeI), a, b, swap) => {
                let ra = self.operand(a);
                let rb = self.operand(b);
                let (x, y) = if *swap { (rb, ra) } else { (ra, rb) };
                self.off = e.span.start;
                let i = if *k == BinKind::LtI { Instr::JumpIfNotLtI(x, y, 0) } else { Instr::JumpIfNotLeI(x, y, 0) };
                vec![self.emit(i)]
            }
            _ => {
                let r = self.operand(e);
                vec![self.emit(Instr::JumpIfFalse(r, 0))]
            }
        };
        self.next = mark;
        out
    }

    /// Emits code that jumps when `e` is true; returns the jumps to patch.
    fn cond_true(&mut self, e: &HExpr) -> Vec<usize> {
        let mark = self.next;
        let out = match &e.kind {
            HExprKind::Or(a, b) => {
                let mut t = self.cond_true(a);
                t.extend(self.cond_true(b));
                t
            }
            HExprKind::And(a, b) => {
                let f = self.cond_false(a);
                let t = self.cond_true(b);
                let at = self.here();
                self.patch_all(f, at);
                t
            }
            HExprKind::Unary(UnKind::Not, x) => self.cond_false(x),
            _ => {
                let r = self.operand(e);
                vec![self.emit(Instr::JumpIfTrue(r, 0))]
            }
        };
        self.next = mark;
        out
    }

    // ----- statements ----------------------------------------------------

    fn block(&mut self, stmts: &[HStmt]) {
        for s in stmts {
            let mark = self.next;
            self.stmt(s);
            self.next = mark;
        }
    }

    /// Evaluates the object and keys of a place into registers.
    fn place_regs(&mut self, p: &HPlace) -> (PathBase, Vec<Reg>, Vec<Option<Const>>, bool) {
        let base = match &p.base {
            HPlaceBase::Local(id) => PathBase::Local(self.reg(*id)),
            HPlaceBase::Prop(obj, slot) => PathBase::Prop(self.operand(obj), *slot as u16),
        };
        let mut keys = Vec::new();
        let mut create = Vec::new();
        let mut push = false;
        for el in &p.path {
            match el {
                HPathElem::Index(k, c) => {
                    keys.push(self.operand(k));
                    create.push(c.as_ref().map(to_const));
                }
                HPathElem::Push => push = true,
            }
        }
        (base, keys, create, push)
    }

    fn store(&mut self, base: &PathBase, keys: Vec<Reg>, create: Vec<Option<Const>>, push: bool, val: Reg) {
        match (base, keys.is_empty() && !push) {
            (PathBase::Local(r), true) => {
                if *r != val {
                    self.emit(Instr::Move(*r, val));
                }
            }
            (PathBase::Prop(o, slot), true) => {
                self.emit(Instr::SetProp(*o, *slot, val));
            }
            _ => {
                self.emit(Instr::SetPath(Box::new(PathSet { base: base.clone(), keys, create, push, val })));
            }
        }
    }

    /// Reads the current value of a place (without `[]`) into a new register.
    fn load_place(&mut self, base: &PathBase, keys: &[Reg]) -> Reg {
        let cur = self.alloc();
        match base {
            PathBase::Local(r) => {
                self.emit(Instr::Move(cur, *r));
            }
            PathBase::Prop(o, slot) => {
                self.emit(Instr::GetProp(cur, *o, *slot));
            }
        }
        for k in keys {
            self.emit(Instr::Index(cur, cur, *k));
        }
        cur
    }

    fn stmt(&mut self, s: &HStmt) {
        match s {
            HStmt::Expr(e) => {
                let t = self.alloc();
                self.expr_into(e, t);
            }
            HStmt::Echo(e) => {
                let r = self.operand(e);
                self.emit(Instr::Echo(r));
            }
            HStmt::Html(text, scripts) => {
                // `<script` tags get the request's CSP nonce: `<script nonce="...">`.
                let mut from = 0;
                for &at in scripts {
                    let c = self.pc.str_const(&text[from..at]);
                    self.emit(Instr::EchoHtml(c));
                    self.emit(Instr::EchoNonce);
                    from = at;
                }
                if from < text.len() || scripts.is_empty() {
                    let c = self.pc.str_const(&text[from..]);
                    self.emit(Instr::EchoHtml(c));
                }
            }
            HStmt::Assign(p, value) => {
                if let (HPlaceBase::Local(id), true) = (&p.base, p.path.is_empty()) {
                    let r = self.reg(*id);
                    self.expr_into(value, r);
                    return;
                }
                let (base, keys, create, push) = self.place_regs(p);
                let v = self.operand(value);
                self.off = value.span.start;
                self.store(&base, keys, create, push, v);
            }
            HStmt::Compound(p, kind, rhs) => {
                if let (HPlaceBase::Local(id), true) = (&p.base, p.path.is_empty()) {
                    let r = self.reg(*id);
                    self.off = rhs.span.start;
                    match (kind, &rhs.kind) {
                        (BinKind::AddI, HExprKind::Const(HConst::Int(n))) => {
                            self.emit(Instr::IncI(r, *n));
                        }
                        (BinKind::SubI, HExprKind::Const(HConst::Int(n))) if *n != i64::MIN => {
                            self.emit(Instr::IncI(r, -*n));
                        }
                        _ => {
                            let rr = self.operand(rhs);
                            self.off = rhs.span.start;
                            self.emit(bin_instr(*kind, r, r, rr));
                        }
                    }
                    return;
                }
                let (base, keys, create, push) = self.place_regs(p);
                let cur = self.load_place(&base, &keys);
                let rr = self.operand(rhs);
                self.off = rhs.span.start;
                if *kind == BinKind::Concat {
                    let start = self.alloc_n(2);
                    self.emit(Instr::Move(start, cur));
                    self.emit(Instr::Move(start + 1, rr));
                    self.emit(Instr::Concat(cur, start, 2, false));
                } else {
                    self.emit(bin_instr(*kind, cur, cur, rr));
                }
                self.store(&base, keys, create, push, cur);
            }
            HStmt::CoalesceAssign(p, rhs) => {
                let (base, keys, create, push) = self.place_regs(p);
                let cur = self.load_place(&base, &keys);
                let j = self.emit(Instr::JumpIfNotNull(cur, 0));
                self.expr_into(rhs, cur);
                self.store(&base, keys, create, push, cur);
                let end = self.here();
                self.patch(j, end);
            }
            HStmt::Append(id, rhs) => {
                let r = self.operand(rhs);
                let dst = self.reg(*id);
                self.emit(Instr::AppendStr(dst, r));
            }
            HStmt::If(c, then, els) => {
                let falses = self.cond_false(c);
                self.block(then);
                if els.is_empty() {
                    let end = self.here();
                    self.patch_all(falses, end);
                } else {
                    let j = self.emit(Instr::Jump(0));
                    let else_at = self.here();
                    self.patch_all(falses, else_at);
                    self.block(els);
                    let end = self.here();
                    self.patch(j, end);
                }
            }
            HStmt::Loop { cond, body, step, do_while } => {
                let top = self.here();
                self.loops.push(LoopCtx { breaks: Vec::new(), continues: Vec::new(), tries: self.tries.len() });
                if *do_while {
                    self.block(body);
                    let cont = self.here();
                    let exits = cond.as_ref().map(|c| self.cond_false(c)).unwrap_or_default();
                    self.emit(Instr::Jump(top));
                    let end = self.here();
                    let lp = self.loops.pop().unwrap();
                    self.patch_all(exits, end);
                    self.patch_all(lp.breaks, end);
                    self.patch_all(lp.continues, cont);
                } else {
                    let exits = cond.as_ref().map(|c| self.cond_false(c)).unwrap_or_default();
                    self.block(body);
                    let cont = self.here();
                    self.block(step);
                    self.emit(Instr::Jump(top));
                    let end = self.here();
                    let lp = self.loops.pop().unwrap();
                    self.patch_all(exits, end);
                    self.patch_all(lp.breaks, end);
                    self.patch_all(lp.continues, cont);
                }
            }
            HStmt::ForRange { var, from, to, inclusive, body } => {
                let v = self.reg(*var);
                self.expr_into(from, v);
                let end_reg = self.alloc();
                self.expr_into(to, end_reg);
                let top = self.here();
                let exit = if *inclusive {
                    self.emit(Instr::JumpIfNotLeI(v, end_reg, 0))
                } else {
                    self.emit(Instr::JumpIfNotLtI(v, end_reg, 0))
                };
                self.loops.push(LoopCtx { breaks: Vec::new(), continues: Vec::new(), tries: self.tries.len() });
                self.block(body);
                let cont = self.here();
                self.emit(Instr::IncI(v, 1));
                self.emit(Instr::Jump(top));
                let end = self.here();
                let lp = self.loops.pop().unwrap();
                self.patch(exit, end);
                self.patch_all(lp.breaks, end);
                self.patch_all(lp.continues, cont);
            }
            HStmt::Foreach { subject, key, val, body } => {
                let it = self.alloc_n(2);
                self.expr_into(subject, it);
                self.off = subject.span.start;
                self.emit(Instr::IterInit(it, it));
                let top = self.here();
                let key = key.map(|k| self.reg(k)).unwrap_or(NO_REG);
                let val = self.reg(*val);
                let next = self.emit(Instr::IterNext { it, key, val, end: 0 });
                self.loops.push(LoopCtx { breaks: Vec::new(), continues: Vec::new(), tries: self.tries.len() });
                self.block(body);
                self.emit(Instr::Jump(top));
                let end = self.here();
                let lp = self.loops.pop().unwrap();
                self.patch(next, end);
                self.patch_all(lp.breaks, end);
                self.patch_all(lp.continues, top);
            }
            HStmt::Break(level) | HStmt::Continue(level) => {
                let is_break = matches!(s, HStmt::Break(_));
                let idx = self.loops.len() - *level as usize;
                let down_to = self.loops[idx].tries;
                self.unwind_tries(down_to);
                let j = self.emit(Instr::Jump(0));
                if is_break {
                    self.loops[idx].breaks.push(j);
                } else {
                    self.loops[idx].continues.push(j);
                }
            }
            HStmt::Return(value, span) => {
                self.off = span.start;
                match value {
                    Some(e) if !self.tries.is_empty() => {
                        // The value is computed before `finally` blocks run.
                        let t = self.alloc();
                        self.expr_into(e, t);
                        self.unwind_tries(0);
                        self.off = span.start;
                        self.emit(Instr::Ret(t));
                    }
                    Some(e) => {
                        let r = self.operand(e);
                        self.emit(Instr::Ret(r));
                    }
                    None => {
                        self.unwind_tries(0);
                        self.off = span.start;
                        self.emit(Instr::RetNull);
                    }
                }
            }
            HStmt::Throw(e) => {
                let r = self.operand(e);
                self.off = e.span.start;
                self.emit(Instr::Throw(r));
            }
            HStmt::Try { body, catches, finally } => {
                let has_catch = !catches.is_empty();
                // Layout: [TryBegin finally] [TryBegin catch] body ...
                let (exc_f, begin_f) = match finally {
                    Some(_) => {
                        let r = self.alloc();
                        (r, Some(self.emit(Instr::TryBegin { catch: 0, exc: r })))
                    }
                    None => (0, None),
                };
                let exc = self.alloc();
                let begin_c = if has_catch { Some(self.emit(Instr::TryBegin { catch: 0, exc })) } else { None };
                self.tries
                    .push(TryCtx { active: has_catch as u32 + finally.is_some() as u32, finally: finally.clone() });
                self.block(body);
                let mut ends = Vec::new();
                if let Some(begin) = begin_c {
                    self.emit(Instr::TryEnd);
                    self.tries.last_mut().unwrap().active -= 1;
                    ends.push(self.emit(Instr::Jump(0)));
                    let catch_at = self.here();
                    self.patch(begin, catch_at);
                    for c in catches {
                        let t = self.alloc();
                        let mut to_body = Vec::new();
                        for class in &c.classes {
                            self.emit(Instr::InstanceOf(t, exc, *class));
                            to_body.push(self.emit(Instr::JumpIfTrue(t, 0)));
                        }
                        let skip = self.emit(Instr::Jump(0));
                        let body_at = self.here();
                        self.patch_all(to_body, body_at);
                        if let Some(v) = c.var {
                            let r = self.reg(v);
                            self.emit(Instr::Move(r, exc));
                        }
                        self.block(&c.body);
                        ends.push(self.emit(Instr::Jump(0)));
                        let next = self.here();
                        self.patch(skip, next);
                    }
                    // No catch matched: rethrow (into the `finally` handler, if any).
                    self.emit(Instr::Throw(exc));
                }
                let after = self.here();
                self.patch_all(ends, after);
                self.tries.pop();
                if let (Some(begin), Some(f)) = (begin_f, finally) {
                    self.emit(Instr::TryEnd);
                    self.block(f);
                    let j = self.emit(Instr::Jump(0));
                    let handler = self.here();
                    self.patch(begin, handler);
                    self.block(f);
                    self.emit(Instr::Throw(exc_f));
                    let end = self.here();
                    self.patch(j, end);
                }
            }
        }
    }

    /// Before leaving enclosing `try` statements with `return`, `break` or
    /// `continue`: removes their handlers and runs their `finally` blocks.
    fn unwind_tries(&mut self, down_to: usize) {
        let mut i = self.tries.len();
        while i > down_to {
            i -= 1;
            for _ in 0..self.tries[i].active {
                self.emit(Instr::TryEnd);
            }
            if let Some(f) = self.tries[i].finally.clone() {
                let outer = self.tries.split_off(i);
                self.block(&f);
                self.tries.extend(outer);
            }
        }
    }
}

/// A function whose every local is an int or bool may be compiled natively.
fn native_sig(f: &HFunc) -> Option<NativeSig> {
    let kind = |t: &Type| match t {
        Type::Int => Some(ValKind::Int),
        Type::Bool => Some(ValKind::Bool),
        _ => None,
    };
    if f.kind != FuncKind::Normal || !f.captures.is_empty() || f.params.len() > 16 {
        return None;
    }
    if !f.locals.iter().all(|l| kind(&l.ty).is_some()) {
        return None;
    }
    let params = f.params.iter().map(|p| kind(&f.locals[*p as usize].ty)).collect::<Option<Vec<_>>>()?;
    let ret = match &f.ret {
        Type::Void => ValKind::Void,
        t => kind(t)?,
    };
    Some(NativeSig { params, ret })
}

/// An int constant that fits an immediate operand.
fn small_int(e: &HExpr) -> Option<i32> {
    match e.kind {
        HExprKind::Const(HConst::Int(n)) => i32::try_from(n).ok(),
        _ => None,
    }
}

fn bin_instr(k: BinKind, d: Reg, a: Reg, b: Reg) -> Instr {
    match k {
        BinKind::AddI => Instr::AddI(d, a, b),
        BinKind::SubI => Instr::SubI(d, a, b),
        BinKind::MulI => Instr::MulI(d, a, b),
        BinKind::ModI => Instr::ModI(d, a, b),
        BinKind::PowI => Instr::PowI(d, a, b),
        BinKind::AddF => Instr::AddF(d, a, b),
        BinKind::SubF => Instr::SubF(d, a, b),
        BinKind::MulF => Instr::MulF(d, a, b),
        BinKind::DivF => Instr::DivF(d, a, b),
        BinKind::ModF => Instr::ModF(d, a, b),
        BinKind::PowF => Instr::PowF(d, a, b),
        BinKind::BitAnd => Instr::BitAnd(d, a, b),
        BinKind::BitOr => Instr::BitOr(d, a, b),
        BinKind::BitXor => Instr::BitXor(d, a, b),
        BinKind::Shl => Instr::Shl(d, a, b),
        BinKind::Shr => Instr::Shr(d, a, b),
        BinKind::EqI => Instr::EqI(d, a, b),
        BinKind::Eq => Instr::Eq(d, a, b),
        BinKind::Ne => Instr::Ne(d, a, b),
        BinKind::LtI => Instr::LtI(d, a, b),
        BinKind::LeI => Instr::LeI(d, a, b),
        BinKind::Lt => Instr::Lt(d, a, b),
        BinKind::Le => Instr::Le(d, a, b),
        BinKind::Cmp => Instr::Cmp(d, a, b),
        BinKind::Concat => Instr::Concat(d, a, 2, false),
    }
}
