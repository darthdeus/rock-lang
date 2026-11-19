//! Keep wide, pure float results out of a call's live set. Reads stay at
//! their MIR positions; only arithmetic on their already-captured values moves.

use super::*;

fn float_arithmetic(inst: &Inst) -> bool {
    matches!(
        inst,
        Inst::FloatAdd(..) | Inst::FloatSub(..) | Inst::FloatMul(..)
    )
}

fn scalar_call(inst: &Inst) -> bool {
    matches!(inst, Inst::Call { .. } | Inst::CallValue { .. })
}

/// Return a reordered clone and the calls that must render at their MIR site.
/// Call folding would otherwise move a guarded call back across the arithmetic.
pub(crate) fn defer_wide_float_math(func: &MirFunc) -> Option<(MirFunc, Vec<Idx<Inst>>)> {
    if !func.blocks.iter().any(|(_, block)| {
        block
            .insts
            .iter()
            .filter(|&&id| float_arithmetic(&func.insts[id]))
            .count()
            >= 8
            && block.insts.iter().any(|&id| scalar_call(&func.insts[id]))
    }) {
        return None;
    }

    let mut users = vec![Vec::new(); func.insts.len()];
    let mut term_used = vec![false; func.insts.len()];
    for (_, block) in func.blocks.iter() {
        for &id in &block.insts {
            func.insts[id].uses_each(&mut |operand| {
                users[raw(operand) as usize].push(id);
            });
        }
        block.term.uses_each(&mut |operand| {
            term_used[raw(operand) as usize] = true;
        });
    }

    let mut reordered = None;
    let mut pinned_calls = Vec::new();
    for (block_id, block) in func.blocks.iter() {
        let insts = &block.insts;
        let mut preceding_math = 0;
        for (call_at, &call) in insts.iter().enumerate() {
            if float_arithmetic(&func.insts[call]) {
                preceding_math += 1;
                continue;
            }
            if !scalar_call(&func.insts[call]) {
                continue;
            }
            if preceding_math < 8 {
                continue;
            }
            let mut move_it: HashSet<Idx<Inst>> = insts[..call_at]
                .iter()
                .copied()
                .filter(|&id| float_arithmetic(&func.insts[id]))
                .collect();

            // A value used by an instruction that stays before the call must
            // stay there too. Reverse order propagates a rejected arithmetic
            // user's dependency to its earlier operands in one walk.
            for &id in insts[..=call_at].iter().rev() {
                if !move_it.contains(&id) {
                    func.insts[id].uses_each(&mut |operand| {
                        move_it.remove(&operand);
                    });
                }
            }

            let mut outputs = 0usize;
            let mut inputs = HashSet::default();
            for &id in &insts[..call_at] {
                if !move_it.contains(&id) {
                    continue;
                }
                if term_used[raw(id) as usize]
                    || users[raw(id) as usize]
                        .iter()
                        .any(|user| !move_it.contains(user))
                {
                    outputs += 1;
                }
                func.insts[id].uses_each(&mut |operand| {
                    if !move_it.contains(&operand) && !matches!(func.insts[operand], Inst::Const(_))
                    {
                        inputs.insert(operand);
                    }
                });
            }
            // Each moved output dies before the call; each captured input may
            // newly live across it. Require a substantial net reduction.
            if outputs < 8 || outputs < inputs.len() + 8 {
                break;
            }

            let mut order = Vec::with_capacity(insts.len());
            order.extend(
                insts[..call_at]
                    .iter()
                    .copied()
                    .filter(|id| !move_it.contains(id)),
            );
            order.push(call);
            order.extend(
                insts[..call_at]
                    .iter()
                    .copied()
                    .filter(|id| move_it.contains(id)),
            );
            order.extend_from_slice(&insts[call_at + 1..]);
            reordered.get_or_insert_with(|| func.clone()).blocks[block_id].insts = order;
            pinned_calls.push(call);
            break;
        }
    }
    reordered.map(|func| (func, pinned_calls))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(func: &mut MirFunc, block: Idx<Block>) -> Idx<Inst> {
        let cell = func.new_cell(None);
        func.emit(block, Inst::Load(cell))
    }

    fn one_block(inputs: usize, share_input: bool) -> MirFunc {
        let mut func = MirFunc::new(ustr::Ustr::from("wide"));
        let block = func.entry;
        let callee = func.emit(block, Inst::Const(Lit::Nil));
        let mut results = Vec::new();
        let shared = load(&mut func, block);
        for _ in 0..inputs {
            let left = if share_input {
                shared
            } else {
                load(&mut func, block)
            };
            let right = if share_input {
                shared
            } else {
                load(&mut func, block)
            };
            results.push(func.emit(block, Inst::FloatAdd(left, right)));
        }
        func.emit(
            block,
            Inst::CallValue {
                callee,
                args: Vec::new(),
            },
        );
        func.set_term(block, Term::Return(results));
        func
    }

    #[test]
    fn leaves_many_inputs_live_when_no_net_pressure_is_saved() {
        assert!(defer_wide_float_math(&one_block(9, false)).is_none());
    }

    #[test]
    fn moves_many_outputs_after_call_when_they_share_a_captured_input() {
        let func = one_block(9, true);
        let (reordered, pinned) = defer_wide_float_math(&func).expect("wide group");
        assert_eq!(pinned.len(), 1);
        let insts = &reordered.blocks[reordered.entry].insts;
        let call_at = insts.iter().position(|id| *id == pinned[0]).unwrap();
        assert!(
            insts[call_at + 1..]
                .iter()
                .all(|&id| float_arithmetic(&reordered.insts[id]))
        );
    }
}
