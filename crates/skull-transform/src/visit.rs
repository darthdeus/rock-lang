use comfy_la_arena::Idx;
use skull_mir::mir::*;

/// Field name for a frame cell in the state table.
pub(crate) fn raw<T>(i: Idx<T>) -> u32 {
    i.into_raw().into()
}

/// Successor blocks of a terminator (NOT closure entries — those are operands).
/// Visit each successor block of `term` (callback form — no per-call `Vec`
/// alloc; the sole caller, `top_level_blocks`, only iterates).
pub(crate) fn term_succs(term: &Term, mut f: impl FnMut(Idx<Block>)) {
    match term {
        Term::Goto(b) => f(*b),
        Term::Branch {
            then_bb, else_bb, ..
        } => {
            f(*then_bb);
            f(*else_bb);
        }
        Term::Switch { cases, default, .. } => {
            for (_, b) in cases {
                f(*b);
            }
            f(*default);
        }
        Term::Yield { resume, .. } => f(*resume),
        Term::Return(_) | Term::Unreachable => {}
    }
}

/// Blocks reachable from the function entry via terminators — the co-fn's own
/// CFG, EXCLUDING closure sub-blocks (reached only through `Inst::Lambda`).
pub(crate) fn top_level_blocks(f: &MirFunc) -> Vec<Idx<Block>> {
    let mut seen = vec![false; f.blocks.len()];
    let mut stack = vec![f.entry];
    seen[raw(f.entry) as usize] = true;
    let mut order = Vec::with_capacity(f.blocks.len());
    while let Some(b) = stack.pop() {
        order.push(b);
        term_succs(&f.blocks[b].term, |s| {
            let seen = &mut seen[raw(s) as usize];
            if !*seen {
                *seen = true;
                stack.push(s);
            }
        });
    }
    order
}

/// Remap an instruction's value operands through `m`.
pub(crate) fn remap_inst(inst: &mut Inst, m: &impl Fn(Idx<Inst>) -> Idx<Inst>) {
    let mut f = |x: &mut Idx<Inst>| *x = m(*x);
    match inst {
        Inst::Const(_)
        | Inst::Global(_)
        | Inst::Load(_)
        | Inst::Lambda { .. }
        | Inst::CoroHook { .. } => {}
        Inst::FfiViewCast { ptr, .. } => f(ptr),
        Inst::IntAdd(a, b)
        | Inst::IntSub(a, b)
        | Inst::IntMul(a, b)
        | Inst::IntDiv(a, b)
        | Inst::IntMod(a, b)
        | Inst::FloatAdd(a, b)
        | Inst::FloatSub(a, b)
        | Inst::FloatMul(a, b)
        | Inst::FloatDiv(a, b)
        | Inst::FloatMod(a, b)
        | Inst::StringConcat(a, b)
        | Inst::IntLt(a, b)
        | Inst::IntLeq(a, b)
        | Inst::IntGt(a, b)
        | Inst::IntGeq(a, b)
        | Inst::UintAdd(a, b)
        | Inst::UintSub(a, b)
        | Inst::UintMul(a, b)
        | Inst::UintDiv(a, b)
        | Inst::UintMod(a, b)
        | Inst::UintLt(a, b)
        | Inst::UintLeq(a, b)
        | Inst::UintGt(a, b)
        | Inst::UintGeq(a, b)
        | Inst::FloatLt(a, b)
        | Inst::FloatLeq(a, b)
        | Inst::FloatGt(a, b)
        | Inst::FloatGeq(a, b)
        | Inst::Eq(a, b)
        | Inst::NotEq(a, b)
        | Inst::BitAnd(a, b)
        | Inst::BitOr(a, b)
        | Inst::BitXor(a, b)
        | Inst::Shl(a, b)
        | Inst::Shr(a, b)
        | Inst::UintBitAnd(a, b)
        | Inst::UintBitOr(a, b)
        | Inst::UintBitXor(a, b)
        | Inst::UintShl(a, b)
        | Inst::UintShr(a, b) => {
            f(a);
            f(b);
        }
        Inst::Neg(x)
        | Inst::Not(x)
        | Inst::BitNot(x)
        | Inst::UintBitNot(x)
        | Inst::ToString(x)
        | Inst::FloatToInt(x)
        | Inst::IntToFloat(x)
        | Inst::UintToFloat(x)
        | Inst::FloatToF32(x)
        | Inst::IntToUint(x)
        | Inst::UintToInt(x)
        | Inst::FloatToUint(x)
        | Inst::UintBitcastInt(x)
        | Inst::IntBitcastUint(x)
        | Inst::CoroSetInner(x)
        | Inst::VecStrideNilToFalse(x)
        | Inst::VecStrideFalseToNil(x) => f(x),
        Inst::Format { value, .. } => f(value),
        Inst::FormatDyn { value, fmt } => {
            f(value);
            f(fmt);
        }
        Inst::Call { args, .. }
        | Inst::CallMulti { args, .. }
        | Inst::CallBoundMulti { args, .. } => args.iter_mut().for_each(&mut f),
        Inst::CallValue { callee, args } => {
            f(callee);
            args.iter_mut().for_each(&mut f);
        }
        Inst::CallValueMulti { callee, args, .. } => {
            f(callee);
            args.iter_mut().for_each(&mut f);
        }
        Inst::MakeTuple(vs) | Inst::MakeVec(vs) => vs.iter_mut().for_each(&mut f),
        Inst::MakeVecColumns {
            columns, capacity, ..
        } => {
            f(capacity);
            columns.iter_mut().flat_map(|(_, vs)| vs).for_each(&mut f)
        }
        Inst::MakeVecColumnHeader { capacity, .. } => f(capacity),
        Inst::MakeVecStrided {
            values, capacity, ..
        } => {
            f(capacity);
            values.iter_mut().for_each(&mut f);
        }
        Inst::MakeVecStridedHeader { capacity, .. } => f(capacity),
        Inst::MakeStruct { fields, .. }
        | Inst::MakeValueStruct { fields, .. }
        | Inst::MaterializeValueStruct { fields, .. }
        | Inst::MakeEnv { fields, .. } => fields.iter_mut().for_each(|(_, a)| f(a)),
        Inst::MakeNullableValue { present, value } => {
            f(present);
            f(value);
        }
        Inst::MakeEnum {
            positional, named, ..
        } => {
            positional.iter_mut().for_each(&mut f);
            named.iter_mut().for_each(|(_, a)| f(a));
        }
        Inst::MakeEnumDyn {
            tag,
            positional,
            named,
            ..
        } => {
            f(tag);
            positional.iter_mut().for_each(&mut f);
            named.iter_mut().for_each(|(_, a)| f(a));
        }
        Inst::FfiNewArray { count, .. } => f(count),
        Inst::Field { base, .. } | Inst::TupleIndex { base, .. } => f(base),
        Inst::Index { base, index, .. } => {
            f(base);
            f(index);
        }
        Inst::VecCheckedIndex { vec, index } => {
            f(vec);
            f(index);
        }
        Inst::Len { base, .. } => f(base),
        Inst::Store { value, .. } => f(value),
        Inst::StoreField { base, value, .. } => {
            f(base);
            f(value);
        }
        Inst::StoreIndex {
            base, index, value, ..
        } => {
            f(base);
            f(index);
            f(value);
        }
        Inst::StoreGlobal { value, .. } => f(value),
        Inst::ArrayPush { arr, value, .. } => {
            f(arr);
            f(value);
        }
        Inst::MapNext { table, .. } => f(table),
    }
}

/// Value operand of a terminator (the only ones are `cond`/`discr`/return value).
pub(crate) fn term_value(term: &Term) -> Option<Idx<Inst>> {
    match term {
        // Coroutine returns are single/zero by the time spill runs (a co fn never
        // scalarizes its completion value); the first covers it.
        Term::Return(vs) => vs.first().copied(),
        Term::Branch { cond, .. } => Some(*cond),
        Term::Switch { discr, .. } => Some(*discr),
        Term::Yield { values, .. } => values.first().copied(),
        _ => None,
    }
}

pub(crate) fn remap_term(term: &mut Term, m: &impl Fn(Idx<Inst>) -> Idx<Inst>) {
    match term {
        Term::Return(vs) => vs.iter_mut().for_each(|v| *v = m(*v)),
        Term::Branch { cond: v, .. } | Term::Switch { discr: v, .. } => *v = m(*v),
        Term::Yield { values, .. } => values.iter_mut().for_each(|v| *v = m(*v)),
        _ => {}
    }
}
