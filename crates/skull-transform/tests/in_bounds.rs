//! `elide_bounds_checks` marks a `[T]` element access `VecInBounds` only when
//! its index is provably within `[0, len)` on every path, and keeps the check
//! wherever the body could have shrunk or rebound the vector in between.
use comfy_la_arena::Idx;
use skull_mir::mir::{Block, Cell, ContainerKind, Inst, Lit, MirFunc, Term};
use skull_transform::elide_bounds_checks;
use ustr::Ustr;

fn u(s: &str) -> Ustr {
    Ustr::from(s)
}

struct Probe {
    f: MirFunc,
    xs: Idx<Cell>,
    ys: Idx<Cell>,
    i: Idx<Cell>,
}

impl Probe {
    fn new() -> Probe {
        let mut f = MirFunc::new(u("probe"));
        let xs = f.new_cell(Some(u("xs")));
        let ys = f.new_cell(Some(u("ys")));
        let i = f.new_cell(Some(u("i")));
        f.params.push(xs);
        f.params.push(ys);
        f.params.push(i);
        Probe { f, xs, ys, i }
    }

    fn emit(&mut self, b: Idx<Block>, inst: Inst) -> Idx<Inst> {
        self.f.emit(b, inst)
    }

    fn load(&mut self, b: Idx<Block>, cell: Idx<Cell>) -> Idx<Inst> {
        self.emit(b, Inst::Load(cell))
    }

    fn int(&mut self, b: Idx<Block>, n: i64) -> Idx<Inst> {
        self.emit(b, Inst::Const(Lit::Int(n)))
    }

    fn store(&mut self, b: Idx<Block>, cell: Idx<Cell>, value: Idx<Inst>) {
        self.emit(b, Inst::Store { cell, value });
    }

    /// `<vec>.len`, as `xs.len()` lowers.
    fn len_of(&mut self, b: Idx<Block>, vec: Idx<Cell>) -> Idx<Inst> {
        let base = self.load(b, vec);
        self.emit(
            b,
            Inst::Field {
                base,
                field: u("len"),
            },
        )
    }

    /// `vec[index] = 1`, returning the store.
    fn set(&mut self, b: Idx<Block>, vec: Idx<Cell>, index: Idx<Cell>) -> Idx<Inst> {
        let base = self.load(b, vec);
        let index = self.load(b, index);
        let value = self.int(b, 1);
        self.emit(
            b,
            Inst::StoreIndex {
                base,
                index,
                value,
                kind: ContainerKind::VecNonNull,
            },
        )
    }

    /// `vec[index]`, returning the read.
    fn get(&mut self, b: Idx<Block>, vec: Idx<Cell>, index: Idx<Cell>) -> Idx<Inst> {
        let base = self.load(b, vec);
        let index = self.load(b, index);
        self.emit(
            b,
            Inst::Index {
                base,
                index,
                kind: ContainerKind::VecNonNull,
            },
        )
    }

    /// A call to an unknown function value.
    fn unknown_call(&mut self, b: Idx<Block>) {
        let callee = self.load(b, self.ys);
        self.emit(
            b,
            Inst::CallValue {
                callee,
                args: vec![],
            },
        );
    }

    fn kind(&self, access: Idx<Inst>) -> ContainerKind {
        match self.f.insts[access] {
            Inst::Index { kind, .. } | Inst::StoreIndex { kind, .. } => kind,
            ref other => panic!("not an access: {other:?}"),
        }
    }

    fn run(&mut self) {
        elide_bounds_checks(None, &mut self.f);
    }
}

/// `for i in 0..xs.len() { <body(B)>; xs[i] = 1; <after(B)> }` with the
/// `lower_for_range` shape: the bound snapshot in a cell, the counter marked
/// as a loop binding, and a pure-increment latch.
struct CountedLoop {
    body: Idx<Block>,
    latch: Idx<Block>,
    exit: Idx<Block>,
}

fn counted_loop(
    p: &mut Probe,
    before: impl FnOnce(&mut Probe, Idx<Block>),
) -> (CountedLoop, Idx<Inst>) {
    let entry = p.f.entry;
    let header = p.f.new_block();
    let body = p.f.new_block();
    let latch = p.f.new_block();
    let exit = p.f.new_block();
    let hi = p.f.new_cell(None);
    p.f.mark_loop_binding(p.i);
    let zero = p.int(entry, 0);
    p.store(entry, p.i, zero);
    let len = p.len_of(entry, p.xs);
    p.store(entry, hi, len);
    p.f.set_term(entry, Term::Goto(header));
    let li = p.load(header, p.i);
    let lh = p.load(header, hi);
    let cond = p.emit(header, Inst::IntLt(li, lh));
    p.f.set_term(
        header,
        Term::Branch {
            cond,
            then_bb: body,
            else_bb: exit,
        },
    );
    before(p, body);
    let store = p.set(body, p.xs, p.i);
    p.f.set_term(body, Term::Goto(latch));
    let li = p.load(latch, p.i);
    let one = p.int(latch, 1);
    let next = p.emit(latch, Inst::IntAdd(li, one));
    p.store(latch, p.i, next);
    p.f.set_term(latch, Term::Goto(header));
    p.f.set_term(exit, Term::Return(vec![]));
    (CountedLoop { body, latch, exit }, store)
}

#[test]
fn a_counted_loop_store_is_in_bounds() {
    let mut p = Probe::new();
    let (_, store) = counted_loop(&mut p, |_, _| {});
    p.run();
    assert_eq!(p.kind(store), ContainerKind::VecNonNullInBounds);
}

#[test]
fn an_unknown_call_in_the_body_keeps_the_check() {
    let mut p = Probe::new();
    let (_, store) = counted_loop(&mut p, |p, b| p.unknown_call(b));
    p.run();
    assert_eq!(p.kind(store), ContainerKind::VecNonNull);
}

#[test]
fn a_call_after_the_store_still_reaches_it_around_the_back_edge() {
    let mut p = Probe::new();
    let (lp, store) = counted_loop(&mut p, |_, _| {});
    p.f.set_term(lp.body, Term::Goto(lp.latch));
    p.unknown_call(lp.latch);
    p.run();
    assert_eq!(p.kind(store), ContainerKind::VecNonNull);
}

#[test]
fn a_literal_push_in_the_body_invalidates_bounds_facts() {
    let mut p = Probe::new();
    let (_, store) = counted_loop(&mut p, |p, b| {
        let arr = p.emit(b, Inst::MakeVec(Vec::new()));
        let value = p.int(b, 7);
        p.emit(
            b,
            Inst::ArrayPush {
                arr,
                value,
                kind: ContainerKind::VecNonNull,
            },
        );
    });
    p.run();
    assert_eq!(p.kind(store), ContainerKind::VecNonNull);
}

#[test]
fn a_push_to_an_external_vector_can_run_a_metamethod() {
    let mut p = Probe::new();
    let (_, store) = counted_loop(&mut p, |p, b| {
        let arr = p.load(b, p.ys);
        let value = p.int(b, 7);
        p.emit(
            b,
            Inst::ArrayPush {
                arr,
                value,
                kind: ContainerKind::Vec,
            },
        );
    });
    p.run();
    assert_eq!(p.kind(store), ContainerKind::VecNonNull);
}

#[test]
fn a_length_store_in_the_body_keeps_the_check() {
    let mut p = Probe::new();
    let (_, store) = counted_loop(&mut p, |p, b| {
        let base = p.load(b, p.ys);
        let value = p.int(b, 0);
        p.emit(
            b,
            Inst::StoreField {
                base,
                field: u("len"),
                value,
            },
        );
    });
    p.run();
    assert_eq!(p.kind(store), ContainerKind::VecNonNull);
}

#[test]
fn rebinding_the_vector_in_the_body_keeps_the_check() {
    let mut p = Probe::new();
    let (_, store) = counted_loop(&mut p, |p, b| {
        let other = p.load(b, p.ys);
        p.store(b, p.xs, other);
    });
    p.run();
    assert_eq!(p.kind(store), ContainerKind::VecNonNull);
}

#[test]
fn a_yield_in_the_body_keeps_the_check() {
    let mut p = Probe::new();
    let (lp, store) = counted_loop(&mut p, |_, _| {});
    // Split the body: the store now runs after a yield.
    let resume = p.f.new_block();
    let insts = std::mem::take(&mut p.f.blocks[lp.body].insts);
    p.f.blocks[resume].insts = insts;
    p.f.set_term(
        lp.body,
        Term::Yield {
            values: vec![],
            resume,
        },
    );
    p.f.set_term(resume, Term::Goto(lp.latch));
    p.run();
    assert_eq!(p.kind(store), ContainerKind::VecNonNull);
}

#[test]
fn an_early_exit_before_the_store_does_not_matter() {
    let mut p = Probe::new();
    let (lp, store) = counted_loop(&mut p, |_, _| {});
    let rest = p.f.new_block();
    let insts = std::mem::take(&mut p.f.blocks[lp.body].insts);
    p.f.blocks[rest].insts = insts;
    let li = p.load(lp.body, p.i);
    let two = p.int(lp.body, 2);
    let cond = p.emit(lp.body, Inst::Eq(li, two));
    p.f.set_term(
        lp.body,
        Term::Branch {
            cond,
            then_bb: lp.exit,
            else_bb: rest,
        },
    );
    p.f.set_term(rest, Term::Goto(lp.latch));
    p.run();
    assert_eq!(p.kind(store), ContainerKind::VecNonNullInBounds);
}

#[test]
fn a_second_vector_in_the_body_keeps_its_check() {
    let mut p = Probe::new();
    let (lp, store) = counted_loop(&mut p, |_, _| {});
    let other = p.set(lp.body, p.ys, p.i);
    p.run();
    assert_eq!(p.kind(store), ContainerKind::VecNonNullInBounds);
    assert_eq!(p.kind(other), ContainerKind::VecNonNull);
}

#[test]
fn a_counter_that_may_start_negative_keeps_the_check() {
    let mut p = Probe::new();
    let (_, store) = counted_loop(&mut p, |_, _| {});
    // Replace the `i = 0` init with `i = ys.len - 1`, which may be -1.
    let entry = p.f.entry;
    let len = p.len_of(entry, p.ys);
    let one = p.int(entry, 1);
    let init = p.emit(entry, Inst::IntSub(len, one));
    p.store(entry, p.i, init);
    p.run();
    assert_eq!(p.kind(store), ContainerKind::VecNonNull);
}

/// `if i < 0 { return }; if i >= xs.len { return }; xs[i] = 1`.
#[test]
fn a_dominating_guard_proves_the_store() {
    let mut p = Probe::new();
    let entry = p.f.entry;
    let second = p.f.new_block();
    let body = p.f.new_block();
    let exit = p.f.new_block();
    let li = p.load(entry, p.i);
    let zero = p.int(entry, 0);
    let neg = p.emit(entry, Inst::IntLt(li, zero));
    p.f.set_term(
        entry,
        Term::Branch {
            cond: neg,
            then_bb: exit,
            else_bb: second,
        },
    );
    let li = p.load(second, p.i);
    let len = p.len_of(second, p.xs);
    let past = p.emit(second, Inst::IntGeq(li, len));
    p.f.set_term(
        second,
        Term::Branch {
            cond: past,
            then_bb: exit,
            else_bb: body,
        },
    );
    let store = p.set(body, p.xs, p.i);
    let other = p.set(body, p.ys, p.i);
    p.f.set_term(body, Term::Return(vec![]));
    p.f.set_term(exit, Term::Return(vec![]));
    p.run();
    assert_eq!(p.kind(store), ContainerKind::VecNonNullInBounds);
    assert_eq!(p.kind(other), ContainerKind::VecNonNull);
}

/// `xs[i]` then `xs[i] = ..`: the read checked the slot.
#[test]
fn a_checked_read_proves_the_store_after_it() {
    let mut p = Probe::new();
    let b = p.f.entry;
    let read = p.get(b, p.xs, p.i);
    let store = p.set(b, p.xs, p.i);
    p.f.set_term(b, Term::Return(vec![read]));
    p.run();
    assert_eq!(p.kind(read), ContainerKind::VecNonNull);
    assert_eq!(p.kind(store), ContainerKind::VecNonNullInBounds);
}

#[test]
fn a_callback_capable_index_forgets_a_prior_bounds_proof() {
    for write in [false, true] {
        let mut p = Probe::new();
        let b = p.f.entry;
        let checked = p.get(b, p.xs, p.i);
        let base = p.load(b, p.ys);
        let index = p.load(b, p.i);
        if write {
            let value = p.int(b, 1);
            p.emit(
                b,
                Inst::StoreIndex {
                    base,
                    index,
                    value,
                    kind: ContainerKind::Vec,
                },
            );
        } else {
            p.emit(
                b,
                Inst::Index {
                    base,
                    index,
                    kind: ContainerKind::Vec,
                },
            );
        }
        let store = p.set(b, p.xs, p.i);
        p.f.set_term(b, Term::Return(vec![checked]));
        p.run();
        assert_eq!(p.kind(store), ContainerKind::VecNonNull);
    }
}

#[test]
fn a_call_between_a_read_and_a_store_keeps_the_check() {
    let mut p = Probe::new();
    let b = p.f.entry;
    let read = p.get(b, p.xs, p.i);
    p.unknown_call(b);
    let store = p.set(b, p.xs, p.i);
    p.f.set_term(b, Term::Return(vec![read]));
    p.run();
    assert_eq!(p.kind(store), ContainerKind::VecNonNull);
}

/// `while i < xs.len { xs[i] = 1; ys[i] = 1; i = i + 1 }` re-reads the live
/// length each turn, so no body effect can stale it.
#[test]
fn a_while_loop_against_the_live_length_proves_its_vector() {
    let mut p = Probe::new();
    let entry = p.f.entry;
    let header = p.f.new_block();
    let body = p.f.new_block();
    let exit = p.f.new_block();
    let zero = p.int(entry, 0);
    p.store(entry, p.i, zero);
    p.f.set_term(entry, Term::Goto(header));
    let li = p.load(header, p.i);
    let len = p.len_of(header, p.xs);
    let cond = p.emit(header, Inst::IntLt(li, len));
    p.f.set_term(
        header,
        Term::Branch {
            cond,
            then_bb: body,
            else_bb: exit,
        },
    );
    let store = p.set(body, p.xs, p.i);
    let other = p.set(body, p.ys, p.i);
    let li = p.load(body, p.i);
    let one = p.int(body, 1);
    let next = p.emit(body, Inst::IntAdd(li, one));
    p.store(body, p.i, next);
    p.f.set_term(body, Term::Goto(header));
    p.f.set_term(exit, Term::Return(vec![]));
    p.run();
    assert_eq!(p.kind(store), ContainerKind::VecNonNullInBounds);
    assert_eq!(p.kind(other), ContainerKind::VecNonNull);
}

/// `rows[r][c] = ..` after `c < rows[r].len`: the two `rows[r]` reads name
/// the same element while nothing stores into a container.
#[test]
fn an_element_of_an_element_is_a_place() {
    let mut p = Probe::new();
    let b = p.f.entry;
    let exit = p.f.new_block();
    let body = p.f.new_block();
    let rows = p.xs;
    let r = p.ys;
    let row = p.get(b, rows, r);
    let len = p.emit(
        b,
        Inst::Field {
            base: row,
            field: u("len"),
        },
    );
    let zero = p.int(b, 0);
    p.store(b, p.i, zero);
    let li = p.load(b, p.i);
    let cond = p.emit(b, Inst::IntLt(li, len));
    p.f.set_term(
        b,
        Term::Branch {
            cond,
            then_bb: body,
            else_bb: exit,
        },
    );
    let row2 = p.get(body, rows, r);
    let index = p.load(body, p.i);
    let value = p.int(body, 1);
    let store = p.emit(
        body,
        Inst::StoreIndex {
            base: row2,
            index,
            value,
            kind: ContainerKind::VecNonNull,
        },
    );
    p.f.set_term(body, Term::Return(vec![]));
    p.f.set_term(exit, Term::Return(vec![]));
    p.run();
    assert_eq!(p.kind(store), ContainerKind::VecNonNullInBounds);

    // A store into any container between the two reads may have replaced
    // the row.
    let mut p = Probe::new();
    let b = p.f.entry;
    let row = p.get(b, p.xs, p.ys);
    let len = p.emit(
        b,
        Inst::Field {
            base: row,
            field: u("len"),
        },
    );
    let zero = p.int(b, 0);
    p.store(b, p.i, zero);
    let li = p.load(b, p.i);
    let cond = p.emit(b, Inst::IntLt(li, len));
    let body = p.f.new_block();
    let exit = p.f.new_block();
    p.f.set_term(
        b,
        Term::Branch {
            cond,
            then_bb: body,
            else_bb: exit,
        },
    );
    p.set(body, p.xs, p.ys);
    let row2 = p.get(body, p.xs, p.ys);
    let index = p.load(body, p.i);
    let value = p.int(body, 1);
    let store = p.emit(
        body,
        Inst::StoreIndex {
            base: row2,
            index,
            value,
            kind: ContainerKind::VecNonNull,
        },
    );
    p.f.set_term(body, Term::Return(vec![]));
    p.f.set_term(exit, Term::Return(vec![]));
    p.run();
    assert_eq!(p.kind(store), ContainerKind::VecNonNull);
}

#[test]
fn a_body_with_a_lambda_is_left_alone() {
    let mut p = Probe::new();
    let (lp, store) = counted_loop(&mut p, |_, _| {});
    let lambda_entry = p.f.new_block();
    p.f.set_term(lambda_entry, Term::Return(vec![]));
    p.emit(
        lp.body,
        Inst::Lambda {
            params: vec![],
            entry: lambda_entry,
        },
    );
    p.run();
    assert_eq!(p.kind(store), ContainerKind::VecNonNull);
}

// --- Column checks and the shapes Stone indexes with.

impl Probe {
    /// The shared element check of a column access, as lowering emits it.
    fn check(&mut self, b: Idx<Block>, vec: Idx<Cell>, index: Idx<Inst>) -> Idx<Inst> {
        let base = self.load(b, vec);
        self.emit(b, Inst::VecCheckedIndex { vec: base, index })
    }

    fn is_slot(&self, id: Idx<Inst>) -> bool {
        matches!(self.f.insts[id], Inst::VecIndexSlot(_))
    }

    /// `if 0 < xs.len { <body> }`: returns the guarded block.
    fn nonempty_guard(&mut self) -> Idx<Block> {
        let entry = self.f.entry;
        let body = self.f.new_block();
        let exit = self.f.new_block();
        let zero = self.int(entry, 0);
        let len = self.len_of(entry, self.xs);
        let cond = self.emit(entry, Inst::IntLt(zero, len));
        self.f.set_term(
            entry,
            Term::Branch {
                cond,
                then_bb: body,
                else_bb: exit,
            },
        );
        self.f.set_term(exit, Term::Return(vec![]));
        body
    }
}

#[test]
fn a_column_check_in_a_counted_loop_becomes_the_slot() {
    let mut p = Probe::new();
    let (lp, _) = counted_loop(&mut p, |_, _| {});
    let i = p.load(lp.body, p.i);
    let check = p.check(lp.body, p.xs, i);
    p.run();
    assert!(p.is_slot(check), "{:?}", p.f.insts[check]);
}

#[test]
fn a_column_check_after_a_call_in_the_loop_stays() {
    let mut p = Probe::new();
    let mut check = None;
    counted_loop(&mut p, |p, b| {
        p.unknown_call(b);
        let i = p.load(b, p.i);
        check = Some(p.check(b, p.xs, i));
    });
    let check = check.unwrap();
    p.run();
    assert!(!p.is_slot(check));
}

#[test]
fn a_checked_store_proves_a_later_column_check() {
    let mut p = Probe::new();
    let (lp, _) = counted_loop(&mut p, |p, b| p.unknown_call(b));
    let i = p.load(lp.body, p.i);
    let check = p.check(lp.body, p.xs, i);
    p.run();
    assert!(p.is_slot(check));
}

#[test]
fn index_zero_is_proven_by_a_nonempty_guard() {
    let mut p = Probe::new();
    let body = p.nonempty_guard();
    let zero = p.int(body, 0);
    let check = p.check(body, p.xs, zero);
    let one = p.int(body, 1);
    let second = p.check(body, p.xs, one);
    p.f.set_term(body, Term::Return(vec![]));
    p.run();
    assert!(p.is_slot(check));
    assert!(!p.is_slot(second), "index 1 needs `len > 1`");
}

#[test]
fn len_minus_one_retains_its_check_after_only_a_nonempty_guard() {
    let mut p = Probe::new();
    let body = p.nonempty_guard();
    let len = p.len_of(body, p.xs);
    let one = p.int(body, 1);
    let last = p.emit(body, Inst::IntSub(len, one));
    let check = p.check(body, p.xs, last);
    let two = p.int(body, 2);
    let before_last = p.emit(body, Inst::IntSub(len, two));
    let second = p.check(body, p.xs, before_last);
    p.f.set_term(body, Term::Return(vec![]));
    p.run();
    assert!(!p.is_slot(check), "`len - 1` can round back to `len`");
    assert!(!p.is_slot(second), "`len - 2` needs `len > 1`");
}

#[test]
fn the_last_element_without_a_guard_stays() {
    let mut p = Probe::new();
    let b = p.f.entry;
    let len = p.len_of(b, p.xs);
    let one = p.int(b, 1);
    let last = p.emit(b, Inst::IntSub(len, one));
    let check = p.check(b, p.xs, last);
    p.f.set_term(b, Term::Return(vec![]));
    p.run();
    assert!(!p.is_slot(check));
}

#[test]
fn a_modulo_of_the_length_is_proven_by_a_nonempty_guard() {
    let mut p = Probe::new();
    let body = p.nonempty_guard();
    let i = p.load(body, p.i);
    let len = p.len_of(body, p.xs);
    let wrapped = p.emit(body, Inst::IntMod(i, len));
    let check = p.check(body, p.xs, wrapped);
    // The same shape against another vector's length proves nothing about `xs`.
    let other_len = p.len_of(body, p.ys);
    let wrapped_other = p.emit(body, Inst::IntMod(i, other_len));
    let second = p.check(body, p.xs, wrapped_other);
    p.f.set_term(body, Term::Return(vec![]));
    p.run();
    assert!(p.is_slot(check));
    assert!(!p.is_slot(second));
}

#[test]
fn a_modulo_without_a_guard_stays() {
    let mut p = Probe::new();
    let b = p.f.entry;
    let i = p.load(b, p.i);
    let len = p.len_of(b, p.xs);
    let wrapped = p.emit(b, Inst::IntMod(i, len));
    let check = p.check(b, p.xs, wrapped);
    p.f.set_term(b, Term::Return(vec![]));
    p.run();
    assert!(!p.is_slot(check), "an empty vector makes the modulo NaN");
}

#[test]
fn a_native_buffer_access_in_a_counted_loop_is_in_bounds() {
    let mut p = Probe::new();
    let (lp, _) = counted_loop(&mut p, |_, _| {});
    let base = p.load(lp.body, p.xs);
    let index = p.load(lp.body, p.i);
    let read = p.emit(
        lp.body,
        Inst::Index {
            base,
            index,
            kind: ContainerKind::NativeBuffer,
        },
    );
    let base = p.load(lp.body, p.ys);
    let index = p.load(lp.body, p.i);
    let other = p.emit(
        lp.body,
        Inst::Index {
            base,
            index,
            kind: ContainerKind::NativeBuffer,
        },
    );
    p.run();
    assert_eq!(p.kind(read), ContainerKind::NativeBufferInBounds);
    assert_eq!(p.kind(other), ContainerKind::NativeBuffer);
}

fn affine_probe(case: &str) -> ContainerKind {
    let mut p = Probe::new();
    let mut b = p.f.entry;
    let exit = p.f.new_block();
    p.f.set_term(exit, Term::Return(vec![]));
    let base = p.load(b, p.xs);
    let count = p.load(b, p.ys);
    let rows = if case == "fractional_count" {
        count
    } else {
        p.emit(b, Inst::FloatToInt(count))
    };
    let row_input = p.load(b, p.i);
    let row = if case == "fractional_row" {
        row_input
    } else {
        p.emit(b, Inst::FloatToInt(row_input))
    };
    let lane_input = p.f.new_cell(None);
    p.f.params.push(lane_input);
    let lane_input = p.load(b, lane_input);
    let lane = if case == "fractional_lane" {
        lane_input
    } else {
        p.emit(b, Inst::FloatToInt(lane_input))
    };
    let zero = p.int(b, 0);
    let four = p.int(b, 4);
    let max = p.int(b, if case == "rounding" { 1 << 53 } else { 100000 });
    let capacity = p.emit(b, Inst::IntMul(rows, four));
    let len = p.emit(
        b,
        Inst::Field {
            base,
            field: u("len"),
        },
    );
    let lane_bound = if case == "extra_lane" {
        p.int(b, 5)
    } else {
        four
    };
    for comparison in [
        Inst::IntGeq(rows, zero),
        Inst::IntLeq(rows, max),
        Inst::IntGeq(row, zero),
        Inst::IntLt(row, rows),
        Inst::IntGeq(lane, zero),
        Inst::IntLt(lane, lane_bound),
        Inst::IntGeq(len, capacity),
    ] {
        let comparison =
            if case == "nan_length_guard" && matches!(comparison, Inst::IntGeq(a, _) if a == len) {
                Inst::IntLt(len, capacity)
            } else {
                comparison
            };
        let length_reversed =
            case == "nan_length_guard" && matches!(comparison, Inst::IntLt(a, _) if a == len);
        let comparison = if case == "nan_guard" {
            match comparison {
                Inst::IntLeq(a, b) => Inst::IntGt(a, b),
                other => other,
            }
        } else {
            comparison
        };
        let reversed =
            length_reversed || (case == "nan_guard" && matches!(comparison, Inst::IntGt(..)));
        let cond = p.emit(b, comparison);
        let next = p.f.new_block();
        p.f.set_term(
            b,
            Term::Branch {
                cond,
                then_bb: if reversed { exit } else { next },
                else_bb: if reversed { next } else { exit },
            },
        );
        b = next;
    }
    if case == "callback" {
        p.unknown_call(b);
    }
    if case == "length_write" {
        p.emit(
            b,
            Inst::StoreField {
                base,
                field: u("len"),
                value: zero,
            },
        );
    }
    let offset = p.emit(b, Inst::IntMul(row, four));
    let index = p.emit(b, Inst::IntAdd(offset, lane));
    let access = p.emit(
        b,
        Inst::Index {
            base,
            index,
            kind: ContainerKind::NativeBuffer,
        },
    );
    p.f.set_term(b, Term::Return(vec![access]));
    p.run();
    p.kind(access)
}

#[test]
fn bounded_whole_affine_native_buffer_index() {
    assert_eq!(affine_probe("valid"), ContainerKind::NativeBufferInBounds);
}

#[test]
fn affine_proof_requires_whole_bounded_operands_and_live_length() {
    for case in [
        "fractional_count",
        "fractional_row",
        "fractional_lane",
        "rounding",
        "nan_guard",
        "nan_length_guard",
        "extra_lane",
        "callback",
        "length_write",
    ] {
        assert_eq!(affine_probe(case), ContainerKind::NativeBuffer, "{case}");
    }
}
