use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use crate::cleanup::{cleanup_in, dce, drop_f32_rounding_into_float_views};
use crate::mem2reg::promote_direct_cells;
use bone_core::prelude::CompilerError;
use bone_core::workspace::Workspace;
use comfy_la_arena::Idx;
use skull_mir::mir::*;
use ustr::Ustr;

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
            // Cdata enums share the value-struct constructor opcode.
            let Some(sid) = ws.nominals.struct_id(*name) else {
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

fn ctor_tree_has_implicit_nobox(ws: &Workspace, func: &MirFunc, mvs: Idx<Inst>) -> bool {
    let (name, fields) = match &func.insts[mvs] {
        Inst::MakeValueStruct { name, fields, .. } => (name, fields),
        // Explicit materialization authorizes only the root box.
        Inst::MaterializeValueStruct { name, fields, .. } => (name, fields),
        _ => return false,
    };
    if let Some(sid) = ws.nominals.struct_id(*name) {
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
            // An explicit value box has no identity and can disappear when its leaves suffice.
            Inst::MakeValueStruct { fields, .. } | Inst::MaterializeValueStruct { fields, .. } => {
                fields.iter().find(|(n, _)| *n == comp).map(|(_, op)| *op)
            }
            _ => {
                let f = func.alloc_inst(Inst::Field {
                    nonthrowing: false,
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

/// Flatten the outermost cdata constructor to its physical leaf fields.
/// The inner constructors can then disappear. GC tables keep declared keys;
/// leaf-table constructors already take their leaves.
fn flatten_nobox_nested_ctors(ws: &Workspace, func: &mut MirFunc) -> Result<bool, CompilerError> {
    if skull_mir::pass_disabled("noboxflat") {
        return Ok(false);
    }
    let any_nobox = func.blocks.iter().any(|(_, b)| {
        b.insts.iter().any(|&i| match &func.insts[i] {
            Inst::MakeValueStruct { name, .. } | Inst::MaterializeValueStruct { name, .. } => ws
                .nominals
                .struct_id(*name)
                .is_some_and(|sid| ws.modules[sid.module.index()].structs[sid.def].is_nobox),
            _ => false,
        })
    });
    if !any_nobox {
        return Ok(false);
    }
    let all: Vec<Idx<Inst>> = func
        .blocks
        .iter()
        .flat_map(|(_, b)| b.insts.iter().copied())
        .collect();
    // Only a cdata parent can absorb its nested constructors.
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
        // Existing leaf form uses dotted keys rather than nested operands.
        if matches!(form, ValueCtorForm::Leaves) {
            continue;
        }
        let plan = rep_arena.try_get(representation).ok_or_else(|| {
            reject(format!(
                "nobox constructor `{name}` carries representation {representation:?} that does not belong to its build arena"
            ))
        })?;
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
        // Repeating cleanup must settle once the fields are unchanged.
        let unchanged = matches!(
            &func.insts[t],
            Inst::MakeValueStruct { fields, .. } | Inst::MaterializeValueStruct { fields, .. }
                if *fields == leaves
        );
        if unchanged {
            continue;
        }
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
    let remap: HashMap<Idx<Inst>, Idx<Inst>> = replacement
        .iter()
        .map(|(k, v)| (*k, *v.last().unwrap()))
        .collect();
    let m = |o: Idx<Inst>| remap.get(&o).copied().unwrap_or(o);
    for (_, inst) in func.insts.iter_mut() {
        inst.remap_uses(&m);
    }
    for (_, block) in func.blocks.iter_mut() {
        block.term.remap_uses(&m);
    }
    crate::visit::splice_replacements(func, &replacement);
    Ok(true)
}

// A promoted cell can expose a nobox constructor to projections in another
// block. Eliminate only those projections: general cleanup can fold ordinary
// value constructors that a ref call has since mutated.
fn fold_nobox_projections(ws: &Workspace, func: &mut MirFunc) {
    let mut subst = Vec::new();
    for (_, block) in func.blocks.iter() {
        for &iid in &block.insts {
            let Some((base, field)) = func.field_projection(iid) else {
                continue;
            };
            let Inst::MakeValueStruct { name, fields, .. } = &func.insts[base] else {
                continue;
            };
            if !ws
                .nominals
                .struct_id(*name)
                .is_some_and(|sid| ws.modules[sid.module.index()].structs[sid.def].is_nobox)
            {
                continue;
            }
            if let Some((_, value)) = fields.iter().find(|(name, _)| *name == field) {
                if subst.is_empty() {
                    subst.resize(func.insts.len(), None);
                }
                subst[iid.into_raw().into_u32() as usize] = Some(*value);
            }
        }
    }
    if subst.is_empty() {
        return;
    }
    crate::copy_prop::substitute_uses(func, &subst);
    for (_, block) in func.blocks.iter_mut() {
        block
            .insts
            .retain(|iid| subst[iid.into_raw().into_u32() as usize].is_none());
    }
    dce(func);
}

/// Prepare a body for emission and implicit nobox validation.
pub fn cleanup_with_nobox(ws: &Workspace, func: &mut MirFunc) -> Result<(), CompilerError> {
    cleanup_in(Some(ws), func)?;
    while flatten_nobox_nested_ctors(ws, func)? {
        cleanup_in(Some(ws), func)?;
    }
    if !skull_mir::pass_disabled("mem2reg") {
        promote_direct_cells(func);
    }
    if !skull_mir::pass_disabled("noboxfold") {
        fold_nobox_projections(ws, func);
    }
    // After promotion, so a rounding that reached the store through a cell
    // (an inlined constructor with a defaulted argument) is seen directly.
    if drop_f32_rounding_into_float_views(func) {
        dce(func);
    }
    Ok(())
}
