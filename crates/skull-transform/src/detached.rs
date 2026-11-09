use rustc_hash::FxHashSet as HashSet;

use crate::closure::{FrameInfo, convert_closures_framed};
use crate::frame::materialize_frame_cells;
use comfy_la_arena::Idx;
use skull_mir::cfg::successors;
use skull_mir::mir::*;
use ustr::Ustr;

/// Blocks reachable from a detached entry through ordinary CFG edges and
/// nested lambda entries. A detached region owns its own ordinary `MirFunc`;
/// the source co-fn arena is only used as the physical source of the clone.
pub(crate) fn detached_region_blocks(func: &MirFunc, entry: Idx<Block>) -> HashSet<Idx<Block>> {
    let mut seen = HashSet::default();
    let mut stack = vec![entry];
    while let Some(block) = stack.pop() {
        if !seen.insert(block) {
            continue;
        }
        for &id in &func.blocks[block].insts {
            if let Inst::Lambda { entry, .. } = func.insts[id] {
                stack.push(entry);
            }
        }
        successors(&func.blocks[block].term, |next| stack.push(next));
    }
    seen
}

/// Copy one hook/defer region into an ordinary MIR function and run the shared
/// closure conversion over it. The detached function receives the coroutine
/// state table as its only parameter; cells owned by the coroutine root still
/// resolve through that table, while cells introduced by the region are locals.
pub(crate) fn lower_detached_region(
    source: &MirFunc,
    entry: Idx<Block>,
    name: &str,
    frame: &FrameInfo,
) -> MirFunc {
    let region = detached_region_blocks(source, entry);
    let mut aux = source.clone();
    aux.name = Ustr::from(name);
    aux.entry = entry;
    aux.code_identity = source
        .code_identity
        .as_ref()
        .and_then(skull_mir::code_identity::CodeIdentity::owner)
        .zip(detached_region_site(source, &region))
        .map(
            |(enclosing, site)| skull_mir::code_identity::CodeIdentity::Nested {
                enclosing,
                site,
                name: aux.name,
            },
        );
    aux.code_abi_fingerprint = Some(format!("detached:{name}"));
    let slots = {
        let mut slots: Vec<u32> = frame.slots.values().copied().collect();
        slots.sort_unstable();
        slots
    };
    aux.code_compatibility =
        aux.code_identity
            .as_ref()
            .map(|_| skull_mir::code_identity::CodeCompatibility {
                call_abi: format!("detached:{name}"),
                capture: skull_mir::code_identity::CaptureSchema::empty(),
                frame: Some(skull_mir::code_identity::FrameSchema { slots }),
                lambda_topology: Default::default(),
            });
    aux.params = vec![frame.s_cell];
    aux.is_coroutine = false;
    aux.resume_arg = None;
    aux.coro_defers.clear();
    aux.nested_fns.clear();
    aux.closure_env_plans.clear();
    aux.hoisted_fn_globals.clear();
    aux.code_value_identities.retain(|inst, _| {
        !matches!(aux.insts[*inst], Inst::Lambda { entry, .. } if !region.contains(&entry))
    });

    // The hook's outer lambda is a registration artifact, not a closure inside
    // the detached body. Remove it from the clone before ScopeTree sees it;
    // nested lambdas whose entries are inside the region remain for conversion.
    for (_, inst) in aux.insts.iter_mut() {
        if let Inst::Lambda {
            entry: lambda_entry,
            ..
        } = inst
            && (*lambda_entry == entry || !region.contains(lambda_entry))
        {
            *inst = Inst::Const(Lit::Nil);
        }
    }

    convert_closures_framed(&mut aux, name, Some(frame), None);
    let aux_entry = aux.entry;
    materialize_frame_cells(&mut aux, aux_entry, frame.s_cell, &frame.slots);
    assert!(
        !aux.insts
            .iter()
            .any(|(_, inst)| matches!(inst, Inst::Lambda { .. })),
        "detached region `{name}` retained an Inst::Lambda after conversion"
    );
    aux
}

fn detached_region_site(
    source: &MirFunc,
    region: &HashSet<Idx<Block>>,
) -> Option<skull_mir::code_identity::LambdaSiteId> {
    region
        .iter()
        .flat_map(|block| source.blocks[*block].insts.iter().copied())
        .filter_map(|inst| source.inst_spans.get(inst).copied())
        .min_by_key(|span| span.offset_range.0)
        .and_then(skull_mir::code_identity::LambdaSiteId::from_span)
}

/// Remove the detached closure producers from the shared coroutine arena after
/// their ordinary `MirFunc` copies have been made. This keeps the old inline
/// emitter path structurally unreachable and gives the residual census a single
/// zero-valued answer.
pub(crate) fn erase_detached_lambdas(
    func: &mut MirFunc,
    entries: impl IntoIterator<Item = Idx<Block>>,
) {
    let mut detached = HashSet::default();
    for entry in entries {
        detached.extend(detached_region_blocks(func, entry));
    }
    let detached_values: Vec<_> = func
        .code_value_identities
        .keys()
        .copied()
        .filter(|inst| {
            matches!(func.insts[*inst], Inst::Lambda { entry, .. } if detached.contains(&entry))
        })
        .collect();
    for inst in detached_values {
        func.code_value_identities.remove(&inst);
    }
    for (_, inst) in func.insts.iter_mut() {
        if let Inst::Lambda { entry, .. } = inst
            && detached.contains(entry)
        {
            *inst = Inst::Const(Lit::Nil);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::lower;
    use rustc_hash::FxHashMap as HashMap;

    #[test]
    fn detached_coroutine_regions_are_ordinary_converted_functions() {
        let mut source = lower("fn f() -> int { let add = (n: int) => n; add(1) }", "f");
        let entry = source.entry();
        let s_cell = source.new_cell(Some(Ustr::from("s")));
        let frame = FrameInfo {
            s_cell,
            fields: HashSet::default(),
            slots: HashMap::default(),
        };
        let aux = lower_detached_region(&source, entry, "f__detached", &frame);
        assert!(
            !aux.is_coroutine,
            "detached body must be an ordinary MirFunc"
        );
        assert!(
            !aux.nested_fns.is_empty(),
            "detached lambda must own a nested MirFunc"
        );
        assert!(
            aux.insts
                .iter()
                .any(|(_, inst)| matches!(inst, Inst::Global(_))),
            "capture-free detached lambda must become a Global value"
        );
        assert!(
            aux.insts
                .iter()
                .all(|(_, inst)| !matches!(inst, Inst::Lambda { .. })),
            "detached conversion must leave no residual lambda"
        );
        erase_detached_lambdas(&mut source, [entry]);
        assert!(
            source
                .insts
                .iter()
                .all(|(_, inst)| !matches!(inst, Inst::Lambda { .. })),
            "the emitted artifact must have no detached residual lambda"
        );
    }

    #[test]
    fn detached_nested_closure_reaches_frame_only_capture_through_state() {
        let mut source = lower(
            "fn f(seed: int) -> int { let get = () => seed; return get() }",
            "f",
        );
        let entry = source.entry();
        let seed = source.params[0];
        let s_cell = source.new_cell(Some(Ustr::from("s")));
        let frame = FrameInfo {
            s_cell,
            fields: [seed].into_iter().collect(),
            slots: [(seed, 0)].into_iter().collect(),
        };
        let aux = lower_detached_region(&source, entry, "f__detached", &frame);
        let plan = aux
            .closure_env_plans
            .values()
            .next()
            .expect("nested closure must have an environment plan");
        assert_eq!(plan.captures(), &[s_cell]);
    }
}
