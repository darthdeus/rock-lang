//! `cleanup` reuses one `VecCheckedIndex` for a repeated element access in a
//! block, and only while nothing between the two can change the Vec's length.
use comfy_la_arena::Idx;
use skull_mir::mir::{Block, ContainerKind, Inst, Lit, MirFunc, Term};
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
            base: p.vec,
            field: Ustr::from("b"),
        },
    );
    p.body.set_term(p.block, Term::Return(vec![a]));
    skull_transform::cleanup(&mut p.body).unwrap();
    assert!(!p.body.blocks[p.block].insts.contains(&unused));
    assert_eq!(checks(&p), 1);
}

#[test]
fn an_unchecked_field_read_keeps_its_nil_error() {
    let mut p = probe();
    let unused = p.body.emit(
        p.block,
        Inst::Field {
            base: p.vec,
            field: Ustr::from("b"),
        },
    );
    p.body.set_term(p.block, Term::Return(vec![]));
    skull_transform::cleanup(&mut p.body).unwrap();
    assert!(p.body.blocks[p.block].insts.contains(&unused));
}

#[test]
fn a_different_index_is_checked_again() {
    let mut p = probe();
    let i = p.body.emit(p.block, Inst::Global(Ustr::from("i")));
    let j = p.body.emit(p.block, Inst::Global(Ustr::from("j")));
    let (_, a) = leaf_read(&mut p, i, "a");
    let (_, b) = leaf_read(&mut p, j, "b");
    let sum = p.body.emit(p.block, Inst::FloatAdd(a, b));
    p.body.set_term(p.block, Term::Return(vec![sum]));
    skull_transform::cleanup(&mut p.body).unwrap();
    assert_eq!(checks(&p), 2);
}

#[test]
fn a_column_slot_write_keeps_the_check() {
    let p = sum_with(|p| {
        let i = p.body.emit(p.block, Inst::Global(Ustr::from("i")));
        let check = p.body.emit(
            p.block,
            Inst::VecCheckedIndex {
                vec: p.vec,
                index: i,
            },
        );
        let col = p.body.emit(
            p.block,
            Inst::Field {
                base: p.vec,
                field: Ustr::from("a"),
            },
        );
        let one = p.body.emit(p.block, Inst::Const(Lit::Float(1.0)));
        p.body.emit(
            p.block,
            Inst::StoreIndex {
                base: col,
                index: check,
                value: one,
                kind: ContainerKind::VecColumn,
            },
        );
    });
    // `i` is read twice from a global, so the write's check is distinct
    // from the reads' and survives, but the second read reuses the first.
    assert_eq!(checks(&p), 2, "{:#?}", p.body.blocks[p.block].insts);
}

#[test]
fn anything_that_may_change_the_length_invalidates_the_check() {
    let barriers: Vec<(&str, fn(&mut Probe))> = vec![
        ("push", |p| {
            let one = p.body.emit(p.block, Inst::Const(Lit::Float(1.0)));
            p.body.emit(
                p.block,
                Inst::ArrayPush {
                    arr: p.vec,
                    value: one,
                    kind: ContainerKind::Vec,
                },
            );
        }),
        ("len store", |p| {
            let zero = p.body.emit(p.block, Inst::Const(Lit::Int(0)));
            p.body.emit(
                p.block,
                Inst::StoreField {
                    base: p.vec,
                    field: Ustr::from("len"),
                    value: zero,
                },
            );
        }),
        ("call through a global", |p| {
            let f = p.body.emit(p.block, Inst::Global(Ustr::from("pop_all")));
            p.body.emit(
                p.block,
                Inst::CallValue {
                    callee: f,
                    args: vec![],
                },
            );
        }),
        ("whole-element store", |p| {
            let i = p.body.emit(p.block, Inst::Global(Ustr::from("i")));
            let one = p.body.emit(p.block, Inst::Const(Lit::Float(1.0)));
            p.body.emit(
                p.block,
                Inst::StoreIndex {
                    base: p.vec,
                    index: i,
                    value: one,
                    kind: ContainerKind::Vec,
                },
            );
        }),
    ];
    for (name, between) in barriers {
        let p = sum_with(between);
        assert_eq!(checks(&p), 2, "{name}: {:#?}", p.body.blocks[p.block].insts);
    }
}
