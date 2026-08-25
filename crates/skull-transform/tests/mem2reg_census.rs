use skull_mir::mir::{Inst, Lit, MirFunc, Term};
use skull_transform::{mem2reg_census, promote_direct_cells};

#[test]
fn census_separates_direct_promotion_from_merge_and_non_dominance() {
    let mut f = MirFunc::new(ustr::ustr("mem2reg_census"));
    let param = f.new_cell(None);
    f.params.push(param);
    let direct = f.new_cell(None);
    let merged = f.new_cell(None);
    let conditional = f.new_cell(None);

    let left = f.new_block();
    let right = f.new_block();
    let join = f.new_block();

    let seven = f.emit(f.entry, Inst::Const(Lit::Int(7)));
    f.emit(
        f.entry,
        Inst::Store {
            cell: direct,
            value: seven,
        },
    );
    let cond = f.emit(f.entry, Inst::Const(Lit::Bool(true)));
    f.set_term(
        f.entry,
        Term::Branch {
            cond,
            then_bb: left,
            else_bb: right,
        },
    );

    let one = f.emit(left, Inst::Const(Lit::Int(1)));
    f.emit(
        left,
        Inst::Store {
            cell: merged,
            value: one,
        },
    );
    f.emit(
        left,
        Inst::Store {
            cell: conditional,
            value: one,
        },
    );
    f.set_term(left, Term::Goto(join));

    let two = f.emit(right, Inst::Const(Lit::Int(2)));
    f.emit(
        right,
        Inst::Store {
            cell: merged,
            value: two,
        },
    );
    f.set_term(right, Term::Goto(join));

    let p = f.emit(join, Inst::Load(param));
    let d = f.emit(join, Inst::Load(direct));
    let m = f.emit(join, Inst::Load(merged));
    let c = f.emit(join, Inst::Load(conditional));
    f.set_term(join, Term::Return(vec![p, d, m, c]));

    let r = mem2reg_census(&f);
    assert_eq!(r.loads, 4);
    assert_eq!(r.stores, 4);
    assert_eq!(r.direct_promotable_cells, 2);
    assert_eq!(r.direct_promotable_loads, 2);
    assert_eq!(r.parameter_promotable_cells, 1);
    assert_eq!(r.parameter_promotable_loads, 1);
    assert_eq!(r.store_promotable_cells, 1);
    assert_eq!(r.store_promotable_loads, 1);
    assert_eq!(r.cross_block_promotable_loads, 2);
    assert_eq!(r.multi_definition_cells, 1);
    assert_eq!(r.non_dominating_single_definition_cells, 1);
    assert_eq!(r.current_cell_slots, 4);
    assert_eq!(r.guaranteed_removable_cell_slots, 1);
}

#[test]
fn direct_promotion_replaces_dominated_loads_but_keeps_merges() {
    let mut f = MirFunc::new(ustr::ustr("direct_promotion"));
    let direct = f.new_cell(None);
    let merged = f.new_cell(None);
    let left = f.new_block();
    let right = f.new_block();
    let join = f.new_block();

    let seven = f.emit(f.entry, Inst::Const(Lit::Int(7)));
    f.emit(
        f.entry,
        Inst::Store {
            cell: direct,
            value: seven,
        },
    );
    let cond = f.emit(f.entry, Inst::Const(Lit::Bool(true)));
    f.set_term(
        f.entry,
        Term::Branch {
            cond,
            then_bb: left,
            else_bb: right,
        },
    );

    let one = f.emit(left, Inst::Const(Lit::Int(1)));
    f.emit(
        left,
        Inst::Store {
            cell: merged,
            value: one,
        },
    );
    f.set_term(left, Term::Goto(join));

    let two = f.emit(right, Inst::Const(Lit::Int(2)));
    f.emit(
        right,
        Inst::Store {
            cell: merged,
            value: two,
        },
    );
    f.set_term(right, Term::Goto(join));

    let direct_load = f.emit(join, Inst::Load(direct));
    let merged_load = f.emit(join, Inst::Load(merged));
    f.set_term(join, Term::Return(vec![direct_load, merged_load]));

    let promoted = promote_direct_cells(&mut f);
    assert_eq!(promoted.cells, 1);
    assert_eq!(promoted.loads, 1);
    assert_eq!(f.blocks[join].term, Term::Return(vec![seven, merged_load]));
    assert!(
        f.blocks
            .iter()
            .flat_map(|(_, block)| block.insts.iter())
            .all(|&id| !matches!(f.insts[id], Inst::Store { cell, .. } if cell == direct))
    );
    assert!(f.blocks[join].insts.contains(&merged_load));
}

#[test]
fn direct_promotion_keeps_cells_observed_by_detached_scopes() {
    let mut f = MirFunc::new(ustr::ustr("captured_direct"));
    let captured = f.new_cell(None);
    let detached = f.new_block();

    let seven = f.emit(f.entry, Inst::Const(Lit::Int(7)));
    f.emit(
        f.entry,
        Inst::Store {
            cell: captured,
            value: seven,
        },
    );
    let main_load = f.emit(f.entry, Inst::Load(captured));
    f.set_term(f.entry, Term::Return(vec![main_load]));

    let detached_load = f.emit(detached, Inst::Load(captured));
    f.set_term(detached, Term::Return(vec![detached_load]));

    assert_eq!(promote_direct_cells(&mut f).cells, 0);
    assert!(f.blocks[f.entry].insts.contains(&main_load));
    assert!(f.blocks[detached].insts.contains(&detached_load));
}

#[test]
fn direct_promotion_keeps_cells_mutated_through_a_field() {
    let mut f = MirFunc::new(ustr::ustr("field_mutation"));
    let aggregate = f.new_cell(None);
    let next = f.new_block();

    let seed = f.emit(
        f.entry,
        Inst::MakeStruct {
            ty: Some(ustr::ustr("Aggregate")),
            fields: Vec::new(),
        },
    );
    f.emit(
        f.entry,
        Inst::Store {
            cell: aggregate,
            value: seed,
        },
    );
    let place = f.emit(f.entry, Inst::Load(aggregate));
    let one = f.emit(f.entry, Inst::Const(Lit::Int(1)));
    f.emit(
        f.entry,
        Inst::StoreField {
            base: place,
            field: ustr::ustr("value"),
            value: one,
        },
    );
    f.set_term(f.entry, Term::Goto(next));

    let readback = f.emit(next, Inst::Load(aggregate));
    f.set_term(next, Term::Return(vec![readback]));

    assert_eq!(promote_direct_cells(&mut f).cells, 0);
    assert!(f.blocks[f.entry].insts.contains(&place));
    assert!(f.blocks[next].insts.contains(&readback));
}
