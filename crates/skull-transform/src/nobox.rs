use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use crate::cleanup::{cleanup_in, dce, drop_f32_rounding_into_float_views};
use crate::mem2reg::promote_direct_cells;
use bone_core::prelude::CompilerError;
use bone_core::workspace::Workspace;
use comfy_la_arena::Idx;
use skull_mir::mir::*;
use ustr::Ustr;

/// Every emitted implicit `nobox` materialization in `func` -- EVERY
/// one, not the first. A body with two sinks is two findings: reporting one
/// makes the second invisible until the first is fixed, so any "N sinks left"
/// count read off this check would be a floor rather than a total (the
/// `pass_uniforms` gap hid behind an earlier rejection in the same body exactly
/// this way).
pub fn nobox_materialization_violations(
    ws: &Workspace,
    func: &MirFunc,
) -> Vec<(String, Option<bone_core::prelude::Span>)> {
    let mut out = Vec::new();
    // Closure conversion retains cloned, unreachable parent/sibling blocks.
    // Follow the emission oracle, including unhoisted coroutine lambdas/hooks.
    for bid in skull_mir::reach::emitted_blocks(func) {
        let block = &func.blocks[bid];
        for &iid in &block.insts {
            let Inst::MakeValueStruct { name, .. } = &func.insts[iid] else {
                continue;
            };
            // A cdata ENUM packs through `MakeValueStruct` under the enum's own
            // name, which is correctly absent from `struct_by_name` -- `nobox` is
            // a struct marker, so those are genuinely out of scope. Any OTHER
            // unknown name is not a licence to skip: this rung enforces a
            // GUARANTEE, so it is reported rather than passed over.
            let Some(sid) = ws.struct_by_name.get(name) else {
                if skull_mir::lower::EnumPlan::for_name(ws, *name)
                    .is_some_and(|plan| plan.cdata().is_some())
                {
                    continue;
                }
                out.push((
                    format!(
                        "`{name}` is constructed as a value struct here but is neither a struct \
                         nor a cdata enum in the workspace, so the `nobox` guarantee cannot be \
                         checked for it"
                    ),
                    func.inst_spans.get(iid).copied(),
                ));
                continue;
            };
            if ws.modules[sid.module.index()].structs[sid.def].is_nobox {
                out.push((
                    format!(
                        "nobox value `{name}` survived lowering as an implicit materialization"
                    ),
                    func.inst_spans.get(iid).copied(),
                ));
            }
        }
    }
    out
}

/// Whether `mvs`'s constructor tree transitively contains an IMPLICIT nobox
/// materialization. The trigger is recursive on purpose: for
/// `Outer { b: Box2 { lo: NV2 {..} } }` the outer's immediate operand is a
/// `Box2` literal, which is not itself nobox. A `MaterializeValueStruct`
/// operand is the SANCTIONED escape and is never a violation.
fn ctor_tree_has_implicit_nobox(ws: &Workspace, func: &MirFunc, mvs: Idx<Inst>) -> bool {
    let (name, fields) = match &func.insts[mvs] {
        Inst::MakeValueStruct { name, fields, .. } => (name, fields),
        // An EXPLICIT box is a sanctioned root, but its nested fields are still
        // implicit: `materialize(E5 { pos: NV2 { .. } })` authorizes the E5 box,
        // never a separate NV2 one.
        Inst::MaterializeValueStruct { name, fields, .. } => (name, fields),
        _ => return false,
    };
    if let Some(sid) = ws.struct_by_name.get(name) {
        if ws.modules[sid.module.index()].structs[sid.def].is_nobox {
            return true;
        }
    }
    fields.iter().any(|(_, op)| {
        // A `materialize` OPERAND is the sanctioned escape -- never a violation,
        // and not descended into.
        !matches!(&func.insts[*op], Inst::MaterializeValueStruct { .. })
            && ctor_tree_has_implicit_nobox(ws, func, *op)
    })
}

/// Resolve one dotted leaf path against a ctor tree, emitting `Field` reads for
/// any OPAQUE operand the path passes through (a nested value read off an
/// existing cdata is legal and must be decomposed, not rejected). `None` when
/// the path does not resolve, which the caller turns into a rejection rather
/// than a guess.
fn leaf_operand(
    func: &mut MirFunc,
    root: Idx<Inst>,
    path: &str,
    out: &mut Vec<Idx<Inst>>,
) -> Option<Idx<Inst>> {
    let mut cur = root;
    for comp in path.split('.') {
        let comp = Ustr::from(comp);
        let next = match &func.insts[cur] {
            // Reads THROUGH an explicit `materialize` operand, so the box it
            // requested is DCE'd when the parent flattens for some other field.
            // Deliberate: a value struct has no identity, so eliding a box of one
            // is unobservable -- `materialize`'s contract is about what the
            // program computes, not about a heap object surviving. Preserving it
            // instead needs the `Field` reads ordered after the box, which the
            // splice position does not currently give (they land at the parent
            // ctor's slot, ahead of where emit places the operand).
            Inst::MakeValueStruct { fields, .. } | Inst::MaterializeValueStruct { fields, .. } => {
                fields.iter().find(|(n, _)| *n == comp).map(|(_, op)| *op)
            }
            // Opaque: read the field off the existing value.
            _ => {
                let f = func.alloc_inst(Inst::Field {
                    base: cur,
                    field: comp,
                });
                out.push(f);
                Some(f)
            }
        }?;
        cur = next;
    }
    Some(cur)
}

/// Flatten a value-struct ctor whose tree implicitly materializes a `nobox`
/// field into one carrying that type's COMPLETE primitive leaf list, keyed by
/// dotted path (`("pos.x", ..)`). The nested ctors then have no users and DCE
/// removes them.
///
/// Flattening happens at the OUTERMOST cdata materialization: rewriting
/// bottom-up would satisfy the guarantee too, but allocate an intermediate cdata
/// per construction. A ctor that is itself a field operand of another cdata
/// ctor is therefore skipped -- the outer one absorbs it.
///
/// Only the CDATA tier is rewritten, because only a cdata parent has physical
/// fields to splice the child's leaves into. The other two tiers need nothing
/// from this pass: a leaf-table box is already built from leaves by `pack`, and
/// a GC-table box keeps its declared keys, so a `nobox` child inside one is a
/// real box that the verifier rejects at its own span. A plain cdata child of
/// a GC-table box (`RoomC { span: Span2 { lo: Cell { .. } } }`) is flattened
/// here like any outermost cdata ctor.
///
/// The dotted form exists only between here (end of `cleanup`) and emit. Field
/// names are documented as MIR-pass metadata rather than the wire shape, and
/// every pass that runs in between (`convert_closures`, `recover_counted_loops`,
/// `inline_hoisted_bodies`, the verifier) remaps operands or matches the struct
/// NAME only -- none interpret field names.
pub fn flatten_nobox_nested_ctors(
    ws: &Workspace,
    func: &mut MirFunc,
) -> Result<bool, CompilerError> {
    let all: Vec<Idx<Inst>> = func
        .blocks
        .iter()
        .flat_map(|(_, b)| b.insts.iter().copied())
        .collect();
    // A ctor used as a field operand of another CDATA ctor is not the
    // outermost one. A table parent absorbs nothing, so a cdata child of a
    // GC-table or leaf-table ctor is itself the outermost cdata materialization.
    let mut inner: HashSet<Idx<Inst>> = HashSet::default();
    for &i in &all {
        if let Inst::MakeValueStruct {
            fields,
            representation,
            ..
        }
        | Inst::MaterializeValueStruct {
            fields,
            representation,
            ..
        } = &func.insts[i]
        {
            let table_parent = matches!(
                func.rep_arena
                    .as_ref()
                    .and_then(|arena| arena.try_get(*representation))
                    .and_then(|plan| plan.value_storage()),
                Some(
                    skull_mir::lower::ValueStorage::GcTable
                        | skull_mir::lower::ValueStorage::LeafTable
                )
            );
            if !table_parent {
                inner.extend(fields.iter().map(|(_, op)| *op));
            }
        }
    }
    let targets: Vec<Idx<Inst>> = all
        .iter()
        .copied()
        .filter(|i| !inner.contains(i) && ctor_tree_has_implicit_nobox(ws, func, *i))
        .collect();
    if targets.is_empty() {
        return Ok(false);
    }
    let rep_arena = func.rep_arena.clone().ok_or_else(|| {
        CompilerError::new_no_location(format!(
            "nobox constructor in `{}` has no build-owned representation arena",
            func.name
        ))
    })?;
    let mut replacement: HashMap<Idx<Inst>, Vec<Idx<Inst>>> = HashMap::default();
    for t in targets {
        let span = func.inst_spans.get(t).copied();
        let reject = |msg: String| match span {
            Some(sp) => CompilerError::new_with_label(sp, msg),
            None => CompilerError::new_no_location(msg),
        };
        let (name, representation, form, explicit) = match &func.insts[t] {
            Inst::MakeValueStruct {
                name,
                representation,
                form,
                ..
            } => (*name, *representation, *form, false),
            Inst::MaterializeValueStruct {
                name,
                representation,
                form,
                ..
            } => (*name, *representation, *form, true),
            _ => continue,
        };
        // A ctor ALREADY in leaf form carries the canonical leaves as its
        // operands, so there is nothing left to splice. It still matches the
        // trigger (a `nobox` name is its own), and re-resolving its dotted keys
        // as a field path fails whenever a path has more than one component --
        // `lo` is not a field of a ctor keyed `lo.x` -- which is what made
        // `materialize` of a nested cdata `nobox` value reject on the pass's
        // second iteration.
        if matches!(form, ValueCtorForm::Leaves) {
            continue;
        }
        let plan = rep_arena.try_get(representation).ok_or_else(|| {
            reject(format!(
                "nobox constructor `{name}` carries representation {representation:?} that does not belong to its build arena"
            ))
        })?;
        // Flattening splices a nested ctor's leaves into the PARENT's physical
        // fields, which only the cdata tier has -- a GC-table parent keeps its
        // declared keys and a leaf-table parent is already built from leaves by
        // `pack`. Leaving those trees alone is not a fallback: an explicit
        // `materialize` at those tiers is legal exactly as it stands, and an
        // IMPLICIT nobox ctor still inside one is rejected, at its own span, by
        // `nobox_materialization_violations`.
        if plan.value_storage() != Some(skull_mir::lower::ValueStorage::CData) {
            continue;
        }
        let paths = plan.leaf_paths().ok_or_else(|| {
            reject(format!(
                "nobox constructor `{name}` carries representation {representation:?} without a leaf-path plan"
            ))
        })?;
        let mut pre: Vec<Idx<Inst>> = Vec::new();
        let mut leaves: Vec<(Ustr, Idx<Inst>)> = Vec::with_capacity(paths.len());
        for path in &paths {
            let op = leaf_operand(func, t, path.as_str(), &mut pre).ok_or_else(|| {
                reject(format!(
                    "nobox constructor `{name}` representation {representation:?} leaf `{path}` does not resolve through its physical fields"
                ))
            })?;
            leaves.push((*path, op));
        }
        // An already-flat ctor rewrites to ITSELF -- a `nobox` name is its own
        // trigger, so `materialize(Color { .. })` is a target on every pass. The
        // rewrite is harmless but the "changed" answer is not: `cleanup_with_nobox`
        // iterates on it, so reporting a no-op as progress never terminates.
        let unchanged = matches!(
            &func.insts[t],
            Inst::MakeValueStruct { fields, .. } | Inst::MaterializeValueStruct { fields, .. }
                if *fields == leaves
        );
        if unchanged {
            continue;
        }
        // Preserve the variant: an explicit box stays explicit, it just stops
        // building a nested one on the way.
        let flat = func.alloc_inst(if explicit {
            Inst::MaterializeValueStruct {
                name,
                representation,
                form: ValueCtorForm::Leaves,
                fields: leaves,
            }
        } else {
            Inst::MakeValueStruct {
                name,
                representation,
                form: ValueCtorForm::Leaves,
                fields: leaves,
            }
        });
        // Minted insts inherit the outer ctor's span: a rejection downstream must
        // still name the user's line.
        if let Some(sp) = span {
            for &n in pre.iter().chain(std::iter::once(&flat)) {
                func.inst_spans.insert(n, sp);
            }
        }
        pre.push(flat);
        replacement.insert(t, pre);
    }
    if replacement.is_empty() {
        return Ok(false);
    }
    // The flattened ctor takes the original's slot, so every existing user of
    // `t` now reads the flat form.
    let remap: HashMap<Idx<Inst>, Idx<Inst>> = replacement
        .iter()
        .map(|(k, v)| (*k, *v.last().unwrap()))
        .collect();
    let m = |o: Idx<Inst>| remap.get(&o).copied().unwrap_or(o);
    for i in func.insts.iter_mut().map(|(id, _)| id).collect::<Vec<_>>() {
        func.insts[i].remap_uses(&m);
    }
    // Terminators too: a `Term` still pointing at the replaced ctor references an
    // inst that is no longer in any block, which regalloc later reads as an
    // unassigned value index and panics rather than failing anywhere useful.
    let block_ids: Vec<Idx<Block>> = func.blocks.iter().map(|(id, _)| id).collect();
    for bid in &block_ids {
        func.blocks[*bid].term.remap_uses(&m);
    }
    for bid in block_ids {
        if !func.blocks[bid]
            .insts
            .iter()
            .any(|i| replacement.contains_key(i))
        {
            continue;
        }
        let old = std::mem::take(&mut func.blocks[bid].insts);
        let mut new = Vec::with_capacity(old.len());
        for i in old {
            match replacement.get(&i) {
                Some(ins) => new.extend_from_slice(ins),
                None => new.push(i),
            }
        }
        func.blocks[bid].insts = new;
    }
    Ok(true)
}

/// [`cleanup`] plus nobox nested-ctor normalization and the DCE that clears the
/// ctors it orphans. Every emit path that is followed by
/// [`nobox_materialization_violations`] must use THIS, not `cleanup`:
/// normalization has to run before the verifier, and after `cleanup`'s own final
/// DCE (the verifier walks `block.insts`, so an orphaned inner ctor left in the
/// list still rejects). Idempotent -- a rewritten ctor is in LEAF form and the
/// pass skips those -- which matters because `cleanup` is called from many
/// sites. The name-based trigger still fires on it, so the skip has to be the
/// form and not the trigger.
///
/// Every emit-path caller uses this; plain `cleanup` survives only as this
/// function's own first step. Were a site to miss it, the failure would be
/// CLOSED -- a spurious rejection from the verifier, never a silent miscompile.
pub fn cleanup_with_nobox(ws: &Workspace, func: &mut MirFunc) -> Result<(), CompilerError> {
    cleanup_in(Some(ws), func)?;
    // INTERLEAVED, not once each. Flattening replaces a nested nobox ctor's
    // users with leaf reads, and that is exactly the shape SROA needs to split
    // the field cell the ctor is stored to: while the outer ctor still reads the
    // nested one whole, the nested ctor has more than one user and SROA leaves
    // the field cell alone, so the box survives. Terminates because the pass is
    // idempotent -- a flattened ctor no longer matches the trigger.
    while flatten_nobox_nested_ctors(ws, func)? {
        cleanup_in(Some(ws), func)?;
    }
    // Each successful flatten is followed by cleanup's fixed-point DCE;
    // the final unsuccessful flatten leaves the live program unchanged.
    promote_direct_cells(func);
    // After promotion, so a rounding that reached the store through a cell
    // (an inlined constructor with a defaulted argument) is seen directly.
    if drop_f32_rounding_into_float_views(func) {
        dce(func);
    }
    Ok(())
}
