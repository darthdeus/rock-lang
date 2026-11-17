use rustc_hash::FxHashSet as HashSet;

use crate::closure::{FrameInfo, convert_closures_framed};
use crate::frame::materialize_frame_cells;
use comfy_la_arena::Idx;
use skull_mir::cfg::successors;
use skull_mir::mir::*;
use ustr::Ustr;

/// Include nested lambda entries in the detached region.
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

/// Copy a detached body into an ordinary function that takes the frame.
pub(crate) fn lower_detached_region(
    source: &MirFunc,
    entry: Idx<Block>,
    name: &str,
    frame: &FrameInfo<'_>,
) -> MirFunc {
    let region = detached_region_blocks(source, entry);
    let mut aux = source.clone();
    aux.name = Ustr::from(name);
    aux.entry = entry;
    aux.code_identity = source
        .code_identity
        .as_ref()
        .map(skull_mir::code_identity::CodeIdentity::owner)
        .zip(detached_region_site(source, &region))
        .map(
            |(enclosing, site)| skull_mir::code_identity::CodeIdentity::Nested {
                enclosing,
                site,
                name: aux.name,
            },
        );
    aux.code_abi_fingerprint = Some(format!("detached:{name}"));
    aux.code_compatibility =
        aux.code_identity
            .as_ref()
            .map(|_| skull_mir::code_identity::CodeCompatibility {
                call_abi: format!("detached:{name}"),
                capture: skull_mir::code_identity::CaptureSchema::empty(),
                frame: Some(frame.schema()),
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

    // The hook's registration lambda is outside the detached body.
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
    materialize_frame_cells(&mut aux, aux_entry, frame.s_cell, frame.slots);
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
        .min_by_key(|span| span.offset_range().0)
        .and_then(skull_mir::code_identity::LambdaSiteId::from_span)
}

/// Remove the original closure producers after their regions have been split.
pub(crate) fn erase_detached_lambdas(
    func: &mut MirFunc,
    entries: impl IntoIterator<Item = Idx<Block>>,
) {
    let mut detached = HashSet::default();
    for entry in entries {
        detached.extend(detached_region_blocks(func, entry));
    }
    func.code_value_identities.retain(|inst, _| {
        !matches!(func.insts[*inst], Inst::Lambda { entry, .. } if detached.contains(&entry))
    });
    for (_, inst) in func.insts.iter_mut() {
        if let Inst::Lambda { entry, .. } = inst
            && detached.contains(entry)
        {
            *inst = Inst::Const(Lit::Nil);
        }
    }
}
