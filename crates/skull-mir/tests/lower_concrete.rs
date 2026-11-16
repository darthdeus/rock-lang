use std::sync::Arc;

use comfy_la_arena::Idx;
use skull_graph::instance_graph;
use skull_mir::instance::Instance;
use skull_mir::lower::{
    RepArena, RepGuarantees, RepId, RepUse, RepUseFacts, ScalarCtx, lower_instance_with_plans,
};
use skull_mir::mir::{Inst, MirFunc, ValueCtorForm};
use skull_mir::monoty::{MonoInterner, MonoTy};

fn lower_test_root(
    ws: &bone_core::workspace::Workspace,
    mid: bone_core::prelude::ModuleId,
    fid: comfy_la_arena::Idx<bone_core::hir::Function>,
) -> Result<skull_mir::mir::MirFunc, skull_parser::CompilerError> {
    let root = skull_mir::mir::FuncRef {
        module: mid,
        func: fid,
    };
    let interner = Arc::new(MonoInterner::new());
    let graph = instance_graph::build_in(ws, vec![root], false, Arc::clone(&interner));
    let concrete = skull_graph::concrete_body::build(ws, &graph);
    let scx = ScalarCtx::new(ws).sharing(interner);
    let plan = concrete.plans.get(&Instance::base(root)).ok_or_else(|| {
        skull_parser::CompilerError::new_no_location("no concrete plan for test root")
    })?;
    lower_instance_with_plans(ws, &scx, plan, &concrete.plans)
}

fn value_constructor(mir: &MirFunc, name: &str) -> Idx<Inst> {
    mir.insts
        .iter()
        .find_map(|(id, inst)| match inst {
            Inst::MakeValueStruct { name: found, .. }
            | Inst::MaterializeValueStruct { name: found, .. }
                if found.as_str() == name =>
            {
                Some(id)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing `{name}` value constructor"))
}

/// Lower `fn_name` from `modules` (last = the user `<test>` module) and
/// return its MIR dump.
fn lower_dump(mut modules: Vec<skull_parser::Ast>, fn_name: &str) -> String {
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let mid = *ws
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("user module id");
    let m = ws.module(mid);
    let (fid, _) = m
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == fn_name)
        .expect("function by name");
    let dump = lower_test_root(&ws, mid, fid)
        .unwrap_or_else(|e| panic!("lower failed: {}", e.message))
        .to_string();
    dump.lines()
        .map(|line| line.strip_prefix("    ").unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn scalar_abi_call_widens_an_int_argument_for_a_float_parameter() {
    let src = "value V2 { x: float, y: float }\n\
               fn mk(x: float, y: float) -> V2 { return V2 { x: x, y: y } }\n\
               fn shift(w: float) -> V2 { return mk(w, 0) }\n\
               fn shift_named(w: float) -> V2 { return mk(y: 0, x: w) }";
    for name in ["shift", "shift_named"] {
        let ast = skull_parser::parse_module_clean(src, ustr::Ustr::from("<test>")).expect("parse");
        let dump = lower_dump(vec![ast], name);
        assert!(
            dump.contains("int_to_float"),
            "an int literal bound to a float parameter must be widened in `{name}`: {dump}"
        );
    }
}

#[test]
fn value_constructors_carry_the_owned_representation() {
    let ast = skull_parser::parse_module(
        "value Inner { x: float }\n\
         value Outer { inner: Inner, y: float }\n\
         struct Holder { value: Outer }\n\
         fn make(seed: Outer) -> Holder {\n\
             Holder { value: Outer { inner: Inner { x: 1.0 }, y: 2.0 } }\n\
         }",
        ustr::Ustr::from("<test>"),
    )
    .expect("parse");
    let mut modules = vec![ast];
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let mid = *ws
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("user module id");
    let fid = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == "make")
        .map(|(fid, _)| fid)
        .expect("make function");
    let mut mir = lower_test_root(&ws, mid, fid).expect("lower");

    let outer = value_constructor(&mir, "Outer");
    let inner = value_constructor(&mir, "Inner");
    let Inst::MakeValueStruct {
        name,
        representation: outer_representation,
        form,
        fields,
    } = mir.insts[outer].clone()
    else {
        panic!("fixture must lower Outer as MakeValueStruct");
    };
    let materialized = Inst::MaterializeValueStruct {
        name,
        representation: outer_representation,
        form,
        fields,
    };
    let owner = mir
        .blocks
        .iter()
        .find_map(|(id, block)| block.insts.contains(&outer).then_some(id))
        .expect("Outer constructor block");
    let materialized = mir.emit(owner, materialized);

    let arena = mir
        .rep_arena
        .as_ref()
        .expect("lowered MIR must retain its representation arena");
    let verify = |mir: &MirFunc| skull_mir::verify::verify(mir, skull_mir::verify::Stage::LOWERED);
    assert!(verify(&mir).is_empty(), "valid constructors must verify");

    let x = match &mir.insts[inner] {
        Inst::MakeValueStruct { fields, .. } => fields[0].1,
        _ => panic!("fixture must lower Inner as MakeValueStruct"),
    };
    let y = match &mir.insts[outer] {
        Inst::MakeValueStruct { fields, .. } => fields[1].1,
        _ => unreachable!("Outer is MakeValueStruct"),
    };
    let mut leaf_form = mir.clone();
    let Inst::MakeValueStruct { form, fields, .. } = &mut leaf_form.insts[outer] else {
        unreachable!("Outer is MakeValueStruct");
    };
    *form = ValueCtorForm::Leaves;
    *fields = vec![(ustr::Ustr::from("inner.x"), x), (ustr::Ustr::from("y"), y)];
    assert!(
        verify(&leaf_form).is_empty(),
        "valid leaf paths must verify"
    );

    let foreign_arena = RepArena::new(Arc::new(MonoInterner::new()));
    let foreign_representation = foreign_arena
        .plan_nominal_value(
            &ws,
            bone_core::hir::FnBackend::Lua,
            ustr::Ustr::from("Outer"),
        )
        .expect("foreign Outer plan");
    let abi = mir.abi_plan.as_ref().expect("function ABI plan");
    let parameter_representation = abi.representation().params()[0];
    let opaque_representation = arena
        .plan(
            &ws,
            bone_core::hir::FnBackend::Lua,
            &MonoTy::Any,
            RepUse::Value,
            &RepUseFacts::default(),
            RepGuarantees::default(),
        )
        .expect("opaque value-use plan");

    #[derive(Clone, Copy)]
    enum Fault {
        MissingArena,
        Rep(RepId),
        Name(&'static str),
        Order,
        Form(ValueCtorForm),
    }
    let faults = [
        (
            outer,
            Fault::MissingArena,
            "requires a representation arena",
        ),
        (
            outer,
            Fault::Rep(foreign_representation),
            "does not belong to this function's representation arena",
        ),
        (
            outer,
            Fault::Rep(parameter_representation),
            "not a value-use plan",
        ),
        (
            outer,
            Fault::Rep(opaque_representation),
            "no value leaf layout",
        ),
        (
            materialized,
            Fault::Name("Other"),
            "representation is for `Outer`, not `Other`",
        ),
        (
            outer,
            Fault::Order,
            "fields do not match its Declared representation",
        ),
        (
            outer,
            Fault::Form(ValueCtorForm::Leaves),
            "fields do not match its Leaves representation",
        ),
        (
            inner,
            Fault::Name("Other"),
            "representation is for `Inner`, not `Other`",
        ),
    ];
    let mut missed = Vec::new();
    for (target, fault, expected) in faults {
        let mut bad = mir.clone();
        if let Fault::MissingArena = fault {
            bad.rep_arena = None;
        } else {
            let (name, representation, form, fields) = match &mut bad.insts[target] {
                Inst::MakeValueStruct {
                    name,
                    representation,
                    form,
                    fields,
                }
                | Inst::MaterializeValueStruct {
                    name,
                    representation,
                    form,
                    fields,
                } => (name, representation, form, fields),
                _ => unreachable!("selected value constructor"),
            };
            match fault {
                Fault::Rep(id) => *representation = id,
                Fault::Name(value) => *name = ustr::Ustr::from(value),
                Fault::Order => fields.swap(0, 1),
                Fault::Form(value) => *form = value,
                Fault::MissingArena => unreachable!(),
            }
        }
        if !verify(&bad).iter().any(|v| v.message.contains(expected)) {
            missed.push(expected);
        }
    }
    assert!(
        missed.is_empty(),
        "missed representation faults: {missed:?}"
    );
}

/// Stdlib-free: enough for literals, vars, `let`, and builtin operators.
fn mir(src: &str, fn_name: &str) -> String {
    let ast = skull_parser::parse_module(src, ustr::Ustr::from("<test>")).expect("parse");
    lower_dump(vec![ast], fn_name)
}

/// Stdlib-loaded: needed for `__op_*` arithmetic overloads. The stdlib is
/// parsed once (cached) and re-checked per call alongside the snippet.
fn mir_std(src: &str, fn_name: &str) -> String {
    use std::sync::LazyLock;
    static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
        rock_stdlib::stdlib_files()
            .iter()
            .map(|f| {
                let id = ustr::Ustr::from(rock_stdlib::stdlib_file_id(f.name).as_str());
                skull_parser::parse_module(f.contents, id).expect("stdlib parse")
            })
            .collect()
    });
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module(src, ustr::Ustr::from("<test>")).expect("parse"));
    lower_dump(modules, fn_name)
}

macro_rules! mir_snapshots {
    ($($name:ident($lower:ident, $src:expr, $root:expr, $expected:expr);)*) => {
        $(
            #[test]
            fn $name() {
                assert_eq!($lower($src, $root), $expected);
            }
        )*
    };
}

#[test]
fn value_struct_literal_scalarizes_at_boundary_else_cdata() {
    // Under the scalar value ABI a value RETURN flattens to leaf scalars — no
    // `V2_ct(...)` cdata is built at the boundary (the result is `return x, y`).
    let dump = mir_std(
        "value V2 { x: int, y: int }\n fn f() -> V2 { V2 { y: 7, x: 3 } }",
        "f",
    );
    assert!(
        !dump.contains("V2_ct"),
        "value return must scalarize (no cdata ctor), got:\n{dump}"
    );
    assert!(
        !dump.contains("make_struct") && !dump.contains("__ty"),
        "value struct must not be a table:\n{dump}"
    );
    // A literal escaping into a COLUMN-ELIGIBLE `[V2]` builds no element at
    // all: the leaves land straight in the per-leaf columns. Value semantics
    // still hold -- storing the scalar leaves IS the copy.
    let col = mir_std(
        "value V2 { x: int, y: int }\n fn g() -> [V2] { [V2 { y: 7, x: 3 }] }",
        "g",
    );
    assert!(
        col.contains("vec.columns") && !col.contains("V2_ct"),
        "a column-eligible element must store leaves, not a cdata ctor, got:\n{col}"
    );
    // An INELIGIBLE element keeps the boxed representation, so the escaping
    // literal still materializes `<name>_ct(...)` via `pack`. `Wide` exceeds
    // MAX_COLUMN_LEAVES; this is the other side of the eligibility boundary.
    // Display carries declared field names (for SROA): `Wide_ct(a: v.., ..)`.
    let esc = mir_std(
        "value Wide { a: int, b: int, c: int, d: int, e: int }\n \
         fn g() -> [Wide] { [Wide { a: 1, b: 2, c: 3, d: 4, e: 5 }] }",
        "g",
    );
    assert!(
        esc.contains("Wide_ct(a:"),
        "an escaping ineligible value literal must build a cdata ctor, got:\n{esc}"
    );
}

#[test]
fn dynamic_column_spread_keeps_element_repr_unboxed() {
    let dump = mir_std(
        "value V2 { x: int, y: int }\n\
         fn copy(xs: [V2]) -> [V2] { [...xs] }",
        "copy",
    );
    assert!(
        !dump.contains("V2_ct"),
        "dynamic column spread must carry leaves into the destination, got:\n{dump}"
    );
}

#[test]
fn compound_assign_to_vec_index() {
    let dump = mir_std(
        "fn f() -> int { let arr = [10, 20, 30]\n arr[1] += 5\n arr[1] }",
        "f",
    );
    assert!(
        dump.contains("store_index"),
        "expected a store_index, got:\n{dump}"
    );
    assert!(
        dump.contains("int_add") || dump.contains("add"),
        "expected an add, got:\n{dump}"
    );
}

#[test]
fn compound_assign_to_struct_field() {
    let dump = mir_std(
        "struct P { x: int }\n fn f() -> int { let p = P { x: 1 }\n p.x += 9\n p.x }",
        "f",
    );
    assert!(
        dump.contains("index.Record") && dump.contains("store_index.Record"),
        "expected a Record read and write, got:\n{dump}"
    );
    assert!(
        dump.contains("const \"x\""),
        "Record property access must use the constant field name, got:\n{dump}"
    );
}

#[test]
fn explicit_int_to_float_is_a_typed_mir_conversion() {
    let dump = mir("fn widen(value: int) -> float { value as float }", "widen");
    assert!(
        dump.contains("int_to_float"),
        "an explicit scalar cast must survive lowering as a typed conversion:\n{dump}"
    );
}

#[test]
fn omitted_result_discards_tail_value_after_its_effects() {
    let dump = mir_std(
        "value V2 { x: int, y: int }\n\
         fn produce() -> V2 { V2 { x: 1, y: 2 } }\n\
         fn sink() { produce() }",
        "sink",
    );
    assert!(
        dump.contains("call"),
        "the tail call must still execute, got:\n{dump}"
    );
    assert!(
        dump.lines().any(|line| line.trim() == "return"),
        "a function with no declared result must return no value, got:\n{dump}"
    );
    assert!(
        !dump.lines().any(|line| line.trim().starts_with("return v")),
        "the ignored tail value must not escape the unit function, got:\n{dump}"
    );
    assert!(
        !dump.contains("V2_ct"),
        "discarding the scalar return must not reconstruct its value type, got:\n{dump}"
    );
}

#[test]
fn inferred_lambda_result_remains_value_bearing() {
    let dump = mir_std("fn sink() { let xs = [2, 1]\n xs.sort(x => x) }", "sink");
    assert!(
        dump.contains("lambda"),
        "fixture must lower the sort key lambda, got:\n{dump}"
    );
    assert!(
        dump.lines().any(|line| line.trim().starts_with("return v")),
        "the context-inferred lambda result must remain value-bearing, got:\n{dump}"
    );
}

// --- Phase 2: control flow as branches + cells ---

#[test]
fn diverging_if_join_does_not_declare_never_storage() {
    let ast = skull_parser::parse_module(
        "fn f(x: int) -> int { if x > 0 { return 1 } else { return 2 } }",
        ustr::Ustr::from("<test>"),
    )
    .expect("parse");
    let mut modules = vec![ast];
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let mid = ws.module_ids[&ustr::Ustr::from("<test>")];
    let (fid, _) = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, function)| function.name.as_str() == "f")
        .expect("function");
    let mir = lower_test_root(&ws, mid, fid).expect("lower");
    assert!(
        mir.cells.iter().skip(1).all(|(_, cell)| cell.ty.is_none()),
        "a diverging join cannot materialize a never-typed local: {mir}"
    );
}

#[test]
fn nullable_aggregate_if_join_does_not_declare_payload_storage() {
    let ast = skull_parser::parse_module(
        "value Pair { x: int, y: int }\n\
         fn f(present: bool) -> Pair? {\n\
             return if present { Pair { x: 1, y: 2 } } else { null }\n\
         }",
        ustr::Ustr::from("<test>"),
    )
    .expect("parse");
    let mut modules = vec![ast];
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let mid = ws.module_ids[&ustr::Ustr::from("<test>")];
    let (fid, _) = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, function)| function.name.as_str() == "f")
        .expect("function");
    let mir = lower_test_root(&ws, mid, fid).expect("lower");
    let typed_temporaries: Vec<_> = mir
        .cells
        .iter()
        .filter(|(_, cell)| cell.name.is_none() && cell.ty.is_some())
        .collect();
    assert!(
        typed_temporaries.is_empty(),
        "nullable payload storage cannot represent its absent arm: {mir}"
    );
}

// --- Phase 2.5: aggregates + container reads ---

#[test]
fn nullable_and_any_vecs_keep_callback_capable_kind() {
    let nullable = mir("fn f(xs: [int?]) -> int? { xs[0] }", "f");
    assert!(nullable.contains("index.Vec "), "{nullable}");
    let dynamic = mir("fn f(xs: [any]) -> any { xs[0] }", "f");
    assert!(dynamic.contains("index.Vec "), "{dynamic}");
}

#[test]
fn record_and_vector_elements_fill_every_live_slot() {
    let record = mir(
        "struct P { x: int }
        fn f(xs: [P]) -> P { xs[0] }",
        "f",
    );
    assert!(record.contains("index.VecNonNull "), "{record}");
    let nested = mir("fn f(xs: [[int]]) -> [int] { xs[0] }", "f");
    assert!(nested.contains("index.VecNonNull "), "{nested}");
}

#[test]
fn nested_native_buffer_value_store_accepts_boxed_function_result() {
    let src = "value Pair { x: int, y: int }\n\
               value Outer { p: Pair, z: int }\n\
               fn make_pair(n: int) -> Pair { Pair { x: n + 5, y: n * 3 } }\n\
               fn replace_nested(b: NativeBuffer<Outer>, f: (int) -> Pair, n: int) {\n\
                   b[0].p = f(n)\n\
               }";
    let dump = mir_std(src, "replace_nested");
    assert_eq!(
        dump.matches("store_index.NativeBufferLeaf").count(),
        2,
        "Pair's declared `x` and `y` leaves must both reach storage: {dump}"
    );
}

// --- Phase 2.5 long tail: assignment targets ---

#[test]
fn match_payload_bindings_retain_their_semantic_types() {
    let ast = skull_parser::parse_module(
        "enum Mixed { Flag(bool), Number(int) }\n\
         fn f(value: Mixed) -> int { match value { Mixed::Flag(flag) => if flag { 1 } else { 0 }, Mixed::Number(number) => number } }",
        ustr::Ustr::from("<test>"),
    )
    .expect("parse");
    let mut modules = vec![ast];
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let mid = ws.module_ids[&ustr::Ustr::from("<test>")];
    let (fid, _) = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, function)| function.name.as_str() == "f")
        .expect("function");
    let mir = lower_test_root(&ws, mid, fid).expect("lower");

    for name in ["flag", "number"] {
        let cell = mir
            .cells
            .iter()
            .find(|(_, cell)| {
                cell.name
                    .is_some_and(|cell_name| cell_name.as_str() == name)
            })
            .map(|(_, cell)| cell)
            .unwrap_or_else(|| panic!("missing match binding `{name}`: {mir}"));
        assert!(
            cell.ty.is_some(),
            "match binding `{name}` lost its semantic type: {mir}"
        );
    }
}

mir_snapshots! {
        int_literal_and_return(mir, "fn five() -> int { 5 }", "five", "\
fn five() {
  bb0:
v0 = const 5
return v0
}");

        param_is_a_cell_read_via_load(mir, "fn id(x: int) -> int { x }", "id", "\
fn id(c0) {
  bb0:
v0 = load c0
return v0
}");

        let_bindings_never_written_again_bind_their_values(mir, "fn f() -> int { let a = 1; let b = 2; a }", "f", "\
fn f() {
  bb0:
v0 = const 1
v1 = const 2
return v0
}");

        int_arithmetic_dispatches_to_typed_op(mir_std, "fn add(a: int, b: int) -> int { a + b }", "add", "\
fn add(c0, c1) {
  bb0:
v0 = load c0
v1 = load c1
v2 = int.add v0, v1
return v2
}");

        float_arithmetic_dispatches_to_float_op(mir_std, "fn addf(a: float, b: float) -> float { a + b }", "addf", "\
fn addf(c0, c1) {
  bb0:
v0 = load c0
v1 = load c1
v2 = float.add v0, v1
return v2
}");

        string_plus_is_concat(mir_std, "fn cat(a: string, b: string) -> string { a + b }", "cat", "\
fn cat(c0, c1) {
  bb0:
v0 = load c0
v1 = load c1
v2 = str.concat v0, v1
return v2
}");

        int_comparison_is_typed(mir, "fn lt(a: int, b: int) -> bool { a < b }", "lt", "\
fn lt(c0, c1) {
  bb0:
v0 = load c0
v1 = load c1
v2 = int.lt v0, v1
return v2
}");

        // user module is mid 1 (builtin is 0); `g` is fn 0, `f` is fn 1, so the
        // call to `g` carries FuncRef 1:0 -- a (module, fn) ref, NOT a string.
        direct_call_resolves_to_a_funcref(mir, "fn g() -> int { 7 }\nfn f() -> int { g() }", "f", "\
fn f() {
  bb0:
v0 = call 1:0()
return v0
}");

        calling_a_fn_typed_param_is_call_value(mir, "fn ap(f: fn() -> int) -> int { f() }", "ap", "\
fn ap(c0) {
  bb0:
v0 = load c0
v1 = call_value v0()
return v1
}");

        // An `if` consumed by a local joins through a result cell (store in
        // each arm, load in the join), with no block params. Terminal destinations
        // bypass this join, so use a non-tail value consumer to retain this contract.
        if_as_value_joins_through_a_cell(mir, "fn sign(x: int) -> int { let value = if x < 0 { 0 } else { 1 }; value }", "sign", "\
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
}");

        and_short_circuits_through_a_cell(mir, "fn f(a: bool, b: bool) -> bool { a && b }", "f", "\
fn f(c0, c1) {
  bb0:
v0 = load c0
branch v0 -> bb1, bb2
  bb1:
v1 = load c1
store c2, v1
goto bb3
  bb2:
v2 = const false
store c2, v2
goto bb3
  bb3:
v3 = load c2
return v3
}");

        or_short_circuits_through_a_cell(mir, "fn f(a: bool, b: bool) -> bool { a || b }", "f", "\
fn f(c0, c1) {
  bb0:
v0 = load c0
branch v0 -> bb1, bb2
  bb1:
v1 = const true
store c2, v1
goto bb3
  bb2:
v2 = load c1
store c2, v2
goto bb3
  bb3:
v3 = load c2
return v3
}");

        null_coalesce_evaluates_lhs_once(mir, "fn f(a: int?) -> int { a ?? 0 }", "f", "\
fn f(c0) {
  bb0:
v0 = load c0
v1 = const nil
v2 = eq v0, v1
branch v2 -> bb1, bb2
  bb1:
v3 = const 0
store c1, v3
goto bb3
  bb2:
store c1, v0
goto bb3
  bb3:
v4 = load c1
return v4
}");

        while_loop_is_header_body_backedge_exit(mir, "fn f(c: bool) { while c { } }", "f", "\
fn f(c0) {
  bb0:
goto bb1
  bb1:
v0 = load c0
branch v0 -> bb2, bb3
  bb2:
goto bb1
  bb3:
return
}");

        // loop var `i` and bound are cells; the latch increments so `continue`
        // still steps. No block params.
        for_range_has_header_body_latch_exit(mir, "fn f(n: int) { for i in 0..n { } }", "f", "\
fn f(c0) {
  bb0:
v0 = const 0
store c1, v0
v1 = load c0
store c2, v1
goto bb1
  bb1:
v2 = load c1
v3 = load c2
v4 = int.lt v2, v3
branch v4 -> bb2, bb4
  bb2:
goto bb3
  bb3:
v5 = load c1
v6 = const 1
v7 = int.add v5, v6
store c1, v7
goto bb1
  bb4:
return
}");

        vec_literal_builds_a_vec(mir, "fn f() -> [int] { [1, 2, 3] }", "f", "\
fn f() {
  bb0:
v0 = const 1
v1 = const 2
v2 = const 3
v3 = vec [v0, v1, v2]
return v3
}");

        // A tuple-return destination consumes its N operands directly through
        // the multi-return ABI, without constructing an intermediate tuple.
        tuple_return_splats_multi_value(mir, "fn f() -> (int, bool) { (1, true) }", "f", "\
fn f() {
  bb0:
v0 = const 1
v1 = const true
return v0, v1
}");

        vec_index_carries_container_kind(mir, "fn f(xs: [int]) -> int { xs[0] }", "f", "\
fn f(c0) {
  bb0:
v0 = load c0
v1 = const 0
v2 = index.VecNonNull v0[v1]
return v2
}");

        tuple_index_projects(mir, "fn f(t: (int, bool)) -> int { t.0 }", "f", "\
fn f(c0) {
  bb0:
v0 = load c0
v1 = tuple_index v0.0
return v1
}");

        // A payload-free enum is just its integer tag: the unit variant lowers
        // to a plain int const (Red = 0), not an interned singleton table.
        enum_unit_variant(mir, "enum Color { Red, Green }\nfn f() -> Color { Color::Red }", "f", "\
fn f() {
  bb0:
v0 = const 0
return v0
}");

        // A scalar-payload enum scalarizes to (tag, _0, _1). The widest variant
        // Square has two fields, so Circle(5) is (0, 5, 0) -- the unused _1 slot
        // is zero-filled.
        enum_tuple_variant_carries_payload(mir, "enum Shape { Circle(int), Square(int, int) }\nfn f() -> Shape { Shape::Circle(5) }", "f", "\
fn f() {
  bb0:
v0 = const 0
v1 = const 5
v2 = const 0
return v0, v1, v2
}");

        struct_literal_in_declared_field_order(mir, "struct Point { x: int, y: int }\nfn f() -> Point { Point { x: 1, y: 2 } }", "f", "\
fn f() {
  bb0:
v0 = const 1
v1 = const 2
v2 = struct Point {x=v0, y=v1}
return v2
}");

        // `y` is omitted; its `= 0` default is lowered from the StructDefaults
        // body inline at the literal site.
        struct_literal_injects_omitted_field_default(mir, "struct P { x: int, y: int = 0 }\nfn f() -> P { P { x: 1 } }", "f", "\
fn f() {
  bb0:
v0 = const 1
v1 = const 0
v2 = struct P {x=v0, y=v1}
return v2
}");

        struct_field_access(mir, "struct Point { x: int, y: int }\nfn f(p: Point) -> int { p.x }", "f", "\
fn f(c0) {
  bb0:
v0 = load c0
v1 = const \"x\"
v2 = index.Record v0[v1]
return v2
}");

        // NativeBuffer: the builtin counted template. (`[T]` routes through
        // the stdlib VecIter protocol instead and never reaches this path.)
        for_over_native_buffer_indexes_each_element(mir, "fn f(xs: NativeBuffer<int>) { for x in xs { } }", "f", "\
fn f(c0) {
  bb0:
v0 = load c0
v1 = len.NativeBuffer v0
v2 = const 0
store c1, v2
goto bb1
  bb1:
v3 = load c1
v4 = int.lt v3, v1
branch v4 -> bb2, bb4
  bb2:
v5 = load c1
v6 = index.NativeBuffer v0[v5]
store c2, v6
goto bb3
  bb3:
v7 = load c1
v8 = const 1
v9 = int.add v7, v8
store c1, v9
goto bb1
  bb4:
return
}");

        reassignment_stores_to_the_cell(mir, "fn f() -> int { let x = 1; x = 2; x }", "f", "\
fn f() {
  bb0:
v0 = const 1
store c0, v0
v1 = const 2
store c0, v1
v2 = load c0
return v2
}");

        field_assignment_uses_record_store_index(mir, "struct P { x: int, y: int }\nfn f(p: P) -> int { p.x = 5; p.x }", "f", "\
fn f(c0) {
  bb0:
v0 = load c0
v1 = const 5
v2 = const \"x\"
store_index.Record v0[v2], v1
v3 = load c0
v4 = const \"x\"
v5 = index.Record v3[v4]
return v5
}");

        index_assignment_is_store_index(mir, "fn f(xs: [int]) -> int { xs[0] = 9; xs[0] }", "f", "\
fn f(c0) {
  bb0:
v0 = load c0
v1 = const 0
v2 = const 9
store_index.VecNonNull v0[v1], v2
v3 = load c0
v4 = const 0
v5 = index.VecNonNull v3[v4]
return v5
}");

        global_assignment_is_store_global(mir, "global G: int = 0\nfn f() -> int { G = 5; G }", "f", "\
fn f() {
  bb0:
v0 = const 5
store_global G, v0
v1 = global G
return v1
}");

        compound_assignment_reads_ops_writes_back(mir_std, "fn f(n: int) -> int { let x = 0; x += n; x }", "f", "\
fn f(c0) {
  bb0:
v0 = const 0
store c1, v0
v1 = load c1
v2 = load c0
v3 = int.add v1, v2
store c1, v3
v4 = load c1
return v4
}");

        // A scalar-payload enum is cdata: passed as (tag, _0) scalars,
        // reconstructed via `Opt_ct`, then matched by int-tag compare
        // (Some = 0, None = 1) reading the `_0` payload field.
        match_enum_tests_tag_and_binds_payload_through_cells(mir, "enum Opt { Some(int), None }\nfn f(o: Opt) -> int { match o { Opt::Some(n) => n, Opt::None => 0 } }", "f", "\
fn f(c0, c1) {
  bb0:
v0 = load c0
v1 = load c1
v2 = Opt_ct(__tag: v0, _0: v1)
v3 = field v2.__tag
v4 = const 0
v5 = eq v3, v4
branch v5 -> bb3, bb2
  bb1:
v6 = load c2
return v6
  bb2:
v7 = field v2.__tag
v8 = const 1
v9 = eq v7, v8
branch v9 -> bb5, bb4
  bb3:
v10 = field v2._0
store c3, v10
v11 = load c3
store c2, v11
goto bb1
  bb4:
unreachable
  bb5:
v12 = const 0
store c2, v12
goto bb1
}");

        // `Some(Circle(r))`: the inner Circle tag test must be GATED behind the
        // outer Some tag (parent tag before any child read) -- a cont block after
        // each tag test. This is what the single-level version couldn't do.
        //
        // The outer Opt rides the flat-enum param ABI: tag leaf `c0` (variant
        // name string) + payload slot `c1` (the Shape, cdata in the dynamic
        // slot). The gate: bb3 (inner `.__tag` int compare) and bb4 (`._0`
        // bind) are reachable only through the outer "Some" branch; the
        // wildcard arm bb2 is the fallthrough of BOTH tag tests.
        match_nested_enum_gates_child_read_behind_parent_tag(mir, "enum Shape { Circle(int), Square(int) }\nenum Opt { Some(Shape), None }\nfn f(o: Opt) -> int { match o { Opt::Some(Shape::Circle(r)) => r, _ => 0 } }", "f", "\
fn f(c0, c1) {
  bb0:
v0 = load c0
v1 = load c1
v2 = const \"Some\"
v3 = eq v0, v2
branch v3 -> bb3, bb2
  bb1:
v4 = load c2
return v4
  bb2:
v5 = const 0
store c2, v5
goto bb1
  bb3:
v6 = field v1.__tag
v7 = const 0
v8 = eq v6, v7
branch v8 -> bb4, bb2
  bb4:
v9 = field v1._0
store c3, v9
v10 = load c3
store c2, v10
goto bb1
  bb5:
unreachable
}");

        // Scalar-payload enum: cdata reconstructed from (tag, _0) scalars; the
        // if-let is an int-tag compare (Some = 0) binding the `_0` payload.
        if_let_binds_on_match_else_branch_otherwise(mir, "enum Opt { Some(int), None }\nfn f(o: Opt) -> int { if let Opt::Some(n) = o { n } else { 0 } }", "f", "\
fn f(c0, c1) {
  bb0:
v0 = load c0
v1 = load c1
v2 = Opt_ct(__tag: v0, _0: v1)
v3 = field v2.__tag
v4 = const 0
v5 = eq v3, v4
branch v5 -> bb3, bb1
  bb1:
v6 = const 0
store c2, v6
goto bb2
  bb2:
v7 = load c2
return v7
  bb3:
v8 = field v2._0
store c3, v8
v9 = load c3
store c2, v9
goto bb2
}");

        for_over_map_uses_next_iteration(mir, "fn f(m: Map<int, int>) { for (k, v) in m { } }", "f", "\
fn f(c0) {
  bb0:
v0 = load c0
v1 = const nil
store c1, v1
goto bb1
  bb1:
c1, c2 = next(v0)
v2 = load c1
v3 = const nil
v4 = eq v2, v3
branch v4 -> bb3, bb2
  bb2:
goto bb1
  bb3:
return
}");

        for_enumerate_binds_index_and_element(mir_std, "fn f(xs: [int]) { for (i, x) in enumerate(xs) { } }", "f", "\
fn f(c0) {
  bb0:
v0 = load c0
v1 = len.VecNonNull v0
v2 = const 0
store c1, v2
goto bb1
  bb1:
v3 = load c1
v4 = int.lt v3, v1
branch v4 -> bb2, bb4
  bb2:
v5 = load c1
v6 = index.VecNonNull v0[v5]
store c2, v6
goto bb3
  bb3:
v7 = load c1
v8 = const 1
v9 = int.add v7, v8
store c1, v9
goto bb1
  bb4:
return
}");

        break_jumps_to_loop_exit(mir, "fn f(c: bool) { while c { break } }", "f", "\
fn f(c0) {
  bb0:
goto bb1
  bb1:
v0 = load c0
branch v0 -> bb2, bb3
  bb2:
goto bb3
  bb3:
return
}");
}

#[test]
fn unsupported_interpolation_hole_error_is_located_at_the_hole() {
    let src = "fn f(a: any) -> string {\n    return \"x {a}\"\n}";
    let mut modules =
        vec![skull_parser::parse_module(src, ustr::Ustr::from("<test>")).expect("parse")];
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let mid = *ws
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("user module id");
    let (fid, _) = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == "f")
        .expect("function by name");
    let err = lower_test_root(&ws, mid, fid).expect_err("an `any` hole has no rendering");
    assert!(
        err.message.contains("interpolation hole"),
        "got: {:?}",
        err.message
    );
    let span = err.labels.first().expect("the hole is labeled").span;
    assert_eq!(&src[span.offset_range().0..span.offset_range().1], "a");
}

#[test]
fn nullable_nobox_presence_normalizes_escaped_boolean() {
    let src = "nobox value Id { n: int } struct State { id: Id? = null }
        fn f(state: State) -> bool { return state.id != null }";
    let ast = skull_parser::parse_module_clean(src, ustr::Ustr::from("<test>")).expect("parse");
    let dump = lower_dump(vec![ast], "f");
    assert_eq!(dump.matches(" = not ").count(), 2, "{dump}");
}

#[test]
fn nullable_nobox_presence_conditions_skip_normalization() {
    let src = "nobox value Id { n: int } struct State { id: Id? = null }
        fn f(state: State, other: State) -> int {
            let score = 0
            if state.id != null { score = 1 }
            let value = if other.id != null { 1 } else { 0 }
            if state.id != null && other.id != null { return 1 }
            if state.id != null || other.id != null { return 2 }
            while state.id != null { state.id = null }
            if value > 0 { score = 2 }
            return score
        }";
    let ast = skull_parser::parse_module_clean(src, ustr::Ustr::from("<test>")).expect("parse");
    let dump = lower_dump(vec![ast], "f");
    assert!(!dump.contains(" = not "), "{dump}");
    assert!(dump.contains("branch "), "{dump}");
}

#[test]
fn branch_conditions_do_not_materialize_logical_results() {
    for (body, result_stores) in [
        ("if a && b && c { 1 } else { 0 }", 0),
        ("if !(a || b) && c { return 1 } return 0", 0),
        ("while !(a || b) && c { break } return 0", 0),
        ("match 0 { _ if !(a || b) && c => 1, _ => 0 }", 2),
        (
            "match 0 { _ if !(a || b) && c => { return 1 }, _ => {} } return 0",
            0,
        ),
    ] {
        let src = format!("fn probe(a: bool, b: bool, c: bool) -> int {{ {body} }}");
        let ast =
            skull_parser::parse_module_clean(&src, ustr::Ustr::from("<test>")).expect("parse");
        let dump = lower_dump(vec![ast], "probe");
        assert!(
            !dump.contains("const false"),
            "condition must branch without a boolean result: {dump}"
        );
        assert!(
            !dump.contains("const true"),
            "condition must branch without a boolean result: {dump}"
        );
        assert!(
            !dump.contains(" = not "),
            "native not must swap branch destinations: {dump}"
        );
        assert_eq!(
            dump.matches("store ").count(),
            result_stores,
            "only non-terminal arm values need stores: {dump}"
        );
    }
}

#[test]
fn a_reassigned_let_keeps_its_cell() {
    let src = "fn bump(a: int) -> int { let b = a * 2\n b = b + 1\n return b }";
    let dump = mir_std(src, "bump");
    assert!(
        dump.contains("store"),
        "a reassigned let is written through its cell: {dump}"
    );
}

#[test]
fn an_unwritten_nobox_let_holds_its_leaves_as_values() {
    let src = "nobox value P { x: int, y: int }\n\
               fn mk(a: int) -> P { return P { x: a, y: a + 1 } }\n\
               fn sum(a: int) -> int { let p = mk(a)\n return p.x + p.y }";
    let dump = mir_std(src, "sum");
    assert!(
        !dump.contains("store"),
        "an unwritten nobox let needs no leaf cells: {dump}"
    );
    let src = "nobox value P { x: int, y: int }\n\
               fn mk(a: int) -> P { return P { x: a, y: a + 1 } }\n\
               fn sum(a: int) -> int { let p = mk(a)\n p.x = 5\n return p.x + p.y }";
    let dump = mir_std(src, "sum");
    assert!(
        dump.contains("store"),
        "a written nobox let keeps its leaf cells: {dump}"
    );
}
