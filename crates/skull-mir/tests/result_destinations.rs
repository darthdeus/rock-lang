//! Return destinations preserve the ordinary ABI while avoiding transient
//! expression cells and aggregates before cleanup has a chance to erase them.
use std::sync::Arc;

use skull_mir::instance::Instance;
use skull_mir::lower::{lower_instance_with_plans, ScalarCtx};
use skull_mir::mir::{FuncRef, Inst, MirFunc, Term};
use skull_mir::monoty::MonoInterner;

fn lower(src: &str) -> MirFunc {
    let ast = skull_parser::parse_module_clean(src, ustr::Ustr::from("<test>"))
        .expect("parse");
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut vec![ast])
        .expect("semantic pass");
    let mid = ws.module_ids[&ustr::Ustr::from("<test>")];
    let (fid, _) = ws.module(mid).functions.iter().find(|(_, f)| f.name.as_str() == "pick").unwrap();
    let root = FuncRef { module: mid, func: fid };
    let interner = Arc::new(MonoInterner::new());
    let graph = skull_graph::instance_graph::build_in(&ws, vec![root], false, interner.clone());
    let concrete = skull_graph::concrete_body::build(&ws, &graph);
    let scx = ScalarCtx::new(&ws).sharing(interner);
    lower_instance_with_plans(&ws, &scx, &concrete.plans[&Instance::base(root)], &concrete.plans)
        .expect("lower")
}

#[test]
fn tail_branches_forward_the_return_destination() {
    let mir = lower("fn pick(flag: bool, a: int, b: int) -> int { if flag { a } else { b } }");
    assert_eq!(mir.blocks.len(), 3, "{mir}");
    assert_eq!(mir.insts.len(), 3, "{mir}");
    assert!(!mir.insts.iter().any(|(_, i)| matches!(i, Inst::Store { .. })), "{mir}");
    assert_eq!(mir.blocks.iter().filter(|(_, b)| matches!(b.term, Term::Return(_))).count(), 2);
}

#[test]
fn tuple_destination_uses_operands_without_construction() {
    let mir = lower("fn pick(flag: bool) -> (int, bool) { return if flag { (1, false) } else { (2, true) } }");
    assert!(!mir.insts.iter().any(|(_, i)| matches!(i, Inst::MakeTuple(_) | Inst::TupleIndex { .. } | Inst::Store { .. })), "{mir}");
    for (_, block) in mir.blocks.iter() {
        if let Term::Return(values) = &block.term {
            assert_eq!(values.len(), 2, "{mir}");
        }
    }
}
