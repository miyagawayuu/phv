//! Conservative register liveness for the generator suspension prototype.

use crate::bytecode::{Function, Instr, Reg, NO_REG};

const MAX_CODE: usize = 512;
const MIN_SAVING: usize = 6;

pub(crate) struct SavedRegisters {
    pub(crate) capacity: usize,
    yields: Vec<(usize, u128)>,
}

impl SavedRegisters {
    pub(crate) fn at(&self, pc: usize) -> Option<u128> {
        self.yields.binary_search_by_key(&pc, |&(at, _)| at).ok().map(|index| self.yields[index].1)
    }
}

#[derive(Clone, Copy)]
struct Step {
    read: u128,
    written: u128,
    next: [Option<usize>; 2],
}

fn bit(reg: Reg) -> Option<u128> {
    if reg == NO_REG {
        return Some(0);
    }
    1u128.checked_shl(reg.into())
}

fn span(start: Reg, count: u16) -> Option<u128> {
    (0..count).try_fold(0, |mask, offset| Some(mask | bit(start.checked_add(offset)?)?))
}

fn step(ins: &Instr, pc: usize) -> Option<Step> {
    use Instr::*;
    let mut next = [Some(pc + 1), None];
    let (read, written) = match *ins {
        Const(d, _) | Null(d) | Bool(d, _) | Int(d, _) | NewObject(d, _) | EnumCase(d, _, _) => (0, bit(d)?),
        Move(d, a)
        | NegI(d, a)
        | NegF(d, a)
        | Not(d, a)
        | BitNot(d, a)
        | IntToFloat(d, a)
        | FloatToInt(d, a)
        | ToStr(d, a)
        | AddIImm(d, a, _)
        | SubIImm(d, a, _)
        | GetProp(d, a, _)
        | InstanceOf(d, a, _)
        | IsType(d, a, _) => (bit(a)?, bit(d)?),
        AddI(d, a, b)
        | SubI(d, a, b)
        | MulI(d, a, b)
        | ModI(d, a, b)
        | PowI(d, a, b)
        | AddF(d, a, b)
        | SubF(d, a, b)
        | MulF(d, a, b)
        | DivF(d, a, b)
        | ModF(d, a, b)
        | PowF(d, a, b)
        | BitAnd(d, a, b)
        | BitOr(d, a, b)
        | BitXor(d, a, b)
        | Shl(d, a, b)
        | Shr(d, a, b)
        | EqI(d, a, b)
        | LtI(d, a, b)
        | LeI(d, a, b)
        | Eq(d, a, b)
        | Ne(d, a, b)
        | Lt(d, a, b)
        | Le(d, a, b)
        | Cmp(d, a, b)
        | EqF(d, a, b)
        | NeF(d, a, b)
        | LtF(d, a, b)
        | LeF(d, a, b)
        | CmpF(d, a, b)
        | Range(d, a, b, _)
        | Index(d, a, b)
        | IndexOrNull(d, a, b) => (bit(a)? | bit(b)?, bit(d)?),
        IncI(r, _) => (bit(r)?, bit(r)?),
        SetProp(o, _, v) => (bit(o)? | bit(v)?, 0),
        Call { dst, args, argc, .. } | CallBuiltin { dst, args, argc, .. } | Concat(dst, args, argc, _) => {
            (span(args, argc)?, bit(dst)?)
        }
        Echo(r) | EchoAttr(r) => (bit(r)?, 0),
        EchoHtml(_) | EchoNonce | GenStart(_) | TryBegin { .. } | TryEnd(_) => (0, 0),
        Ret(r) | Throw(r) | MatchError(r) => {
            next[0] = None;
            (bit(r)?, 0)
        }
        RetNull => {
            next[0] = None;
            (0, 0)
        }
        Jump(target) => {
            next[0] = Some(target as usize);
            (0, 0)
        }
        JumpIfFalse(r, target)
        | JumpIfTrue(r, target)
        | JumpIfNull(r, target)
        | JumpIfNotNull(r, target)
        | JumpIfNotLtIImm(r, _, target)
        | JumpIfNotLeIImm(r, _, target)
        | JumpIfLtIImm(r, _, target)
        | JumpIfLeIImm(r, _, target) => {
            next[1] = Some(target as usize);
            (bit(r)?, 0)
        }
        JumpIfNotLtI(a, b, target)
        | JumpIfNotLeI(a, b, target)
        | JumpIfEq(a, b, target)
        | JumpIfNotEq(a, b, target) => {
            next[1] = Some(target as usize);
            (bit(a)? | bit(b)?, 0)
        }
        Yield { key, val, from: false, resume } => {
            // Fallthrough is the early-close entry.
            next[1] = Some(resume as usize);
            (bit(key)? | bit(val)?, 0)
        }
        // Instructions that may implicitly access registers, debugger and
        // profiler markers, and generator relays keep the original layout.
        _ => return None,
    };
    Some(Step { read, written, next })
}

impl SavedRegisters {
    pub(crate) fn analyze(func: &Function, args: usize) -> Option<Self> {
        let width = func.num_regs as usize;
        let len = func.code.len();
        if !(8..=128).contains(&width) || len > MAX_CODE {
            return None;
        }
        let blank = Step { read: 0, written: 0, next: [None; 2] };
        let mut steps = [blank; MAX_CODE];
        for (pc, ins) in func.code.iter().enumerate() {
            let s = step(ins, pc)?;
            if s.next.into_iter().flatten().any(|target| target >= len)
                || matches!(*ins, Instr::TryBegin { catch, .. } if catch as usize >= len)
            {
                return None;
            }
            steps[pc] = s;
        }
        let mut live = [0u128; MAX_CODE];
        // Overapproximate the exception edge from every instruction by
        // including every catch in this function, before any writes.
        // Bounded sweeps avoid excessive analysis for complex functions.
        for _ in 0..32 {
            let mut changed = false;
            let exceptions = func.code.iter().fold(0, |mask, ins| match *ins {
                Instr::TryBegin { catch, .. } => mask | live[catch as usize],
                _ => mask,
            });
            for (pc, s) in steps[..len].iter().enumerate().rev() {
                let after = s.next.into_iter().flatten().fold(0, |mask, target| mask | live[target]);
                let before = s.read | (after & !s.written) | exceptions;
                changed |= live[pc] != before;
                live[pc] = before;
            }
            if !changed {
                let mut yields = Vec::new();
                let mut capacity = args;
                for (pc, ins) in func.code.iter().enumerate() {
                    if let Instr::Yield { resume, .. } = *ins {
                        let mask = live[resume as usize] | live[pc + 1];
                        if width < 128 && mask >> width != 0 {
                            return None;
                        }
                        capacity = capacity.max(mask.count_ones() as usize);
                        yields.push((pc, mask));
                    }
                }
                return (!yields.is_empty() && capacity + MIN_SAVING <= width).then_some(Self { capacity, yields });
            }
        }
        None
    }
}

pub(crate) fn ranges(mut mask: u128) -> impl Iterator<Item = std::ops::Range<usize>> {
    std::iter::from_fn(move || {
        if mask == 0 {
            return None;
        }
        let start = mask.trailing_zeros() as usize;
        let end = start + (mask >> start).trailing_ones() as usize;
        mask &= u128::MAX.checked_shl(end as u32).unwrap_or(0);
        Some(start..end)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use Instr::*;

    #[test]
    fn suspension_keeps_reads_from_resume_close_and_catch() {
        let function = Function {
            num_regs: 16,
            code: vec![
                GenStart(0),
                TryBegin { catch: 7, exc: 4 },
                Yield { key: NO_REG, val: 3, from: false, resume: 5 },
                Echo(1),
                RetNull,
                Echo(0),
                RetNull,
                Echo(2),
                RetNull,
            ],
            ..Function::default()
        };
        let plan = SavedRegisters::analyze(&function, 0).unwrap();
        assert_eq!(plan.capacity, 3);
        let mut live_ranges = ranges(plan.at(2).unwrap());
        assert_eq!(live_ranges.next(), Some(0..3));
        assert_eq!(live_ranges.next(), None);
    }
}
