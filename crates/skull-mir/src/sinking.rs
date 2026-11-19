use comfy_la_arena::Idx;

use crate::mir::{Block, Inst, MirFunc};

/// Allocation exhaustion is excluded. Field reads are non-throwing when lowering
/// proves a non-nil Rock value base.
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
            | Inst::Field {
                nonthrowing: false,
                ..
            }
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

/// `Load`, `Call`, `Index`, `Field` and `Len` are impure or cell-order-sensitive,
/// so they materialize at their program position.
pub fn always_inlinable(inst: &Inst) -> bool {
    matches!(
        inst,
        Inst::Const(_)
            | Inst::VecIndexSlot(_)
            | Inst::NbIndexSlot(_)
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
    // A projection may enter a host callback that writes globals, so it stays before
    // a later global snapshot.
    !inst.produces_value() || ordered_read(inst) || matches!(inst, Inst::Global(_))
}

// Non-throwing fields can be deleted when unused, but remain mutable reads: their
// snapshot must not sink across a call or a storage write.
fn ordered_read(inst: &Inst) -> bool {
    may_throw(inst) || matches!(inst, Inst::Field { .. })
}

/// A throwing expression may reach its use only when no effect or other possible
/// error intervenes.
pub fn preserve_effect_order(func: &MirFunc, blocks: &[Idx<Block>], inlinable: &mut [bool]) {
    preserve_effect_order_with(func, blocks, inlinable, |_, _| false);
}

/// `commutes(folded, between)` names an error a folded throwing expression may
/// cross because the target cannot tell the two apart.
pub fn preserve_effect_order_with(
    func: &MirFunc,
    blocks: &[Idx<Block>],
    inlinable: &mut [bool],
    commutes: impl Fn(&Inst, &Inst) -> bool,
) {
    let raw = |id: Idx<Inst>| id.into_raw().into_u32() as usize;
    // Position in its block before which a value is finally evaluated.
    let mut limit = vec![0usize; func.insts.len()];
    for &block in blocks {
        let insts = &func.blocks[block].insts;
        func.blocks[block].term.uses_each(&mut |value| {
            limit[raw(value)] = insts.len();
        });
        // Follow folded consumers to their final evaluation point: the immediate use
        // alone misses a throw nested in a folded tuple or concat.
        for (pos, &id) in insts.iter().enumerate().rev() {
            let k = raw(id);
            let inst = &func.insts[id];
            if inlinable[k] && ordered_read(inst) {
                let end = limit[k].max(pos + 1);
                if insts[pos + 1..end].iter().any(|&between| {
                    let between = &func.insts[between];
                    is_barrier(between) && !commutes(inst, between)
                }) {
                    inlinable[k] = false;
                }
            }
            // Operands run before the instruction; a folded barrier evaluates where it
            // stands, so its operands do too.
            let at = if inlinable[k] && !is_barrier(inst) {
                limit[k]
            } else {
                pos
            };
            inst.uses_each(&mut |value| {
                let use_at = &mut limit[raw(value)];
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
    fn projections_and_calls_cannot_cross_a_write() {
        for make in [
            (|v| Inst::Index {
                base: v,
                index: v,
                kind: ContainerKind::Vec,
            }) as fn(Idx<Inst>) -> Inst,
            |v| Inst::VecCheckedIndex { vec: v, index: v },
            |v| Inst::Field {
                nonthrowing: false,
                base: v,
                field: Ustr::from("value"),
            },
            |v| Inst::Field {
                nonthrowing: true,
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
    fn a_throw_can_fold_into_a_folded_call_that_consumes_it() {
        let mut func = MirFunc::new(Ustr::from("probe"));
        let b = func.entry;
        let input = func.emit(b, Inst::Const(Lit::Nil));
        let read = func.emit(
            b,
            Inst::Field {
                nonthrowing: false,
                base: input,
                field: Ustr::from("value"),
            },
        );
        let call = func.emit(
            b,
            Inst::CallValue {
                callee: input,
                args: vec![read],
            },
        );
        func.set_term(b, Term::Return(vec![call]));
        let inlinable = mask(&func);
        assert!(folded(&inlinable, call));
        assert!(folded(&inlinable, read));
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
                nonthrowing: false,
                base: input,
                field: Ustr::from("value"),
            },
        );
        func.set_term(b, Term::Return(vec![checked]));
        assert!(!folded(&mask(&func), checked));
    }
}
