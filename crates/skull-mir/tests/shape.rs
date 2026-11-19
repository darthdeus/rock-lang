//! Hand-built MIR checked against its dump.

use skull_mir::mir::*;
use ustr::ustr;

#[test]
fn dump_covers_aggregates_and_containers() {
    let mut f = MirFunc::new(ustr("probe"));
    let bb = f.entry();
    let a = f.emit(bb, Inst::Const(Lit::Int(1)));
    let b = f.emit(bb, Inst::Const(Lit::Int(2)));
    let tup = f.emit(bb, Inst::MakeTuple(vec![a, b]));
    let vec = f.emit(bb, Inst::MakeVec(vec![a, b]));
    let st = f.emit(
        bb,
        Inst::MakeStruct {
            ty: Some(ustr("Foo")),
            fields: vec![(ustr("a"), a), (ustr("b"), b)],
        },
    );
    f.emit(
        bb,
        Inst::MakeEnum {
            tag: ustr("Some"),
            ty: ustr("E"),
            positional: vec![a],
            named: vec![],
        },
    );
    let fld = f.emit(
        bb,
        Inst::Field {
            nonthrowing: false,
            base: st,
            field: ustr("a"),
        },
    );
    f.emit(
        bb,
        Inst::TupleIndex {
            base: tup,
            index: 0,
        },
    );
    f.emit(
        bb,
        Inst::Index {
            base: vec,
            index: a,
            kind: ContainerKind::Vec,
        },
    );
    f.emit(
        bb,
        Inst::Len {
            base: vec,
            kind: ContainerKind::Vec,
        },
    );
    f.emit(
        bb,
        Inst::StoreField {
            base: st,
            field: ustr("a"),
            value: b,
        },
    );
    f.emit(
        bb,
        Inst::StoreIndex {
            base: vec,
            index: a,
            value: b,
            kind: ContainerKind::Vec,
        },
    );
    f.set_term(bb, Term::Return(vec![fld]));

    let expected = "\
fn probe() {
  bb0:
    v0 = const 1
    v1 = const 2
    v2 = tuple [v0, v1]
    v3 = vec [v0, v1]
    v4 = struct Foo {a=v0, b=v1}
    v5 = enum E::Some(v0)
    v6 = field v4.a
    v7 = tuple_index v2.0
    v8 = index.Vec v3[v0]
    v9 = len.Vec v3
    store_field v4.a, v1
    store_index.Vec v3[v0], v1
    return v6
}";
    assert_eq!(f.to_string(), expected);
}

#[test]
fn dump_covers_arith_unary_and_yield() {
    let mut f = MirFunc::new(ustr("probe"));
    let entry = f.entry();
    let s0 = f.new_block();
    let s1 = f.new_block();
    let resume = f.new_block();

    let a = f.emit(entry, Inst::Const(Lit::Float(1.5)));
    let b = f.emit(entry, Inst::Global(ustr("g")));
    let sum = f.emit(entry, Inst::FloatAdd(a, b));
    let neg = f.emit(entry, Inst::Neg(sum));
    let bits = f.emit(entry, Inst::BitXor(neg, a));
    let cond = f.emit(entry, Inst::Eq(bits, a));
    f.set_term(
        entry,
        Term::Branch {
            cond,
            then_bb: s0,
            else_bb: s1,
        },
    );

    f.set_term(
        s0,
        Term::Yield {
            values: vec![bits],
            resume,
        },
    );
    f.set_term(
        s1,
        Term::Yield {
            values: vec![],
            resume,
        },
    );
    f.set_term(resume, Term::Return(vec![]));

    let expected = "\
fn probe() {
  bb0:
    v0 = const 1.5
    v1 = global g
    v2 = float.add v0, v1
    v3 = neg v2
    v4 = bit.xor v3, v0
    v5 = eq v4, v0
    branch v5 -> bb1, bb2
  bb1:
    yield v4 -> bb3
  bb2:
    yield -> bb3
  bb3:
    return
}";
    assert_eq!(f.to_string(), expected);
}
