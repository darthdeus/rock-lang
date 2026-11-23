use comfy_la_arena::Idx;

use crate::mir::{Block, Cell, Inst, Lit, MirFunc};

fn raw<T>(id: Idx<T>) -> u32 {
    id.into_raw().into_u32()
}

pub struct ScopeUses {
    pub count: Vec<u32>,
    pub block: Vec<u32>,
    /// `u32::MAX` when the last reader is a terminator.
    pub user: Vec<u32>,
    pub operand: Vec<u8>,
}

const NONE: u32 = u32::MAX;
/// The operands of a push, which writes the length before it evaluates the
/// pushed value, and a callee that would open its statement.
const PINNED: u8 = 1;
/// A store base opens its statement: folded, it renders parenthesized, and a
/// statement opening with `(` is ambiguous Lua after a line that ends in an
/// expression.
const STORE_BASE: u8 = 2;

#[derive(Clone, Copy, Default)]
struct CellWrites {
    count: u32,
    by: u32,
    block: u32,
    pos: u32,
}

/// Cells written exactly once in `scope`, as a multi-return destination, and
/// read only after that call in the same block: a load of one is the call's
/// result local itself, since the one write dominates every read.
fn single_receive_cells(
    func: &MirFunc,
    scope: &[Idx<Block>],
    is_pinned: impl Fn(Idx<Cell>) -> bool,
    writes: &[CellWrites],
) -> Vec<bool> {
    let captured = |cell: Idx<Cell>| {
        func.closure_env_plans
            .values()
            .any(|plan| plan.captures_cell(cell) || plan.env_cell() == Some(cell))
    };
    let mut out = vec![false; func.cells.len()];
    let mut any = false;
    for (cell, _) in func.cells.iter() {
        let w = writes[raw(cell) as usize];
        // A bound call's destinations are placeholders the bound substitution
        // replaces; never a receive.
        if w.count == 1
            && matches!(
                func.insts[Idx::from_raw(w.by.into())],
                Inst::CallMulti { .. } | Inst::CallValueMulti { .. }
            )
            && !is_pinned(cell)
            && !captured(cell)
        {
            out[raw(cell) as usize] = true;
            any = true;
        }
    }
    if !any {
        return out;
    }
    for &b in scope {
        for (pos, &id) in func.blocks[b].insts.iter().enumerate() {
            func.insts[id].cell_reads_each(&mut |cell| {
                if !out[raw(cell) as usize] {
                    return;
                }
                let w = writes[raw(cell) as usize];
                if w.block != raw(b)
                    || w.pos as usize >= pos
                    || !matches!(func.insts[id], Inst::Load(_))
                {
                    out[raw(cell) as usize] = false;
                }
            });
        }
    }
    out
}

/// Returns the folded values, the folded loads that may render at any number
/// of use sites, and how `scope` reads its values.
pub fn compute_inlinable(
    func: &MirFunc,
    scope: &[Idx<Block>],
    is_pinned: impl Fn(Idx<Cell>) -> bool,
) -> (Vec<bool>, Vec<bool>, ScopeUses) {
    // Insts outside `scope` (other closures share the arena) stay unset and
    // never qualify.
    let ni = func.insts.len();
    let mut uses = ScopeUses {
        count: vec![0u32; ni],
        block: vec![NONE; ni],
        user: vec![NONE; ni],
        operand: vec![0u8; ni],
    };
    let mut writes = vec![CellWrites::default(); func.cells.len()];
    // Selected-layout emission may read a dynamic tag more than once.
    let mut reread: Vec<Idx<Inst>> = Vec::new();
    for &b in scope {
        let br = raw(b);
        for (pos, &id) in func.blocks[b].insts.iter().enumerate() {
            let inst = &func.insts[id];
            inst.cell_writes_each(&mut |cell| {
                let w = &mut writes[raw(cell) as usize];
                w.count += 1;
                *w = CellWrites {
                    count: w.count,
                    by: raw(id),
                    block: br,
                    pos: pos as u32,
                };
            });
            inst.uses_each(&mut |u| {
                let k = raw(u) as usize;
                uses.count[k] += 1;
                uses.block[k] = br;
                uses.user[k] = raw(id);
            });
            match inst {
                Inst::StoreField { base, .. } | Inst::StoreIndex { base, .. } => {
                    uses.operand[raw(*base) as usize] |= PINNED | STORE_BASE;
                }
                Inst::CallValueMulti { dests, callee, .. } if dests.is_empty() => {
                    uses.operand[raw(*callee) as usize] |= PINNED;
                }
                Inst::ArrayPush { arr, value, .. } => {
                    uses.operand[raw(*arr) as usize] |= PINNED;
                    uses.operand[raw(*value) as usize] |= PINNED;
                }
                Inst::MakeEnumDyn { tag, .. } if !matches!(func.insts[*tag], Inst::Const(_)) => {
                    reread.push(*tag);
                }
                _ => {}
            }
        }
        func.blocks[b].term.uses_each(&mut |u| {
            let k = raw(u) as usize;
            uses.count[k] += 1;
            uses.block[k] = br;
            uses.user[k] = NONE;
        });
    }
    let mut immutable_params = vec![false; func.cells.len()];
    for &cell in &func.params {
        immutable_params[raw(cell) as usize] = writes[raw(cell) as usize].count == 0;
    }
    let received = single_receive_cells(func, scope, is_pinned, &writes);
    let mut inl = vec![false; ni];
    let mut repeatable = vec![false; ni];
    for &tag in &reread {
        uses.count[raw(tag) as usize] += 1;
    }
    for &b in scope {
        for &id in &func.blocks[b].insts {
            let k = raw(id) as usize;
            let parameter_load = matches!(
                &func.insts[id],
                Inst::Load(cell)
                    if immutable_params[raw(*cell) as usize] || received[raw(*cell) as usize]
            );
            // `(nil)[1] = x` cannot start a Lua statement, so a nil base keeps a register.
            let nil = matches!(func.insts[id], Inst::Const(Lit::Nil))
                && uses.operand[k] & STORE_BASE == 0;
            repeatable[k] = parameter_load || nil;
            // A store base other than a plain local keeps its own register.
            if parameter_load
                || nil
                || (uses.count[k] == 1
                    && uses.block[k] == raw(b)
                    && uses.operand[k] & STORE_BASE == 0
                    && always_inlinable(&func.insts[id]))
            {
                inl[k] = true;
            }
        }
    }
    for &tag in &reread {
        uses.count[raw(tag) as usize] -= 1;
    }
    (inl, repeatable, uses)
}

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
    use crate::mir::{Lit, Term};
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
