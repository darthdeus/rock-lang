//! `elide_bounds_checks_for_target` marks a `[T]` element access `VecInBounds` only when
//! its index is provably within `[0, len)` on every path, and keeps the check
//! wherever the body could have shrunk or rebound the vector in between.
use bone_core::hir::FnBackend;
use comfy_la_arena::Idx;
use skull_mir::mir::{Block, Cell, ContainerKind, Inst, Lit, MirFunc, Term};
use skull_transform::elide_bounds_checks_for_target;
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
                nonthrowing: false,
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

    /// The JS target, which adds no allocation guarantee of its own.
    fn run(&mut self) {
        elide_bounds_checks_for_target(None, &mut self.f, FnBackend::Js);
    }
}

/// `for i in 0..xs.len() { <body(B)>; xs[i] = 1; <after(B)> }` with the
/// `lower_for_range` shape: the bound snapshot in a cell, the counter marked
/// as a loop binding, and a pure-increment latch.
struct CountedLoop {
    body: Idx<Block>,
    latch: Idx<Block>,
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
    (CountedLoop { body, latch }, store)
}

#[test]
fn counted_loop_body_barriers_keep_the_check() {
    let keeps_check = |name: &str, before: fn(&mut Probe, Idx<Block>)| {
        let mut p = Probe::new();
        let (_, store) = counted_loop(&mut p, before);
        p.run();
        assert_eq!(p.kind(store), ContainerKind::VecNonNull, "{name}");
    };
    keeps_check("unknown call", |p, b| p.unknown_call(b));
    keeps_check("external push", |p, b| {
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
    keeps_check("length store", |p, b| {
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
    keeps_check("vector rebind", |p, b| {
        let other = p.load(b, p.ys);
        p.store(b, p.xs, other);
    });
    keeps_check("possibly negative counter", |p, _| {
        let entry = p.f.entry;
        let len = p.len_of(entry, p.ys);
        let one = p.int(entry, 1);
        let init = p.emit(entry, Inst::IntSub(len, one));
        p.store(entry, p.i, init);
    });
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
            nonthrowing: false,
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
fn affine_proof_requires_whole_bounded_operands_and_live_length() {
    assert_eq!(affine_probe("valid"), ContainerKind::NativeBufferInBounds);
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

#[test]
fn only_a_private_lua_literal_push_preserves_unrelated_bounds() {
    let literal = |p: &mut Probe| p.emit(p.f.entry, Inst::MakeVec(vec![]));
    let cases: [(&str, fn(&mut Probe) -> Idx<Inst>, FnBackend, ContainerKind); 4] = [
        (
            "private literal",
            literal,
            FnBackend::Lua,
            ContainerKind::VecNonNullInBounds,
        ),
        (
            "exposed backing",
            |p| {
                let arr = p.emit(p.f.entry, Inst::MakeVec(vec![]));
                p.emit(
                    p.f.entry,
                    Inst::Field {
                        nonthrowing: false,
                        base: arr,
                        field: u("_data"),
                    },
                );
                arr
            },
            FnBackend::Lua,
            ContainerKind::VecNonNull,
        ),
        (
            // The push goes through the direct alias, which must be revoked too.
            "borrowed cell",
            |p| {
                let cell = p.f.new_cell(None);
                p.f.cells[cell].borrowed = true;
                let arr = p.emit(p.f.entry, Inst::MakeVec(vec![]));
                p.store(p.f.entry, cell, arr);
                arr
            },
            FnBackend::Lua,
            ContainerKind::VecNonNull,
        ),
        (
            "fresh JS array with possibly inherited setters",
            literal,
            FnBackend::Js,
            ContainerKind::VecNonNull,
        ),
    ];
    for (name, make_arr, backend, expected) in cases {
        let mut p = Probe::new();
        let arr = make_arr(&mut p);
        let (_, store) = counted_loop(&mut p, |p, b| {
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
        elide_bounds_checks_for_target(None, &mut p.f, backend);
        assert_eq!(p.kind(store), expected, "{name}");
    }
}

#[test]
fn only_private_lua_record_reads_preserve_bounds() {
    for mode in ["lua", "alias", "borrowed", "js"] {
        let mut p = Probe::new();
        let entry = p.f.entry;
        let seven = p.int(entry, 7);
        let record = p.emit(
            entry,
            Inst::MakeStruct {
                ty: Some(u("Report")),
                fields: vec![(u("offset"), seven)],
            },
        );
        if mode == "borrowed" {
            p.f.cells[p.i].borrowed = true;
        }
        p.store(entry, p.i, record);
        let body = p.nonempty_guard();
        let base = if mode == "alias" {
            p.load(body, p.i)
        } else {
            record
        };
        let field = p.emit(body, Inst::Const(Lit::Str(u("offset"))));
        p.emit(
            body,
            Inst::Index {
                base,
                index: field,
                kind: ContainerKind::Record,
            },
        );
        let zero = p.int(body, 0);
        let check = p.check(body, p.xs, zero);
        p.f.set_term(body, Term::Return(vec![]));
        let backend = if mode == "js" {
            FnBackend::Js
        } else {
            FnBackend::Lua
        };
        elide_bounds_checks_for_target(None, &mut p.f, backend);
        assert_eq!(p.is_slot(check), matches!(mode, "lua" | "alias"), "{mode}");
    }
}
