use std::sync::Arc;

use crate::common::key;
use comfy_la_arena::Idx;
use skull_graph::instance_graph;
use skull_mir::instance::Instance;
use skull_mir::lower::{
    RepArena, RepGuarantees, RepId, RepUse, RepUseFacts, ScalarCtx, lower_instance_with_plans,
};
use skull_mir::mir::{ContainerKind, Inst, Lit, MirFunc, Term, ValueCtorForm};
use skull_mir::monoty::{MonoInterner, MonoTy};

#[test]
fn coroutine_flat_refs_keep_a_defensive_lowering_diagnostic() {
    for call in ["bump(xs[0])", "relay(xs[0])"] {
        // Admission rejects this source; exercise lowering without that gate.
        let ws = crate::common::ws_of(&format!(
            "value Cell {{ w: int }}\n\
             co fn bump(c: ref Cell) -> Coroutine<int> {{ c.w += 1; yield c.w }}\n\
             fn relay(c: ref Cell) -> Coroutine<int> {{ return bump(c) }}\n\
             fn main() {{ let xs: [Cell] = [Cell {{ w: 1 }}]; let c = {call} }}"
        ));
        let mid = ws.module_ids[&ustr::Ustr::from("<test>")];
        let (fid, _) = ws
            .module(mid)
            .functions
            .iter()
            .find(|(_, f)| f.name == key(&ws, "main"))
            .unwrap();
        for backend in [
            bone_core::hir::FnBackend::Lua,
            bone_core::hir::FnBackend::Js,
        ] {
            let err = lower_test_root_with_context(
                &ws,
                mid,
                fid,
                ScalarCtx::new(&ws).with_target(backend),
            )
            .expect_err("flat coroutine arguments must reject");
            assert!(
                err.message.contains(
                    "cannot pass a reference to a flat-stored container element to coroutine `bump`"
                ),
                "{backend:?} {call}: {err:?}"
            );
            assert!(!err.message.contains("lowering gap"), "{err:?}");
            assert_eq!(err.location.file.as_str(), "<test>");
        }
    }
}

#[test]
fn checked_compound_assignments_only_keep_safe_navigation_targets() {
    use bone_core::hir::Expr;

    let ws = crate::common::checked_ws_of(
        "struct Boxed { x: int, next: Boxed? }\n\
         fn update(b: Boxed?, x: int) {\n\
             let n = x\n\
             n += 1\n\
             let xs: [int] = [1]\n\
             xs[0] += 2\n\
             b?.x += 3\n\
             b?.next?.x += 4\n\
         }",
    );
    let mid = ws.module_ids[&ustr::Ustr::from("<test>")];
    let module = ws.module(mid);
    let (fid, _) = module
        .functions
        .iter()
        .find(|(_, f)| f.name == key(&ws, "update"))
        .unwrap();
    let body = &module.bodies[&bone_core::hir::BodyOwner::Function(fid)];
    let targets: Vec<_> = body
        .expr_arena
        .iter()
        .filter_map(|(_, expr)| {
            if let Expr::CompoundAssign { target, .. } = expr {
                Some(*target)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(targets.len(), 2);
    assert!(
        targets
            .iter()
            .all(|target| matches!(body.expr_arena[*target], Expr::SafeNav { .. }))
    );
    lower_test_root(&ws, mid, fid).expect("safe-navigation assignments lower");

    for target in ["b?.next.x", "b?.xs[0]"] {
        let ws = crate::common::ws_of(&format!(
            "struct Boxed {{ x: int, next: Boxed, xs: [int] }}\n\
             fn update(b: Boxed?) {{ {target} += 1 }}"
        ));
        assert!(
            !skull_mir::verdict::body_errors(&ws).is_empty(),
            "{target} must be rejected before MIR"
        );
    }
}

fn lower_test_root(
    ws: &bone_core::workspace::Workspace,
    mid: bone_core::prelude::ModuleId,
    fid: comfy_la_arena::Idx<bone_core::hir::Function>,
) -> Result<skull_mir::mir::MirFunc, skull_parser::CompilerError> {
    lower_test_root_with_context(ws, mid, fid, ScalarCtx::new(ws))
}

fn lower_test_root_with_context(
    ws: &bone_core::workspace::Workspace,
    mid: bone_core::prelude::ModuleId,
    fid: comfy_la_arena::Idx<bone_core::hir::Function>,
    scx: ScalarCtx,
) -> Result<skull_mir::mir::MirFunc, skull_parser::CompilerError> {
    let root = skull_mir::mir::FuncRef {
        module: mid,
        func: fid,
    };
    let interner = Arc::new(MonoInterner::new());
    let graph = instance_graph::build_in(ws, vec![root], Arc::clone(&interner));
    let concrete = skull_graph::concrete_body::build(ws, &graph);
    let scx = scx.sharing(interner);
    let plan = concrete.plans.get(&Instance::base(root)).ok_or_else(|| {
        skull_parser::CompilerError::new_no_location("no concrete plan for test root")
    })?;
    lower_instance_with_plans(ws, &scx, plan, &concrete.plans)
}

#[test]
#[cfg(debug_assertions)]
fn checked_lowering_rejects_incomplete_and_stale_type_tables() {
    use bone_core::hir::BodyOwner;
    use skull_mir::mir::FuncRef;

    let mut ws = crate::common::checked_ws_of(
        "struct Boxed { n: int = 7 }\n\
         fn main() -> int { let x = Boxed {}; return x.n }",
    );
    let mid = ws.module_ids[&ustr::Ustr::from("<test>")];
    let (fid, _) = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name == key(&ws, "main"))
        .unwrap();
    let root = FuncRef {
        module: mid,
        func: fid,
    };
    let graph = instance_graph::build(&ws, vec![root]);
    let concrete = skull_graph::concrete_body::build(&ws, &graph);
    let scx = ScalarCtx::new(&ws).sharing(Arc::clone(graph.interner()));
    let plan = concrete.plan_for(&Instance::base(root)).unwrap();
    lower_instance_with_plans(&ws, &scx, plan, &concrete.plans).unwrap();

    let mut missing = plan.clone();
    missing.types = Default::default();
    let error = lower_instance_with_plans(&ws, &scx, &missing, &concrete.plans).unwrap_err();
    assert!(error.message.contains("missing concrete type"), "{error:?}");

    missing = plan.clone();
    assert!(!missing.foreign_domains.is_empty());
    missing.foreign_domains.clear();
    let error = lower_instance_with_plans(&ws, &scx, &missing, &concrete.plans).unwrap_err();
    assert!(
        error
            .message
            .contains("missing type tables for site domain"),
        "{error:?}"
    );

    ws.modules[mid.index()]
        .bodies
        .get_mut(&BodyOwner::Function(fid))
        .unwrap()
        .generation += 1;
    let error = lower_instance_with_plans(&ws, &scx, plan, &concrete.plans).unwrap_err();
    assert!(
        error.message.contains("stale concrete type tables"),
        "{error:?}"
    );
}

#[test]
fn nested_first_class_field_updates_rebuild_every_parent() {
    let ws = crate::common::checked_ws_of(
        "value Inner { x: int, y: int }\n\
         value Outer { inner: Inner, z: int }\n\
         fn update(p: Outer) -> int { let old = p.inner.x++; return old + p.inner.x }",
    );
    let mid = ws.module_ids[&ustr::Ustr::from("<test>")];
    let (fid, _) = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name == key(&ws, "update"))
        .unwrap();
    let mir = lower_test_root_with_context(&ws, mid, fid, ScalarCtx::new_values_first_class(&ws))
        .expect("nested first-class update lowers before the shader profile gate");
    for name in ["Inner", "Outer"] {
        value_constructor(&mir, name);
    }
    assert!(
        !mir.insts
            .iter()
            .any(|(_, inst)| matches!(inst, Inst::StoreField { .. }))
    );
}

fn value_constructor(mir: &MirFunc, name: &str) -> Idx<Inst> {
    mir.insts
        .iter()
        .find_map(|(id, inst)| match inst {
            Inst::MakeValueStruct { name: found, .. }
            | Inst::MaterializeValueStruct { name: found, .. }
                if found.as_str().rsplit("::").next() == Some(name) =>
            {
                Some(id)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing `{name}` value constructor"))
}

fn lower_fixture(mut modules: Vec<skull_parser::Ast>, fn_name: &str) -> MirFunc {
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let mid = ws.module_ids[&ustr::Ustr::from("<test>")];
    let (fid, _) = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str().rsplit("::").next() == Some(fn_name))
        .expect("function by name");
    lower_test_root(&ws, mid, fid).unwrap_or_else(|e| panic!("lower failed: {}", e.message))
}

#[test]
fn scalar_abi_call_widens_an_int_argument_for_a_float_parameter() {
    let src = "value V2 { x: float, y: float }\n\
               fn mk(x: float, y: float) -> V2 { return V2 { x: x, y: y } }\n\
               fn shift(w: float) -> V2 { return mk(w, 0) }\n\
               fn shift_named(w: float) -> V2 { return mk(y: 0, x: w) }";
    for name in ["shift", "shift_named"] {
        let mir = mir(src, name);
        assert!(
            has(&mir, |inst| matches!(inst, Inst::IntToFloat(_))),
            "an int literal bound to a float parameter must be widened in `{name}`: {mir:#?}"
        );
    }
}

#[test]
fn explicit_int_to_float_is_a_typed_mir_conversion() {
    let mir = mir("fn widen(value: int) -> float { value as float }", "widen");
    assert!(
        has(&mir, |inst| matches!(inst, Inst::IntToFloat(_))),
        "an explicit scalar cast must survive lowering as a typed conversion:\n{mir:#?}"
    );
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
        .find(|(_, f)| f.name.as_str().rsplit("::").next() == Some("make"))
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
        .plan_nominal_value(&ws, bone_core::hir::FnBackend::Lua, key(&ws, "Outer"))
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
fn mir(src: &str, fn_name: &str) -> MirFunc {
    let ast = skull_parser::parse_module(src, ustr::Ustr::from("<test>")).expect("parse");
    lower_fixture(vec![ast], fn_name)
}

/// Stdlib-loaded: needed for `__op_*` arithmetic overloads. The stdlib is
/// parsed once (cached) and re-checked per call alongside the snippet.
fn mir_std(src: &str, fn_name: &str) -> MirFunc {
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
    lower_fixture(modules, fn_name)
}

fn instruction_ids(mir: &MirFunc) -> impl Iterator<Item = Idx<Inst>> + '_ {
    mir.blocks
        .values()
        .flat_map(|block| block.insts.iter().copied())
}

fn instructions(mir: &MirFunc) -> impl Iterator<Item = &Inst> {
    instruction_ids(mir).map(|id| &mir.insts[id])
}

fn values(mir: &MirFunc) -> Vec<Idx<Inst>> {
    instruction_ids(mir)
        .filter(|&id| mir.insts[id].produces_value())
        .collect()
}

fn has(mir: &MirFunc, predicate: impl FnMut(&Inst) -> bool) -> bool {
    instructions(mir).any(predicate)
}

fn count(mir: &MirFunc, mut predicate: impl FnMut(&Inst) -> bool) -> usize {
    instructions(mir).filter(|inst| predicate(inst)).count()
}

fn returns(mir: &MirFunc) -> impl Iterator<Item = &[Idx<Inst>]> {
    mir.blocks.values().filter_map(|block| match &block.term {
        Term::Return(values) => Some(values.as_slice()),
        _ => None,
    })
}

fn assert_no_v2_box(mir: &MirFunc) {
    assert!(
        !has(mir, |inst| matches!(inst,
        Inst::MakeValueStruct { name, .. } | Inst::MaterializeValueStruct { name, .. }
        if bone_core::modules::display_name(name.as_str()) == "V2")),
        "{mir:#?}"
    );
}

macro_rules! mir_properties {
    ($($name:ident($lower:ident, $src:expr, $root:expr) |$mir:ident| $check:block)*) => {
        $(
            #[test]
            fn $name() {
                let $mir = $lower($src, $root);
                $check
            }
        )*
    };
}

#[test]
fn dynamic_column_spread_keeps_element_repr_unboxed() {
    let mir = mir_std(
        "value V2 { x: int, y: int }\n\
         fn copy(xs: [V2]) -> [V2] { [...xs] }",
        "copy",
    );
    assert_no_v2_box(&mir);
}

#[test]
fn nested_native_buffer_value_store_accepts_boxed_function_result() {
    let src = "value Pair { x: int, y: int }\n\
               value Outer { p: Pair, z: int }\n\
               fn make_pair(n: int) -> Pair { Pair { x: n + 5, y: n * 3 } }\n\
               fn replace_nested(b: NativeBuffer<Outer>, f: (int) -> Pair, n: int) {\n\
                   b[0].p = f(n)\n\
               }";
    let mir = mir_std(src, "replace_nested");
    assert_eq!(
        count(&mir, |inst| matches!(
            inst,
            Inst::StoreIndex {
                kind: ContainerKind::NativeBufferLeaf,
                ..
            }
        )),
        2,
        "Pair's declared `x` and `y` leaves must both reach storage: {mir:#?}"
    );
}

#[test]
fn omitted_result_discards_tail_value_after_its_effects() {
    let mir = mir_std(
        "value V2 { x: int, y: int }\n\
         fn produce() -> V2 { V2 { x: 1, y: 2 } }\n\
         fn sink() { produce() }",
        "sink",
    );
    assert!(
        has(&mir, |inst| matches!(
            inst,
            Inst::Call { .. } | Inst::CallMulti { .. }
        )),
        "{mir:#?}"
    );
    assert_eq!(returns(&mir).collect::<Vec<_>>(), [&[]]);
    assert_no_v2_box(&mir);
}

#[test]
fn inferred_lambda_result_remains_value_bearing() {
    let mir = mir_std("fn sink() { let xs = [2, 1]\n xs.sort(x => x) }", "sink");
    let entry = instructions(&mir)
        .find_map(|inst| match inst {
            Inst::Lambda { entry, .. } => Some(*entry),
            _ => None,
        })
        .expect("sort key lambda");
    assert!(
        skull_mir::cfg::reachable_blocks_from(&mir, entry)
            .into_iter()
            .any(|bb| matches!(&mir.blocks[bb].term, Term::Return(values) if !values.is_empty()))
    );
}

#[test]
fn diverging_if_join_does_not_declare_never_storage() {
    let mir = mir(
        "fn f(x: int) -> int { if x > 0 { return 1 } else { return 2 } }",
        "f",
    );
    assert!(
        mir.cells.iter().skip(1).all(|(_, cell)| cell.ty.is_none()),
        "a diverging join cannot materialize a never-typed local: {mir:#?}"
    );
}

#[test]
fn nullable_aggregate_if_join_does_not_declare_payload_storage() {
    let mir = mir(
        "value Pair { x: int, y: int }\n\
         fn f(present: bool) -> Pair? {\n\
             return if present { Pair { x: 1, y: 2 } } else { null }\n\
         }",
        "f",
    );
    assert!(
        !mir.cells
            .values()
            .any(|cell| cell.name.is_none() && cell.ty.is_some()),
        "nullable payload storage cannot represent its absent arm: {mir:#?}"
    );
}

mir_properties! {
    string_plus_is_concat(mir_std, "fn cat(a: string, b: string) -> string { a + b }", "cat") |m| {
        let [a, b, result, ..] = values(&m)[..] else { panic!("{m:#?}") };
        assert_eq!(m.insts[result], Inst::StringConcat(a, b));
    }
    tuple_return_splats_multi_value(mir, "fn f() -> (int, bool) { (1, true) }", "f") |m| {
        let v = values(&m);
        assert_eq!(returns(&m).collect::<Vec<_>>(), [&v[..2]]);
        assert!(!has(&m, |inst| matches!(inst, Inst::MakeTuple(_))));
    }
    vec_index_carries_container_kind(mir, "fn f(xs: [int]) -> int { xs[0] }", "f") |m| {
        let v = values(&m);
        assert!(has(&m, |inst| *inst == Inst::Index {
            base: v[0], index: v[1], kind: ContainerKind::VecNonNull,
        }));
    }
    enum_unit_variant(mir, "enum Color { Red, Green }\nfn f() -> Color { Color::Red }", "f") |m| {
        assert_int_return(&m, &[0]);
    }
    enum_tuple_variant_carries_payload(mir, "enum Shape { Circle(int), Square(int, int) }\nfn f() -> Shape { Shape::Circle(5) }", "f") |m| {
        assert_int_return(&m, &[0, 5, 0]);
    }
    struct_literal_in_declared_field_order(mir, "struct Point { x: int, y: int }\nfn f() -> Point { Point { x: 1, y: 2 } }", "f") |m| {
        assert_xy_fields(&m, "Point");
    }
    struct_literal_injects_omitted_field_default(mir, "struct P { x: int, y: int = 0 }\nfn f() -> P { P { x: 1 } }", "f") |m| {
        let v = values(&m);
        assert_eq!(m.insts[v[0]], Inst::Const(Lit::Int(1)));
        assert_eq!(m.insts[v[1]], Inst::Const(Lit::Int(0)));
        assert_xy_fields(&m, "P");
    }
    for_over_native_buffer_indexes_each_element(mir_std, "fn f(xs: NativeBuffer<int>) { for x in xs { } }", "f") |m| {
        let v = values(&m);
        let mut insts = instructions(&m);
        assert!(insts.any(|inst| *inst == Inst::Len { base: v[0], kind: ContainerKind::NativeBuffer }));
        assert!(insts.any(|inst| *inst == Inst::Index {
            base: v[0], index: v[5], kind: ContainerKind::NativeBuffer,
        }));
    }
    global_assignment_is_store_global(mir, "global G: int = 0\nfn f() -> int { G = 5; G }", "f") |m| {
        let v = values(&m);
        assert!(has(&m, |inst| matches!(inst, Inst::StoreGlobal { name, value }
            if bone_core::modules::display_name(name.as_str()) == "G" && *value == v[0])));
    }
    for_over_map_uses_next_iteration(mir, "fn f(m: Map<int, int>) { for (k, v) in m { } }", "f") |m| {
        let v = values(&m);
        let cells: Vec<_> = m.cells.iter().map(|(id, _)| id).collect();
        let mut ids = instruction_ids(&m);
        assert!(ids.any(|id| m.insts[id] == Inst::MapNext {
            key_cell: cells[1], val_cell: cells[2], table: v[0],
        }));
        assert_eq!(m.insts[v[3]], Inst::Const(Lit::Nil));
        assert!(ids.any(|id| id == v[3]));
        assert!(ids.any(|id| m.insts[id] == Inst::Eq(v[2], v[3])));
    }
}

fn assert_int_return(mir: &MirFunc, want: &[i64]) {
    let v = values(mir);
    assert_eq!(returns(mir).collect::<Vec<_>>(), [&v[..want.len()]]);
    for (&id, &n) in v.iter().zip(want) {
        assert_eq!(mir.insts[id], Inst::Const(Lit::Int(n)));
    }
}

fn assert_xy_fields(mir: &MirFunc, name: &str) {
    let v = values(mir);
    assert!(has(
        mir,
        |inst| matches!(inst, Inst::MakeStruct { ty: Some(ty), fields }
        if bone_core::modules::display_name(ty.as_str()) == name
        && fields == &[(ustr::ustr("x"), v[0]), (ustr::ustr("y"), v[1])])
    ));
}

fn field(inst: &Inst, owner: Idx<Inst>, key: &str) -> bool {
    matches!(inst, Inst::Field { base, field, .. } if *base == owner && *field == key)
}

#[test]
fn match_enum_tests_tag_and_binds_payload_through_cells() {
    let mir = mir(
        "enum Opt { Some(int), None }\nfn f(o: Opt) -> int { match o { Opt::Some(n) => n, Opt::None => 0 } }",
        "f",
    );
    let v = values(&mir);
    let cells: Vec<_> = mir.cells.iter().map(|(id, _)| id).collect();
    let mut ids = instruction_ids(&mir);
    assert!(ids.any(|id| matches!(&mir.insts[id],
        Inst::MakeValueStruct { name, fields, .. } | Inst::MaterializeValueStruct { name, fields, .. }
        if bone_core::modules::display_name(name.as_str()) == "Opt"
            && fields == &[(ustr::ustr("__tag"), v[0]), (ustr::ustr("_0"), v[1])])));
    assert!(ids.any(|id| mir.insts[id] == Inst::Eq(v[3], v[4])));
    assert!(ids.any(|id| field(&mir.insts[id], v[2], "_0")));
    assert!(ids.any(|id| mir.insts[id]
        == Inst::Store {
            cell: cells[3],
            value: v[10]
        }));
    assert_eq!(mir.insts[v[11]], Inst::Load(cells[3]));
    assert!(ids.any(|id| id == v[11]));
    assert!(ids.any(|id| mir.insts[id]
        == Inst::Store {
            cell: cells[2],
            value: v[11]
        }));
}

#[test]
fn if_let_binds_on_match_else_branch_otherwise() {
    let mir = mir(
        "enum Opt { Some(int), None }\nfn f(o: Opt) -> int { if let Opt::Some(n) = o { n } else { 0 } }",
        "f",
    );
    let v = values(&mir);
    let blocks: Vec<_> = mir.blocks.iter().map(|(id, _)| id).collect();
    assert!(mir.blocks.values().any(|block| block.term
        == Term::Branch {
            cond: v[5],
            then_bb: blocks[3],
            else_bb: blocks[1],
        }));
    assert!(field(&mir.insts[v[8]], v[2], "_0"));
    let cell = mir.cells.iter().nth(2).unwrap().0;
    assert!(has(&mir, |inst| *inst == Inst::Store { cell, value: v[9] }));
}

#[test]
fn match_nested_enum_gates_child_read_behind_parent_tag() {
    let source = "enum Shape { Circle(int), Square(int) }\n\
                 enum Opt { Some(Shape), None }\n\
                 fn f(o: Opt) -> int { match o { Opt::Some(Shape::Circle(r)) => r, _ => 0 } }";
    let mir = mir(source, "f");
    let v = values(&mir);
    let blocks: Vec<_> = mir.blocks.iter().map(|(id, _)| id).collect();
    assert!(mir.blocks.values().any(|block| block.term
        == Term::Branch {
            cond: v[3],
            then_bb: blocks[3],
            else_bb: blocks[2],
        }));
    assert!(mir.blocks[blocks[3]].insts.contains(&v[6]));
    assert!(field(&mir.insts[v[6]], v[1], "__tag"));
    assert_eq!(
        mir.blocks[blocks[3]].term,
        Term::Branch {
            cond: v[8],
            then_bb: blocks[4],
            else_bb: blocks[2],
        }
    );
    assert!(mir.blocks[blocks[4]].insts.contains(&v[9]));
    assert!(field(&mir.insts[v[9]], v[1], "_0"));
    for key in ["__tag", "_0"] {
        assert_eq!(count(&mir, |inst| field(inst, v[1], key)), 1);
    }
}

#[test]
fn null_coalesce_evaluates_lhs_once() {
    let mir = mir("fn f(a: int?) -> int { a ?? 0 }", "f");
    let a = mir.params[0];
    let v = values(&mir);
    assert_eq!(count(&mir, |inst| *inst == Inst::Load(a)), 1);
    assert_eq!(mir.insts[v[0]], Inst::Load(a));
    assert_eq!(mir.insts[v[1]], Inst::Const(Lit::Nil));
    assert_eq!(mir.insts[v[2]], Inst::Eq(v[0], v[1]));
    let cell = mir.cells.iter().nth(1).unwrap().0;
    assert!(has(&mir, |inst| *inst == Inst::Store { cell, value: v[0] }));
}

#[test]
fn nullable_nobox_presence_normalizes_escaped_boolean() {
    let src = "nobox value Id { n: int } struct State { id: Id? = null }
        fn f(state: State) -> bool { return state.id != null }";
    let mir = mir(src, "f");
    assert_eq!(
        count(&mir, |inst| matches!(inst, Inst::Not(_))),
        2,
        "{mir:#?}"
    );
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
    let mir = mir(src, "f");
    assert!(!has(&mir, |inst| matches!(inst, Inst::Not(_))), "{mir:#?}");
    assert!(
        mir.blocks
            .values()
            .any(|block| matches!(block.term, Term::Branch { .. })),
        "{mir:#?}"
    );
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
        let mir = mir(&src, "probe");
        assert!(
            !has(&mir, |inst| matches!(inst, Inst::Const(Lit::Bool(false)))),
            "condition must branch without a boolean result: {mir:#?}"
        );
        assert!(
            !has(&mir, |inst| matches!(inst, Inst::Const(Lit::Bool(true)))),
            "condition must branch without a boolean result: {mir:#?}"
        );
        assert!(
            !has(&mir, |inst| matches!(inst, Inst::Not(_))),
            "native not must swap branch destinations: {mir:#?}"
        );
        assert_eq!(
            count(&mir, |inst| matches!(inst, Inst::Store { .. })),
            result_stores,
            "only non-terminal arm values need stores: {mir:#?}"
        );
    }
}

#[test]
fn a_reassigned_let_keeps_its_cell() {
    let src = "fn bump(a: int) -> int { let b = a * 2\n b = b + 1\n return b }";
    let mir = mir_std(src, "bump");
    assert!(
        has(&mir, |inst| matches!(inst, Inst::Store { .. })),
        "a reassigned let is written through its cell: {mir:#?}"
    );
}

#[test]
fn an_unwritten_nobox_let_holds_its_leaves_as_values() {
    let src = "nobox value P { x: int, y: int }\n\
               fn mk(a: int) -> P { return P { x: a, y: a + 1 } }\n\
               fn sum(a: int) -> int { let p = mk(a)\n return p.x + p.y }";
    let mir = mir_std(src, "sum");
    assert!(
        !has(&mir, |inst| matches!(inst, Inst::Store { .. })),
        "an unwritten nobox let needs no leaf cells: {mir:#?}"
    );
    let src = "nobox value P { x: int, y: int }\n\
               fn mk(a: int) -> P { return P { x: a, y: a + 1 } }\n\
               fn sum(a: int) -> int { let p = mk(a)\n p.x = 5\n return p.x + p.y }";
    let mir = mir_std(src, "sum");
    assert!(
        has(&mir, |inst| matches!(inst, Inst::Store { .. })),
        "a written nobox let keeps its leaf cells: {mir:#?}"
    );
}
