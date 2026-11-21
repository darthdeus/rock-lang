//! `cleanup` reuses one `VecCheckedIndex` for a repeated element access in a
//! block, and only while nothing between the two can change the Vec's length.
use comfy_la_arena::Idx;
use skull_mir::mir::{Block, ContainerKind, Inst, MirFunc, Term};
use ustr::Ustr;

struct Probe {
    body: MirFunc,
    block: Idx<Block>,
    vec: Idx<Inst>,
}

fn probe() -> Probe {
    let mut body = MirFunc::new(Ustr::from("probe"));
    let block = body.entry;
    let vec = body.emit(block, Inst::Global(Ustr::from("xs")));
    Probe { body, block, vec }
}

/// One column-leaf read `vec.<field>[check(vec, index)]`, as the column lowering
/// emits it, returning the check and the read.
fn leaf_read(p: &mut Probe, index: Idx<Inst>, field: &str) -> (Idx<Inst>, Idx<Inst>) {
    let check = p
        .body
        .emit(p.block, Inst::VecCheckedIndex { vec: p.vec, index });
    let col = p.body.emit(
        p.block,
        Inst::Field {
            nonthrowing: false,
            base: p.vec,
            field: Ustr::from(field),
        },
    );
    let read = p.body.emit(
        p.block,
        Inst::Index {
            base: col,
            index: check,
            kind: ContainerKind::VecColumn,
        },
    );
    (check, read)
}

fn checks(p: &Probe) -> usize {
    p.body.blocks[p.block]
        .insts
        .iter()
        .filter(|&&i| matches!(p.body.insts[i], Inst::VecCheckedIndex { .. }))
        .count()
}

/// `xs[i].a + xs[i].b` with `between` emitted between the two element reads.
fn sum_with(between: impl FnOnce(&mut Probe)) -> Probe {
    let mut p = probe();
    let i = p.body.emit(p.block, Inst::Global(Ustr::from("i")));
    let (_, a) = leaf_read(&mut p, i, "a");
    between(&mut p);
    let (_, b) = leaf_read(&mut p, i, "b");
    let sum = p.body.emit(p.block, Inst::FloatAdd(a, b));
    p.body.set_term(p.block, Term::Return(vec![sum]));
    skull_transform::cleanup(&mut p.body).unwrap();
    p
}

#[test]
fn a_repeated_element_access_checks_once() {
    let p = sum_with(|_| {});
    assert_eq!(checks(&p), 1, "{:#?}", p.body.blocks[p.block].insts);
}

#[test]
fn unused_column_loads_after_the_check_are_dropped() {
    let mut p = probe();
    let i = p.body.emit(p.block, Inst::Global(Ustr::from("i")));
    let (_, a) = leaf_read(&mut p, i, "a");
    let unused = p.body.emit(
        p.block,
        Inst::Field {
            nonthrowing: false,
            base: p.vec,
            field: Ustr::from("b"),
        },
    );
    p.body.set_term(p.block, Term::Return(vec![a]));
    skull_transform::cleanup(&mut p.body).unwrap();
    assert!(!p.body.blocks[p.block].insts.contains(&unused));
    assert_eq!(checks(&p), 1);
}
