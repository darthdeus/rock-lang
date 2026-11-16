//! Return destinations preserve the ordinary ABI while avoiding transient
//! expression cells and aggregates before cleanup has a chance to erase them.
use skull_mir::mir::{Inst, MirFunc, Term};

fn lower(src: &str) -> MirFunc {
    crate::common::lower(src, "pick")
}

#[test]
fn tail_branches_forward_the_return_destination() {
    let mir = lower("fn pick(flag: bool, a: int, b: int) -> int { if flag { a } else { b } }");
    assert_eq!(mir.blocks.len(), 3, "{mir}");
    assert_eq!(mir.insts.len(), 3, "{mir}");
    assert!(
        !mir.insts
            .iter()
            .any(|(_, i)| matches!(i, Inst::Store { .. })),
        "{mir}"
    );
    assert_eq!(
        mir.blocks
            .iter()
            .filter(|(_, b)| matches!(b.term, Term::Return(_)))
            .count(),
        2
    );
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
