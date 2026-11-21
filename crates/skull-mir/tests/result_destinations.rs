//! Return destinations preserve the ordinary ABI while avoiding transient
//! expression cells and aggregates before cleanup has a chance to erase them.
use skull_mir::mir::{Inst, MirFunc, Term};

fn lower(src: &str) -> MirFunc {
    crate::common::lower(src, "pick")
}

#[test]
fn tuple_destination_uses_operands_without_construction() {
    let mir = lower(
        "fn pick(flag: bool) -> (int, bool) { return if flag { (1, false) } else { (2, true) } }",
    );
    assert!(
        !mir.insts.iter().any(|(_, i)| matches!(
            i,
            Inst::MakeTuple(_) | Inst::TupleIndex { .. } | Inst::Store { .. }
        )),
        "{mir}"
    );
    for (_, block) in mir.blocks.iter() {
        if let Term::Return(values) = &block.term {
            assert_eq!(values.len(), 2, "{mir}");
        }
    }
}
