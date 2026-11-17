use std::cell::OnceCell;

use comfy_la_arena::Idx;
use rustc_hash::FxHashSet as HashSet;
use skull_mir::cfg::{dominators, reachable_blocks_from};
use skull_mir::mir::{Block, Cell, Inst, MirFunc};

use crate::visit::raw;

#[derive(Clone, Default)]
struct CellUses {
    store: Option<(Idx<Block>, usize, Idx<Inst>)>,
    loads: Vec<(Idx<Block>, usize, Idx<Inst>)>,
    excluded: bool,
}

fn loaded_root(func: &MirFunc, mut value: Idx<Inst>) -> Option<Idx<Cell>> {
    loop {
        match &func.insts[value] {
            Inst::Load(cell) => return Some(*cell),
            Inst::Field { base, .. } | Inst::TupleIndex { base, .. } => value = *base,
            _ => return None,
        }
    }
}

pub(crate) fn promote_direct_cells(func: &mut MirFunc) {
    if func.is_coroutine || func.cells.is_empty() {
        return;
    }
    let reachable: HashSet<_> = reachable_blocks_from(func, func.entry())
        .into_iter()
        .collect();
    let mut cells = vec![CellUses::default(); func.cells.len()];
    for &cell in func.params.iter().chain(&func.inline_cleanup_load_cells) {
        cells[raw(cell)].excluded = true;
    }
    for (block, body) in func.blocks.iter() {
        if !reachable.contains(&block) {
            for &id in &body.insts {
                func.insts[id].cells_each(&mut |cell| cells[raw(cell)].excluded = true);
            }
            continue;
        }
        for (pos, &id) in body.insts.iter().enumerate() {
            let inst = &func.insts[id];
            match inst {
                Inst::Load(cell) => cells[raw(*cell)].loads.push((block, pos, id)),
                Inst::Store { cell, value } => {
                    let uses = &mut cells[raw(*cell)];
                    uses.excluded |= uses.store.replace((block, pos, *value)).is_some();
                }
                Inst::StoreField { base, .. } => {
                    if let Some(cell) = loaded_root(func, *base) {
                        cells[raw(cell)].excluded = true;
                    }
                }
                _ => inst.cell_writes_each(&mut |cell| cells[raw(cell)].excluded = true),
            }
        }
    }

    let idom = OnceCell::new();
    let mut subst = Vec::new();
    for uses in cells {
        let Some((block, pos, value)) = uses.store else {
            continue;
        };
        if uses.excluded || uses.loads.is_empty() {
            continue;
        }
        if !uses.loads.iter().all(|&(load_block, load_pos, _)| {
            if block == load_block {
                pos < load_pos
            } else {
                idom.get_or_init(|| dominators(func, func.entry()))
                    .dominates(block, load_block)
            }
        }) {
            continue;
        }
        if subst.is_empty() {
            subst.resize(func.insts.len(), None);
        }
        for (_, _, load) in uses.loads {
            subst[raw(load)] = Some(value);
        }
    }
    if !subst.is_empty() {
        super::copy_prop::substitute_uses(func, &subst);
        super::dce(func);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use skull_mir::mir::{Lit, Term};

    #[test]
    fn a_load_after_a_bypassed_store_returns_nil() {
        let (ws, interner, _) =
            crate::test_support::lower_fixture("fn fixture() {}", "<promotion-test>", "fixture");
        let mut body = MirFunc::new("join".into());
        let flag = body.new_cell(Some("flag".into()));
        body.params.push(flag);
        let cell = body.new_cell(None);
        let store = body.new_block();
        let join = body.new_block();
        let cond = body.emit(body.entry, Inst::Load(flag));
        body.set_term(
            body.entry,
            Term::Branch {
                cond,
                then_bb: store,
                else_bb: join,
            },
        );
        let value = body.emit(store, Inst::Const(Lit::Int(7)));
        body.emit(store, Inst::Store { cell, value });
        body.set_term(store, Term::Goto(join));
        let load = body.emit(join, Inst::Load(cell));
        body.set_term(join, Term::Return(vec![load]));
        promote_direct_cells(&mut body);

        let ctx = skull_codegen_lua::CodegenCtx::new(&ws);
        let lua = skull_codegen_lua::emit_named(&ws, &ctx, &interner, "join", &body, None);
        let output = std::process::Command::new("luajit")
            .args([
                "-e",
                &format!("{lua}\nassert(join(false) == nil and join(true) == 7)"),
            ])
            .output()
            .expect("run LuaJIT");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
