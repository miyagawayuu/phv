//! A Cranelift JIT for functions that only compute with `int` and `bool`
//! (the compiler marks them with a `NativeSig`). Everything else stays in
//! the interpreter.
//!
//! Native code keeps the interpreter's guarantees: integer overflow, modulo
//! by zero and negative shifts raise the same exceptions, recursion depth is
//! limited, and a budget counter at calls and loop back-edges lets the time
//! and memory limits interrupt long-running code.

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{types, AbiParam, Block, InstBuilder, MemFlagsData, Signature, Value};
use cranelift_codegen::isa::TargetFrontendConfig;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{default_libcall_names, FuncId, Linkage, Module};
use phx_vm::bytecode::{Function, Instr, Program};
use phx_vm::vm::{phx_jit_tick, JIT_ERR_DEPTH, JIT_ERR_DIV_ZERO, JIT_ERR_OVERFLOW, JIT_ERR_SHIFT};
use phx_vm::{JitEntry, JitTable};
use std::collections::HashSet;

const I64: types::Type = types::I64;
// Offsets of the `JitCtx` fields.
const CTX_ERROR: i32 = 0;
const CTX_DEPTH: i32 = 8;
const CTX_MAX_DEPTH: i32 = 16;
const CTX_BUDGET: i32 = 24;

/// Owns the JIT's executable memory.
struct Code(#[allow(dead_code)] JITModule);
// SAFETY: after `finalize_definitions` the module is never used again except
// to be dropped; the code it owns is immutable and safe to call from any thread.
unsafe impl Send for Code {}
unsafe impl Sync for Code {}

/// Whether the interpreter instruction can be compiled natively.
fn supported(ins: &Instr, ok: &HashSet<u32>) -> bool {
    use Instr::*;
    match ins {
        Int(..) | Bool(..) | Move(..) | AddI(..) | SubI(..) | MulI(..) | ModI(..) | NegI(..) | IncI(..)
        | AddIImm(..) | SubIImm(..) | BitAnd(..) | BitOr(..) | BitXor(..) | Shl(..) | Shr(..) | BitNot(..)
        | Not(..) | EqI(..) | LtI(..) | LeI(..) | Eq(..) | Ne(..) | Lt(..) | Le(..) | Cmp(..) | Jump(..)
        | JumpIfFalse(..) | JumpIfTrue(..) | JumpIfNotLtI(..) | JumpIfNotLeI(..) | JumpIfNotLtIImm(..)
        | JumpIfNotLeIImm(..) | Ret(..) | RetNull => true,
        Call { func, .. } => ok.contains(func),
        _ => false,
    }
}

/// Whether every register, jump target and call in `f` is in range. Bytecode
/// can come from the on-disk cache, and the JIT must never trust it blindly.
fn well_formed(prog: &Program, f: &Function) -> bool {
    use Instr::*;
    let Some(sig) = &f.native else { return false };
    let regs = f.num_regs as usize;
    let n = f.code.len() as u32;
    let r = |x: &u16| (*x as usize) < regs;
    if sig.params.len() > 16 || sig.params.len() > regs {
        return false;
    }
    if !matches!(f.code.last(), Some(Ret(_)) | Some(RetNull)) {
        return false;
    }
    f.code.iter().all(|ins| match ins {
        Int(d, _) | Bool(d, _) | IncI(d, _) => r(d),
        Move(a, b) | NegI(a, b) | BitNot(a, b) | Not(a, b) | AddIImm(a, b, _) | SubIImm(a, b, _) => r(a) && r(b),
        AddI(a, b, c)
        | SubI(a, b, c)
        | MulI(a, b, c)
        | ModI(a, b, c)
        | BitAnd(a, b, c)
        | BitOr(a, b, c)
        | BitXor(a, b, c)
        | Shl(a, b, c)
        | Shr(a, b, c)
        | EqI(a, b, c)
        | LtI(a, b, c)
        | LeI(a, b, c)
        | Eq(a, b, c)
        | Ne(a, b, c)
        | Lt(a, b, c)
        | Le(a, b, c)
        | Cmp(a, b, c) => r(a) && r(b) && r(c),
        Jump(t) => *t < n,
        JumpIfFalse(a, t) | JumpIfTrue(a, t) | JumpIfNotLtIImm(a, _, t) | JumpIfNotLeIImm(a, _, t) => r(a) && *t < n,
        JumpIfNotLtI(a, b, t) | JumpIfNotLeI(a, b, t) => r(a) && r(b) && *t < n,
        Ret(a) => r(a),
        RetNull => true,
        Call { dst, func, args, argc } => {
            let callee = prog.funcs.get(*func as usize).and_then(|c| c.native.as_ref());
            r(dst)
                && callee.is_some_and(|c| c.params.len() == *argc as usize)
                && (*args as usize + *argc as usize) <= regs
        }
        _ => true,
    })
}

/// Functions that can be compiled: native signature, supported instructions,
/// and calls only to other compilable functions.
fn eligible(prog: &Program) -> HashSet<u32> {
    let mut ok: HashSet<u32> = prog
        .funcs
        .iter()
        .enumerate()
        .filter(|(_, f)| f.native.is_some() && well_formed(prog, f))
        .map(|(i, _)| i as u32)
        .collect();
    loop {
        let before = ok.len();
        let snapshot = ok.clone();
        ok.retain(|f| prog.funcs[*f as usize].code.iter().all(|i| supported(i, &snapshot)));
        if ok.len() == before {
            return ok;
        }
    }
}

fn inner_sig(module: &JITModule, f: &Function) -> Signature {
    let mut sig = module.make_signature();
    sig.params.push(AbiParam::new(module.target_config().pointer_type()));
    for _ in &f.native.as_ref().unwrap().params {
        sig.params.push(AbiParam::new(I64));
    }
    sig.returns.push(AbiParam::new(I64));
    sig
}

/// Compiles every eligible function. Returns `None` if there are none (or
/// the host is not supported by Cranelift).
pub fn build(prog: &Program) -> Option<JitTable> {
    let ok = eligible(prog);
    if ok.is_empty() {
        return None;
    }
    let mut flags = settings::builder();
    flags.set("use_colocated_libcalls", "false").ok()?;
    flags.set("is_pic", "false").ok()?;
    flags.set("opt_level", "speed").ok()?;
    let isa = cranelift_native::builder().ok()?.finish(settings::Flags::new(flags)).ok()?;
    let mut jb = JITBuilder::with_isa(isa, default_libcall_names());
    jb.symbol("phx_jit_tick", phx_jit_tick as *const u8);
    let mut module = JITModule::new(jb);
    let ptr = module.target_config().pointer_type();

    let mut tick_sig = module.make_signature();
    tick_sig.params.push(AbiParam::new(ptr));
    tick_sig.returns.push(AbiParam::new(I64));
    let tick = module.declare_function("phx_jit_tick", Linkage::Import, &tick_sig).ok()?;

    let mut ids: Vec<Option<(FuncId, FuncId)>> = vec![None; prog.funcs.len()];
    let mut order: Vec<u32> = ok.iter().copied().collect();
    order.sort();
    for &f in &order {
        let func = &prog.funcs[f as usize];
        let inner = module.declare_function(&format!("phx{f}"), Linkage::Local, &inner_sig(&module, func)).ok()?;
        let mut esig = module.make_signature();
        esig.params.push(AbiParam::new(ptr));
        esig.params.push(AbiParam::new(ptr));
        esig.returns.push(AbiParam::new(I64));
        let entry = module.declare_function(&format!("phx{f}_entry"), Linkage::Local, &esig).ok()?;
        ids[f as usize] = Some((inner, entry));
    }

    let mut ctx = module.make_context();
    let mut fctx = FunctionBuilderContext::new();
    for &f in &order {
        let func = &prog.funcs[f as usize];
        let (inner, entry) = ids[f as usize].unwrap();
        ctx.func.signature = inner_sig(&module, func);
        {
            let config = module.target_config();
            let t = Translator::new(FunctionBuilder::new(&mut ctx.func, &mut fctx), func, config);
            let tick_ref = module.declare_func_in_func(tick, t.b.func);
            let callees: Vec<Option<cranelift_codegen::ir::FuncRef>> =
                ids.iter().map(|id| id.map(|(inner, _)| module.declare_func_in_func(inner, t.b.func))).collect();
            t.translate(tick_ref, &callees);
        }
        module.define_function(inner, &mut ctx).ok()?;
        module.clear_context(&mut ctx);

        // entry(ctx, args) -> inner(ctx, args[0], args[1], ...)
        ctx.func.signature = {
            let mut s = module.make_signature();
            s.params.push(AbiParam::new(ptr));
            s.params.push(AbiParam::new(ptr));
            s.returns.push(AbiParam::new(I64));
            s
        };
        {
            let mut b = FunctionBuilder::new(&mut ctx.func, &mut fctx);
            let mf = MemFlagsData::trusted();
            let blk = b.create_block();
            b.append_block_params_for_function_params(blk);
            b.switch_to_block(blk);
            let (c, args) = (b.block_params(blk)[0], b.block_params(blk)[1]);
            let n = func.native.as_ref().unwrap().params.len();
            let mut vals = vec![c];
            for i in 0..n {
                vals.push(b.ins().load(I64, mf, args, (i * 8) as i32));
            }
            let callee = module.declare_func_in_func(inner, b.func);
            let call = b.ins().call(callee, &vals);
            let r = b.inst_results(call)[0];
            b.ins().return_(&[r]);
            b.seal_all_blocks();
            b.finalize(module.target_config());
        }
        module.define_function(entry, &mut ctx).ok()?;
        module.clear_context(&mut ctx);
    }
    module.finalize_definitions().ok()?;

    let mut entries: Vec<Option<JitEntry>> = vec![None; prog.funcs.len()];
    for &f in &order {
        let (_, entry) = ids[f as usize].unwrap();
        let p = module.get_finalized_function(entry);
        // SAFETY: `entry` was defined above with exactly the `JitEntry` signature.
        entries[f as usize] = Some(unsafe { std::mem::transmute::<*const u8, JitEntry>(p) });
    }
    Some(JitTable { entries, keep_alive: Box::new(Code(module)) })
}

struct Translator<'a> {
    b: FunctionBuilder<'a>,
    func: &'a Function,
    mf: MemFlagsData,
    config: TargetFrontendConfig,
    vars: Vec<Variable>,
    blocks: Vec<Block>,
    ctx: Value,
}

impl<'a> Translator<'a> {
    fn new(b: FunctionBuilder<'a>, func: &'a Function, config: TargetFrontendConfig) -> Translator<'a> {
        let mf = MemFlagsData::trusted();
        Translator { b, func, mf, config, vars: Vec::new(), blocks: Vec::new(), ctx: Value::from_u32(0) }
    }

    fn get(&mut self, r: u16) -> Value {
        self.b.use_var(self.vars[r as usize])
    }

    fn set(&mut self, r: u16, v: Value) {
        self.b.def_var(self.vars[r as usize], v);
    }

    fn bool_val(&mut self, cond: Value) -> Value {
        self.b.ins().uextend(I64, cond)
    }

    /// Stores `code` in `ctx.error` and returns 0.
    fn fail(&mut self, code: Value) {
        self.b.ins().store(self.mf, code, self.ctx, CTX_ERROR);
        let zero = self.b.ins().iconst(I64, 0);
        self.b.ins().return_(&[zero]);
    }

    /// Branches to a fresh error block when `cond` is set; continues in a new block.
    fn fail_if(&mut self, cond: Value, code: i64) {
        let err = self.b.create_block();
        let cont = self.b.create_block();
        self.b.ins().brif(cond, err, &[], cont, &[]);
        self.b.switch_to_block(err);
        let c = self.b.ins().iconst(I64, code);
        self.fail(c);
        self.b.switch_to_block(cont);
    }

    /// Decrements the budget and calls `phx_jit_tick` when it runs out.
    fn tick(&mut self, tick: cranelift_codegen::ir::FuncRef) {
        let budget = self.b.ins().load(I64, self.mf, self.ctx, CTX_BUDGET);
        let next = self.b.ins().iadd_imm_s(budget, -1);
        self.b.ins().store(self.mf, next, self.ctx, CTX_BUDGET);
        let out = self.b.ins().icmp_imm_s(IntCC::SignedLessThanOrEqual, next, 0);
        let slow = self.b.create_block();
        let cont = self.b.create_block();
        self.b.ins().brif(out, slow, &[], cont, &[]);
        self.b.switch_to_block(slow);
        let call = self.b.ins().call(tick, &[self.ctx]);
        let r = self.b.inst_results(call)[0];
        let err = self.b.create_block();
        self.b.ins().brif(r, err, &[], cont, &[]);
        self.b.switch_to_block(err);
        self.fail(r);
        self.b.switch_to_block(cont);
    }

    fn ret(&mut self, v: Value) {
        let d = self.b.ins().load(I64, self.mf, self.ctx, CTX_DEPTH);
        let d = self.b.ins().iadd_imm_s(d, -1);
        self.b.ins().store(self.mf, d, self.ctx, CTX_DEPTH);
        self.b.ins().return_(&[v]);
    }

    /// `a + b` (or `a - b`) with an overflow check.
    fn add_checked(&mut self, a: Value, b: Value, sub: bool) -> Value {
        let (r, of) = if sub {
            let r = self.b.ins().isub(a, b);
            // overflow iff a and b have different signs and r's sign differs from a's
            let x = self.b.ins().bxor(a, b);
            let y = self.b.ins().bxor(a, r);
            (r, self.b.ins().band(x, y))
        } else {
            let r = self.b.ins().iadd(a, b);
            // overflow iff r's sign differs from both a's and b's
            let x = self.b.ins().bxor(a, r);
            let y = self.b.ins().bxor(b, r);
            (r, self.b.ins().band(x, y))
        };
        let neg = self.b.ins().icmp_imm_s(IntCC::SignedLessThan, of, 0);
        self.fail_if(neg, JIT_ERR_OVERFLOW);
        r
    }

    fn mul_checked(&mut self, a: Value, b: Value) -> Value {
        let lo = self.b.ins().imul(a, b);
        let hi = self.b.ins().smulhi(a, b);
        let sign = self.b.ins().sshr_imm_s(lo, 63);
        let of = self.b.ins().icmp(IntCC::NotEqual, hi, sign);
        self.fail_if(of, JIT_ERR_OVERFLOW);
        lo
    }

    fn jump(&mut self, target: u32) {
        let blk = self.blocks[target as usize];
        self.b.ins().jump(blk, &[]);
    }

    fn translate(mut self, tick: cranelift_codegen::ir::FuncRef, callees: &[Option<cranelift_codegen::ir::FuncRef>]) {
        let code = &self.func.code;
        let nparams = self.func.native.as_ref().unwrap().params.len();
        let entry = self.b.create_block();
        self.b.append_block_params_for_function_params(entry);
        self.blocks = (0..code.len()).map(|_| self.b.create_block()).collect();
        self.vars = (0..self.func.num_regs.max(1)).map(|_| self.b.declare_var(I64)).collect();

        self.b.switch_to_block(entry);
        self.ctx = self.b.block_params(entry)[0];
        let zero = self.b.ins().iconst(I64, 0);
        for r in 0..self.vars.len() {
            let v = if r < nparams { self.b.block_params(entry)[r + 1] } else { zero };
            self.b.def_var(self.vars[r], v);
        }
        // depth check and budget
        let d = self.b.ins().load(I64, self.mf, self.ctx, CTX_DEPTH);
        let d = self.b.ins().iadd_imm_s(d, 1);
        self.b.ins().store(self.mf, d, self.ctx, CTX_DEPTH);
        let max = self.b.ins().load(I64, self.mf, self.ctx, CTX_MAX_DEPTH);
        let deep = self.b.ins().icmp(IntCC::SignedGreaterThan, d, max);
        self.fail_if(deep, JIT_ERR_DEPTH);
        self.tick(tick);
        self.jump(0);

        for (i, ins) in code.iter().enumerate() {
            self.b.switch_to_block(self.blocks[i]);
            let next = (i + 1) as u32;
            match *ins {
                Instr::Int(d, v) => {
                    let c = self.b.ins().iconst(I64, v);
                    self.set(d, c);
                }
                Instr::Bool(d, v) => {
                    let c = self.b.ins().iconst(I64, v as i64);
                    self.set(d, c);
                }
                Instr::Move(d, s) => {
                    let v = self.get(s);
                    self.set(d, v);
                }
                Instr::AddI(d, a, b) | Instr::SubI(d, a, b) => {
                    let (x, y) = (self.get(a), self.get(b));
                    let r = self.add_checked(x, y, matches!(ins, Instr::SubI(..)));
                    self.set(d, r);
                }
                Instr::AddIImm(d, a, n) | Instr::SubIImm(d, a, n) => {
                    let x = self.get(a);
                    let y = self.b.ins().iconst(I64, n as i64);
                    let r = self.add_checked(x, y, matches!(ins, Instr::SubIImm(..)));
                    self.set(d, r);
                }
                Instr::IncI(r, n) => {
                    let x = self.get(r);
                    let y = self.b.ins().iconst(I64, n);
                    let v = self.add_checked(x, y, false);
                    self.set(r, v);
                }
                Instr::MulI(d, a, b) => {
                    let (x, y) = (self.get(a), self.get(b));
                    let r = self.mul_checked(x, y);
                    self.set(d, r);
                }
                Instr::ModI(d, a, b) => {
                    let (x, y) = (self.get(a), self.get(b));
                    let is_zero = self.b.ins().icmp_imm_s(IntCC::Equal, y, 0);
                    self.fail_if(is_zero, JIT_ERR_DIV_ZERO);
                    // i64::MIN % -1 traps on x86; the answer is 0.
                    let is_m1 = self.b.ins().icmp_imm_s(IntCC::Equal, y, -1);
                    let one = self.b.ins().iconst(I64, 1);
                    let safe = self.b.ins().select(is_m1, one, y);
                    let r = self.b.ins().srem(x, safe);
                    let zero = self.b.ins().iconst(I64, 0);
                    let r = self.b.ins().select(is_m1, zero, r);
                    self.set(d, r);
                }
                Instr::NegI(d, a) => {
                    let x = self.get(a);
                    let is_min = self.b.ins().icmp_imm_s(IntCC::Equal, x, i64::MIN);
                    self.fail_if(is_min, JIT_ERR_OVERFLOW);
                    let r = self.b.ins().ineg(x);
                    self.set(d, r);
                }
                Instr::BitAnd(d, a, b) | Instr::BitOr(d, a, b) | Instr::BitXor(d, a, b) => {
                    let (x, y) = (self.get(a), self.get(b));
                    let r = match ins {
                        Instr::BitAnd(..) => self.b.ins().band(x, y),
                        Instr::BitOr(..) => self.b.ins().bor(x, y),
                        _ => self.b.ins().bxor(x, y),
                    };
                    self.set(d, r);
                }
                Instr::Shl(d, a, b) | Instr::Shr(d, a, b) => {
                    let (x, y) = (self.get(a), self.get(b));
                    let neg = self.b.ins().icmp_imm_s(IntCC::SignedLessThan, y, 0);
                    self.fail_if(neg, JIT_ERR_SHIFT);
                    let big = self.b.ins().icmp_imm_s(IntCC::SignedGreaterThanOrEqual, y, 64);
                    let (shifted, overflowed) = if matches!(ins, Instr::Shl(..)) {
                        (self.b.ins().ishl(x, y), self.b.ins().iconst(I64, 0))
                    } else {
                        (self.b.ins().sshr(x, y), self.b.ins().sshr_imm_s(x, 63))
                    };
                    let r = self.b.ins().select(big, overflowed, shifted);
                    self.set(d, r);
                }
                Instr::BitNot(d, a) => {
                    let x = self.get(a);
                    let r = self.b.ins().bnot(x);
                    self.set(d, r);
                }
                Instr::Not(d, a) => {
                    let x = self.get(a);
                    let r = self.b.ins().bxor_imm_s(x, 1);
                    self.set(d, r);
                }
                Instr::EqI(d, a, b)
                | Instr::Eq(d, a, b)
                | Instr::Ne(d, a, b)
                | Instr::LtI(d, a, b)
                | Instr::Lt(d, a, b)
                | Instr::LeI(d, a, b)
                | Instr::Le(d, a, b) => {
                    let cc = match ins {
                        Instr::EqI(..) | Instr::Eq(..) => IntCC::Equal,
                        Instr::Ne(..) => IntCC::NotEqual,
                        Instr::LtI(..) | Instr::Lt(..) => IntCC::SignedLessThan,
                        _ => IntCC::SignedLessThanOrEqual,
                    };
                    let (x, y) = (self.get(a), self.get(b));
                    let c = self.b.ins().icmp(cc, x, y);
                    let r = self.bool_val(c);
                    self.set(d, r);
                }
                Instr::Cmp(d, a, b) => {
                    let (x, y) = (self.get(a), self.get(b));
                    let gt = self.b.ins().icmp(IntCC::SignedGreaterThan, x, y);
                    let lt = self.b.ins().icmp(IntCC::SignedLessThan, x, y);
                    let gt = self.bool_val(gt);
                    let lt = self.bool_val(lt);
                    let r = self.b.ins().isub(gt, lt);
                    self.set(d, r);
                }
                Instr::Jump(t) => {
                    if t as usize <= i {
                        self.tick(tick);
                    }
                    self.jump(t);
                    continue;
                }
                Instr::JumpIfFalse(r, t) | Instr::JumpIfTrue(r, t) => {
                    let c = self.get(r);
                    let (yes, no) = if matches!(ins, Instr::JumpIfTrue(..)) { (t, next) } else { (next, t) };
                    let (yes, no) = (self.blocks[yes as usize], self.blocks[no as usize]);
                    self.b.ins().brif(c, yes, &[], no, &[]);
                    continue;
                }
                Instr::JumpIfNotLtI(a, b, t) | Instr::JumpIfNotLeI(a, b, t) => {
                    let (x, y) = (self.get(a), self.get(b));
                    let cc = if matches!(ins, Instr::JumpIfNotLtI(..)) {
                        IntCC::SignedLessThan
                    } else {
                        IntCC::SignedLessThanOrEqual
                    };
                    let c = self.b.ins().icmp(cc, x, y);
                    let (yes, no) = (self.blocks[next as usize], self.blocks[t as usize]);
                    self.b.ins().brif(c, yes, &[], no, &[]);
                    continue;
                }
                Instr::JumpIfNotLtIImm(a, n, t) | Instr::JumpIfNotLeIImm(a, n, t) => {
                    let x = self.get(a);
                    let cc = if matches!(ins, Instr::JumpIfNotLtIImm(..)) {
                        IntCC::SignedLessThan
                    } else {
                        IntCC::SignedLessThanOrEqual
                    };
                    let c = self.b.ins().icmp_imm_s(cc, x, n as i64);
                    let (yes, no) = (self.blocks[next as usize], self.blocks[t as usize]);
                    self.b.ins().brif(c, yes, &[], no, &[]);
                    continue;
                }
                Instr::Call { dst, func, args, argc } => {
                    let callee = callees[func as usize].expect("call to a function that was not compiled");
                    let mut vals = vec![self.ctx];
                    for k in 0..argc {
                        vals.push(self.get(args + k));
                    }
                    let call = self.b.ins().call(callee, &vals);
                    let r = self.b.inst_results(call)[0];
                    let e = self.b.ins().load(I64, self.mf, self.ctx, CTX_ERROR);
                    let failed = self.b.create_block();
                    let cont = self.b.create_block();
                    self.b.ins().brif(e, failed, &[], cont, &[]);
                    self.b.switch_to_block(failed);
                    let zero = self.b.ins().iconst(I64, 0);
                    self.b.ins().return_(&[zero]);
                    self.b.switch_to_block(cont);
                    self.set(dst, r);
                }
                Instr::Ret(r) => {
                    let v = self.get(r);
                    self.ret(v);
                    continue;
                }
                Instr::RetNull => {
                    let z = self.b.ins().iconst(I64, 0);
                    self.ret(z);
                    continue;
                }
                _ => unreachable!("unsupported instruction reached the JIT"),
            }
            if (next as usize) < code.len() {
                self.jump(next);
            } else {
                let z = self.b.ins().iconst(I64, 0);
                self.ret(z);
            }
        }
        self.b.seal_all_blocks();
        self.b.finalize(self.config);
    }
}
