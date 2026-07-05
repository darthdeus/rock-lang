//! Phase-0 shape contracts for the cell-model IR + dump.
//!
//! These hand-build MIR and assert the dump. They pin the IR *contract* that
//! Phase-2 lowering must reproduce — so when lowering lands, an
//! `assert_eq!(lower(src), <this shape>)` test goes red the moment lowering
//! emits the wrong thing. Today they also exercise the dump (the oracle every
//! later test reads through).

use skull_mir::mir::*;
use ustr::ustr;

/// P1 (bone-mir-design D1/D4): the value of an `if`-as-value flows through a
/// **cell** — store-in-each-arm + load-after — NOT a block param. The IR makes
/// this the *only* representable shape: [`Block`] has no params field, so a
/// phi/block-param join is unrepresentable by construction.
///
/// Source contract:  `fn sign(x: int) -> int { if x < 0 { 0 } else { 1 } }`
#[test]
fn if_as_value_joins_through_a_cell_not_block_params() {
    let mut f = MirFunc::new(ustr("sign"));

    let x = f.new_cell(Some(ustr("x"))); // c0: param cell
    f.params.push(x);
    let result = f.new_cell(Some(ustr("result"))); // c1: the if-as-value result

    let entry = f.entry();
    let then_bb = f.new_block();
    let else_bb = f.new_block();
    let join = f.new_block();

    // entry: compute `x < 0`, branch.
    let v_x = f.emit(entry, Inst::Load(x));
    let v_zero = f.emit(entry, Inst::Const(Lit::Int(0)));
    let v_lt = f.emit(entry, Inst::IntLt(v_x, v_zero));
    f.set_term(
        entry,
        Term::Branch {
            cond: v_lt,
            then_bb,
            else_bb,
        },
    );

    // then-arm: result = 0
    let v0 = f.emit(then_bb, Inst::Const(Lit::Int(0)));
    f.emit(
        then_bb,
        Inst::Store {
            cell: result,
            value: v0,
        },
    );
    f.set_term(then_bb, Term::Goto(join));

    // else-arm: result = 1
    let v1 = f.emit(else_bb, Inst::Const(Lit::Int(1)));
    f.emit(
        else_bb,
        Inst::Store {
            cell: result,
            value: v1,
        },
    );
    f.set_term(else_bb, Term::Goto(join));

    // join: load the cell, return it.
    let v_res = f.emit(join, Inst::Load(result));
    f.set_term(join, Term::Return(vec![v_res]));

    let expected = "\
fn sign(c0) {
  bb0:
    v0 = load c0
    v1 = const 0
    v2 = int.lt v0, v1
    branch v2 -> bb1, bb2
  bb1:
    v3 = const 0
    store c1, v3
    goto bb3
  bb2:
    v4 = const 1
    store c1, v4
    goto bb3
  bb3:
    v5 = load c1
    return v5
}";
    assert_eq!(f.to_string(), expected);
}

/// Exercises every aggregate / projection / container dump arm, so the oracle
/// is trustworthy before Phase-2.5 lowering relies on it.
#[test]
fn dump_covers_aggregates_and_containers() {
    let mut f = MirFunc::new(ustr("probe"));
    let bb = f.entry();
    let a = f.emit(bb, Inst::Const(Lit::Int(1)));
    let b = f.emit(bb, Inst::Const(Lit::Int(2)));
    let tup = f.emit(bb, Inst::MakeTuple(vec![a, b]));
    let vec = f.emit(bb, Inst::MakeVec(vec![a, b]));
    f.emit(bb, Inst::MakeArray(vec![a]));
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
    let fld = f.emit(bb, Inst::Field { base: st, field: ustr("a") });
    f.emit(bb, Inst::TupleIndex { base: tup, index: 0 });
    f.emit(bb, Inst::Index { base: vec, index: a, kind: ContainerKind::Vec });
    f.emit(bb, Inst::Len { base: vec, kind: ContainerKind::Vec });
    f.emit(bb, Inst::StoreField { base: st, field: ustr("a"), value: b });
    f.emit(bb, Inst::StoreIndex { base: vec, index: a, value: b, kind: ContainerKind::Vec });
    f.set_term(bb, Term::Return(vec![fld]));

    let expected = "\
fn probe() {
  bb0:
    v0 = const 1
    v1 = const 2
    v2 = tuple [v0, v1]
    v3 = vec [v0, v1]
    v4 = array [v0]
    v5 = struct Foo {a=v0, b=v1}
    v6 = enum E::Some(v0)
    v7 = field v5.a
    v8 = tuple_index v2.0
    v9 = index.Vec v3[v0]
    v10 = len.Vec v3
    store_field v5.a, v1
    store_index.Vec v3[v0], v1
    return v7
}";
    assert_eq!(f.to_string(), expected);
}

/// Exercises the remaining dump arms (typed arith, unary, bitwise, global,
/// `Switch`, `Yield`) so the oracle is trustworthy before lowering relies on it.
#[test]
fn dump_covers_arith_unary_switch_and_yield() {
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
    f.set_term(
        entry,
        Term::Switch {
            discr: bits,
            cases: vec![(Lit::Int(0), s0), (Lit::Int(1), s1)],
            default: resume,
        },
    );

    f.set_term(s0, Term::Yield {
        value: Some(bits),
        resume,
    });
    f.set_term(s1, Term::Yield {
        value: None,
        resume,
    });
    f.set_term(resume, Term::Return(vec![]));

    let expected = "\
fn probe() {
  bb0:
    v0 = const 1.5
    v1 = global g
    v2 = float.add v0, v1
    v3 = neg v2
    v4 = bit.xor v3, v0
    switch v4 [0 -> bb1, 1 -> bb2] else bb3
  bb1:
    yield v4 -> bb3
  bb2:
    yield -> bb3
  bb3:
    return
}";
    assert_eq!(f.to_string(), expected);
}
