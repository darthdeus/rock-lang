//! Evaluation-order constraints shared by expression-folding backends.

use comfy_la_arena::Idx;

use crate::mir::{Block, Inst, MirFunc};

/// Whether evaluating this instruction can raise a language/runtime error.
/// Allocation exhaustion is excluded. Projections are conservative because
/// MIR does not carry a non-null proof on their base operand.
pub fn may_throw(inst: &Inst) -> bool {
    matches!(
        inst,
        Inst::Format { .. }
            | Inst::FormatDyn { .. }
            | Inst::IntToUint(_)
            | Inst::FloatToUint(_)
            | Inst::UintShl(..)
            | Inst::UintShr(..)
            | Inst::Index { .. }
            | Inst::VecCheckedIndex { .. }
            | Inst::Field { .. }
            | Inst::TupleIndex { .. }
            | Inst::Len { .. }
            | Inst::Call { .. }
            | Inst::CallValue { .. }
            | Inst::CallMulti { .. }
            | Inst::CallValueMulti { .. }
            | Inst::CallBoundMulti { .. }
            | Inst::FfiNewArray { .. }
            | Inst::FfiViewCast { .. }
    )
}

/// An expression a backend may fold into its single use site: pure apart from
/// the throws [`preserve_effect_order`] constrains. `Load`, `Call`, `Index`,
/// `Field` and `Len` are excluded (impure or cell-order-sensitive), so those
/// materialize at their program position.
pub fn always_inlinable(inst: &Inst) -> bool {
    matches!(
        inst,
        Inst::Const(_)
            | Inst::IntAdd(..)
            | Inst::IntSub(..)
            | Inst::IntMul(..)
            | Inst::IntDiv(..)
            | Inst::IntMod(..)
            | Inst::UintAdd(..)
            | Inst::UintSub(..)
            | Inst::UintMul(..)
            | Inst::UintDiv(..)
            | Inst::UintMod(..)
            | Inst::FloatAdd(..)
            | Inst::FloatSub(..)
            | Inst::FloatMul(..)
            | Inst::FloatDiv(..)
            | Inst::FloatMod(..)
            | Inst::StringConcat(..)
            | Inst::IntLt(..)
            | Inst::IntLeq(..)
            | Inst::IntGt(..)
            | Inst::IntGeq(..)
            | Inst::UintLt(..)
            | Inst::UintLeq(..)
            | Inst::UintGt(..)
            | Inst::UintGeq(..)
            | Inst::FloatLt(..)
            | Inst::FloatLeq(..)
            | Inst::FloatGt(..)
            | Inst::FloatGeq(..)
            | Inst::Eq(..)
            | Inst::NotEq(..)
            | Inst::BitAnd(..)
            | Inst::BitOr(..)
            | Inst::BitXor(..)
            | Inst::Shl(..)
            | Inst::Shr(..)
            | Inst::UintBitAnd(..)
            | Inst::UintBitOr(..)
            | Inst::UintBitXor(..)
            | Inst::UintShl(..)
            | Inst::UintShr(..)
            | Inst::Neg(_)
            | Inst::Not(_)
            | Inst::BitNot(_)
            | Inst::UintBitNot(_)
            | Inst::ToString(_)
            | Inst::FloatToInt(_)
            | Inst::IntToFloat(_)
            | Inst::UintToFloat(_)
            | Inst::FloatToF32(_)
            | Inst::IntToUint(_)
            | Inst::UintToInt(_)
            | Inst::FloatToUint(_)
            | Inst::UintBitcastInt(_)
            | Inst::IntBitcastUint(_)
            | Inst::Format { .. }
            | Inst::FormatDyn { .. }
            | Inst::MakeTuple(_)
            | Inst::MakeVec(_)
            | Inst::MakeStruct { .. }
            | Inst::MakeValueStruct { .. }
            | Inst::MakeNullableValue { .. }
            | Inst::MakeEnum { .. }
            | Inst::MakeEnumDyn { .. }
    )
}

fn is_barrier(inst: &Inst) -> bool {
    !inst.produces_value() || may_throw(inst)
}

/// Restrict an existing same-block folding mask to preserve observable order.
/// Non-throwing expressions keep their eligibility. A throwing expression may
/// reach its use only when no effect or other possible error intervenes.
pub fn preserve_effect_order(func: &MirFunc, blocks: &[Idx<Block>], inlinable: &mut [bool]) {
    let mut after = vec![0usize; func.insts.len()];
    let mut evaluation = vec![0usize; func.insts.len()];
    for &block in blocks {
        let mut epoch = 0;
        for &id in &func.blocks[block].insts {
            epoch += usize::from(is_barrier(&func.insts[id]));
            after[id.into_raw().into_u32() as usize] = epoch;
        }
        func.blocks[block].term.uses_each(&mut |value| {
            evaluation[value.into_raw().into_u32() as usize] = epoch;
        });
        // Follow folded consumers to their final evaluation point. Checking
        // only the immediate use misses a throw nested in a folded tuple/concat.
        for &id in func.blocks[block].insts.iter().rev() {
            let k = id.into_raw().into_u32() as usize;
            let inst = &func.insts[id];
            if inlinable[k] && may_throw(inst) && evaluation[k] != after[k] {
                inlinable[k] = false;
            }
            epoch -= usize::from(is_barrier(inst));
            let at = if inlinable[k] { evaluation[k] } else { epoch };
            inst.uses_each(&mut |value| {
                let use_at = &mut evaluation[value.into_raw().into_u32() as usize];
                *use_at = (*use_at).max(at);
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mir::{ContainerKind, Lit, Term};
    use ustr::Ustr;

    fn mask(func: &MirFunc) -> Vec<bool> {
        let mut inlinable = func
            .insts
            .iter()
            .map(|(_, inst)| inst.produces_value())
            .collect::<Vec<_>>();
        preserve_effect_order(func, &[func.entry], &mut inlinable);
        inlinable
    }

    fn folded(inlinable: &[bool], id: Idx<Inst>) -> bool {
        inlinable[id.into_raw().into_u32() as usize]
    }

    #[test]
    fn throwing_projections_and_calls_cannot_cross_a_write() {
        for make in [
            (|v| Inst::Index {
                base: v,
                index: v,
                kind: ContainerKind::Vec,
            }) as fn(Idx<Inst>) -> Inst,
            |v| Inst::VecCheckedIndex { vec: v, index: v },
            |v| Inst::Field {
                base: v,
                field: Ustr::from("value"),
            },
            |v| Inst::TupleIndex { base: v, index: 0 },
            |v| Inst::CallValue {
                callee: v,
                args: vec![],
            },
        ] {
            let mut func = MirFunc::new(Ustr::from("probe"));
            let b = func.entry;
            let base = func.emit(b, Inst::Const(Lit::Nil));
            let value = func.emit(b, make(base));
            func.emit(
                b,
                Inst::StoreGlobal {
                    name: Ustr::from("marks"),
                    value: base,
                },
            );
            func.set_term(b, Term::Return(vec![value]));
            assert!(!folded(&mask(&func), value), "{:?}", func.insts[value]);
        }
    }

    #[test]
    fn folded_consumer_cannot_carry_a_throw_past_an_effect() {
        let mut func = MirFunc::new(Ustr::from("probe"));
        let b = func.entry;
        let input = func.emit(b, Inst::Const(Lit::Int(-1)));
        let checked = func.emit(b, Inst::IntToUint(input));
        let tuple = func.emit(b, Inst::MakeTuple(vec![checked]));
        func.emit(
            b,
            Inst::StoreGlobal {
                name: Ustr::from("marks"),
                value: input,
            },
        );
        func.set_term(b, Term::Return(vec![tuple]));
        let inlinable = mask(&func);
        assert!(!folded(&inlinable, checked));
        assert!(folded(&inlinable, tuple));
    }

    #[test]
    fn non_throwing_arithmetic_can_cross_effects() {
        let mut func = MirFunc::new(Ustr::from("probe"));
        let b = func.entry;
        let input = func.emit(b, Inst::Const(Lit::Int(1)));
        let sum = func.emit(b, Inst::IntAdd(input, input));
        let tuple = func.emit(b, Inst::MakeTuple(vec![sum]));
        func.emit(
            b,
            Inst::CallValue {
                callee: input,
                args: vec![],
            },
        );
        func.set_term(b, Term::Return(vec![tuple]));
        let inlinable = mask(&func);
        assert!(folded(&inlinable, sum));
        assert!(folded(&inlinable, tuple));
    }

    #[test]
    fn a_throw_can_fold_into_the_next_effect_operand() {
        let mut func = MirFunc::new(Ustr::from("probe"));
        let b = func.entry;
        let input = func.emit(b, Inst::Const(Lit::Int(-1)));
        let checked = func.emit(b, Inst::IntToUint(input));
        func.emit(
            b,
            Inst::StoreGlobal {
                name: Ustr::from("value"),
                value: checked,
            },
        );
        assert!(folded(&mask(&func), checked));
    }

    #[test]
    fn a_throw_cannot_cross_another_possible_error() {
        let mut func = MirFunc::new(Ustr::from("probe"));
        let b = func.entry;
        let input = func.emit(b, Inst::Const(Lit::Int(-1)));
        let checked = func.emit(b, Inst::IntToUint(input));
        func.emit(
            b,
            Inst::Field {
                base: input,
                field: Ustr::from("value"),
            },
        );
        func.set_term(b, Term::Return(vec![checked]));
        assert!(!folded(&mask(&func), checked));
    }
}
