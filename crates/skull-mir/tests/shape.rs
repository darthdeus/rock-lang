//! Operand traversal and value classification for hand-built MIR.

use comfy_la_arena::Idx;
use skull_mir::mir::*;
use ustr::ustr;

#[test]
fn aggregate_and_container_operands_and_effects() {
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

    assert_operands(
        &f,
        bb,
        10,
        &[
            &[],
            &[],
            &[a, b],
            &[a, b],
            &[a, b],
            &[a],
            &[st],
            &[tup],
            &[vec, a],
            &[vec],
            &[st, b],
            &[vec, a, b],
        ],
    );
    assert_eq!(f.blocks[bb].term, Term::Return(vec![fld]));
}

#[test]
fn arithmetic_and_yield_operands() {
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

    assert_operands(
        &f,
        entry,
        6,
        &[&[], &[], &[a, b], &[sum], &[neg, a], &[bits, a]],
    );
    assert_eq!(
        f.blocks[entry].term,
        Term::Branch {
            cond,
            then_bb: s0,
            else_bb: s1
        },
    );
    for (block, values) in [(s0, vec![bits]), (s1, vec![])] {
        let term = &f.blocks[block].term;
        let mut used = Vec::new();
        term.uses_each(&mut |value| used.push(value));
        assert_eq!(used, values);
        assert_eq!(*term, Term::Yield { values, resume });
    }
    assert_eq!(f.blocks[resume].term, Term::Return(vec![]));
}

fn assert_operands(f: &MirFunc, block: Idx<Block>, value_count: usize, operands: &[&[Idx<Inst>]]) {
    let insts = &f.blocks[block].insts;
    assert_eq!(insts.len(), operands.len());
    for (index, (&id, &want)) in insts.iter().zip(operands).enumerate() {
        let inst = &f.insts[id];
        let mut used = Vec::new();
        inst.uses_each(&mut |value| used.push(value));
        assert_eq!(used, want, "{inst:?}");
        assert_eq!(inst.produces_value(), index < value_count, "{inst:?}");
    }
}
