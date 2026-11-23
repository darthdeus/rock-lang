//! Owned ABI and representation choices for bodies, boundaries and storage.
//! Expected physical layouts stay explicit so internally consistent but
//! incorrect representation choices still fail.

use crate::common::{MirFixture, checked_ws_of, key, signature_plan};

use skull_mir::concrete::ConcreteSignature;
use skull_mir::instance::Instance;
use skull_mir::lower::{
    BackendFnAbiPlan, EnumLayout, EnumPlan, GlobalPlan, RepArena, RepGuarantees, RepUse,
    RepUseFacts, ScalarCtx, struct_field_plan,
};
use skull_mir::mir::FuncRef;
use skull_mir::monoty::{MonoCtor, MonoTy};
use ustr::Ustr;

fn func_ref(ws: &bone_core::workspace::Workspace, name: &str) -> FuncRef {
    let overloads = ws.functions_by_name.get(&key(ws, name)).expect("declared");
    assert_eq!(
        overloads.len(),
        1,
        "`{name}` must be unambiguous in the fixture"
    );
    let (module, func) = overloads[0];
    FuncRef { module, func }
}

fn nominal(ws: &bone_core::workspace::Workspace, name: &str) -> MonoTy {
    MonoTy::App {
        ctor: MonoCtor::Nominal(key(ws, name)),
        params: Vec::new(),
    }
}

#[test]
fn concrete_plan_keeps_a_generic_bound_tuple_parameter_opaque() {
    let ws = checked_ws_of(
        r#"
nobox value V2 { x: float, y: float }
@scalar_abi fn generic_tuple_take<T>(value: T) -> T { return value }
@scalar_abi fn written_tuple(value: (V2, float)) -> (V2, float) { return value }
fn main() { print("{generic_tuple_take((V2 { x: 1.0, y: 2.0 }, 3.0)).0.x}") }
"#,
    );
    let scx = ScalarCtx::new(&ws);
    let take = func_ref(&ws, "generic_tuple_take");
    let v2_ty = nominal(&ws, "V2");
    let tuple = scx
        .interner()
        .intern(MonoTy::Tuple(vec![v2_ty, MonoTy::Float]));
    let take_instance = Instance::intern(
        scx.interner(),
        take,
        vec![MonoTy::Tuple(vec![MonoTy::App {
            ctor: MonoCtor::Nominal(key(&ws, "V2")),
            params: Vec::new(),
        }])],
    );
    let plan = signature_plan(
        take_instance,
        ConcreteSignature {
            params: vec![tuple],
            param_tuple_spelled: vec![false],
            result: Some(tuple),
        },
    );
    let abi = BackendFnAbiPlan::for_concrete_plan(&ws, &scx, &plan);
    assert_eq!(
        abi.param_leaf_widths(),
        None,
        "a tuple supplied through generic T must keep the generic boundary boxed"
    );
    assert_eq!(
        abi.result_lanes(),
        3,
        "a generic tuple result keeps its multi-value return shape"
    );

    let written = func_ref(&ws, "written_tuple");
    let written_plan = signature_plan(
        Instance::base(written),
        ConcreteSignature {
            params: vec![tuple],
            param_tuple_spelled: vec![true],
            result: Some(tuple),
        },
    );
    let written_abi = BackendFnAbiPlan::for_concrete_plan(&ws, &scx, &written_plan);
    assert_eq!(written_abi.param_leaf_widths(), Some(vec![3]));
}

#[test]
fn missing_bound_result_plan_is_an_explicit_error() {
    let ws = checked_ws_of("fn main() { print(\"ok\") }\n");
    let scx = ScalarCtx::new(&ws);
    let signature = ConcreteSignature::default();
    let owner = Instance::base(func_ref(&ws, "main"));
    let abi = BackendFnAbiPlan::for_concrete_signature(&ws, &scx, &owner, &signature);
    let result = abi.bound_next_layout(signature.result);
    assert!(
        result.is_err(),
        "a missing concrete bound result must not become a no-op"
    );
}

#[test]
fn enum_layout_is_selected_by_one_owned_plan() {
    let cdata_ws = checked_ws_of("enum E { A(int), B(int), None }");
    let cdata = EnumPlan::for_name(&cdata_ws, key(&cdata_ws, "E")).expect("enum plan");
    assert!(matches!(cdata.layout(), EnumLayout::Cdata(_)));
    assert!(cdata.cdata().is_some());
    assert!(cdata.flat().is_none());

    let flat_ws = checked_ws_of("nobox value P { x: int, y: int }\nenum E { Has(P), Nope }");
    let flat = EnumPlan::for_name(&flat_ws, key(&flat_ws, "E")).expect("enum plan");
    assert!(matches!(flat.layout(), EnumLayout::Flat(_)));
    assert!(flat.flat().is_some());
    assert!(flat.cdata().is_none());

    let unit_ws = checked_ws_of("enum E { A, B }");
    let unit = EnumPlan::for_name(&unit_ws, key(&unit_ws, "E")).expect("enum plan");
    assert!(unit.is_payload_free());
    assert!(matches!(unit.layout(), EnumLayout::PayloadFree));
}

#[test]
fn concrete_rep_planner_is_total_over_the_logical_type_vocabulary() {
    let ws = checked_ws_of("nobox value V2 { x: float, y: float }\nvalue Gc { text: string }");
    let arena = RepArena::default();
    let valid = vec![
        MonoTy::Int,
        MonoTy::Uint,
        MonoTy::Float,
        MonoTy::Bool,
        MonoTy::String,
        MonoTy::Any,
        MonoTy::Unit,
        MonoTy::Never,
        MonoTy::Nullable(Box::new(MonoTy::Int)),
        MonoTy::Tuple(vec![MonoTy::Int, MonoTy::Float]),
        MonoTy::App {
            ctor: MonoCtor::Array,
            params: vec![MonoTy::Int],
        },
        MonoTy::Function {
            params: vec![MonoTy::Int],
            ret: Box::new(MonoTy::Bool),
        },
        nominal(&ws, "V2"),
    ];
    assert_eq!(
        valid.len(),
        13,
        "the valid-type denominator must stay nonzero"
    );
    let facts = RepUseFacts::default();
    let guarantees = RepGuarantees::default();
    for ty in &valid {
        arena
            .plan(
                &ws,
                bone_core::hir::FnBackend::Lua,
                ty,
                RepUse::Parameter,
                &facts,
                guarantees,
            )
            .unwrap_or_else(|e| panic!("valid logical type `{}` was rejected: {e}", ty.render()));
    }
    let rejected = arena
        .plan(
            &ws,
            bone_core::hir::FnBackend::Lua,
            &MonoTy::App {
                ctor: MonoCtor::Array,
                params: vec![MonoTy::Int],
            },
            RepUse::Parameter,
            &facts,
            RepGuarantees {
                nobox: true,
                allow_opaque: false,
            },
        )
        .expect_err("a nobox container must not silently become an opaque slot");
    assert!(rejected.message.contains("nobox"));
    let rejected_nominal = arena
        .plan(
            &ws,
            bone_core::hir::FnBackend::Lua,
            &nominal(&ws, "Gc"),
            RepUse::Parameter,
            &facts,
            RepGuarantees {
                nobox: true,
                allow_opaque: false,
            },
        )
        .expect_err("a GC-carrying nominal must not silently become an opaque nobox slot");
    assert!(rejected_nominal.message.contains("nobox"));
}

#[test]
fn global_storage_uses_an_owned_value_plan() {
    let ws = checked_ws_of(
        "nobox value G { x: float, y: float }\n\
         global state: G = G { x: 1.0, y: 2.0 }\n\
         fn main() { print(\"ok\") }\n",
    );
    let mid = *ws
        .module_ids
        .get(&Ustr::from("<test>"))
        .expect("test module");
    let gid = ws.module(mid).global_by_name[&key(&ws, "state")];
    let scx = ScalarCtx::new(&ws);
    let plan = GlobalPlan::for_declared_type(&ws, &scx, ws.module(mid).globals[gid].ty)
        .expect("a declared global must have a physical storage plan");
    assert_eq!(plan.use_context(), RepUse::Value);
    assert_eq!(
        plan.lanes(),
        2,
        "the global plan must own both scalar leaves"
    );
}

#[test]
fn nominal_value_presence_flags_are_owned_by_the_rep_plan() {
    let ws = checked_ws_of(
        "nobox value V2 { x: float, y: float }\nvalue Boxed { child: V2?, tag: int }\n",
    );
    let arena = RepArena::default();
    assert_eq!(
        arena
            .nominal_value_leaf_presence_flags_required(
                &ws,
                bone_core::hir::FnBackend::Lua,
                key(&ws, "Boxed"),
            )
            .expect("the required presence plan must exist"),
        vec![true, false, false, false]
    );
}

#[test]
fn shared_struct_field_plan_is_required_and_fail_closed() {
    let mut ws = checked_ws_of(
        "struct Child { x: int }\n\
         value Inner { x: int }\n\
         value Outer { inner: Inner }\n\
         nobox value N { x: int }\n\
         struct Holder { embed child: Child?, n: N, direct: int, coroutine: Coroutine<int, int, int> }",
    );
    let backend = bone_core::hir::FnBackend::Lua;
    let holder = key(&ws, "Holder");
    let outer = key(&ws, "Outer");
    for (parent, field, inline, direct, nullable) in [
        (holder, "child", Some(key(&ws, "Child")), None, true),
        (holder, "n", Some(key(&ws, "N")), None, false),
        (holder, "direct", None, None, false),
        (outer, "inner", None, Some(key(&ws, "Inner")), false),
        (
            holder,
            "coroutine",
            None,
            Some(Ustr::from("Coroutine")),
            false,
        ),
    ] {
        let plan = struct_field_plan(&ws, backend, parent, Ustr::from(field), false)
            .unwrap_or_else(|err| panic!("{field}: {err}"));
        assert_eq!(plan.inline_child(), inline, "{field}");
        assert_eq!(plan.direct_child(), direct, "{field}");
        assert_eq!(plan.is_direct(), inline.is_none(), "{field}");
        assert_eq!(plan.nullable(), nullable, "{field}");
    }

    for (parent, field, expected) in [
        (Ustr::from("Missing"), "child", "struct `Missing`"),
        (holder, "missing", "field `missing`"),
    ] {
        let error = struct_field_plan(&ws, backend, parent, Ustr::from(field), false)
            .expect_err("unknown field or parent must reject");
        assert!(error.message.contains(expected), "{error}");
    }

    let sid = ws.nominals.struct_id(holder).expect("holder declaration");
    ws.modules[sid.module.index()].structs[sid.def].fields[0].ty = bone_core::prelude::TyId::Local(
        comfy_la_arena::Idx::from_raw(comfy_la_arena::RawIdx::from(0)),
    );
    let unresolved = struct_field_plan(&ws, backend, holder, Ustr::from("child"), false)
        .expect_err("an unresolved field type must reject");
    assert!(unresolved.message.contains("resolved type"), "{unresolved}");
}

#[test]
fn aggregate_parameter_transport_preserves_demand_and_boundary_rules() {
    use skull_mir::mir::{Inst, Lit};

    let ws = checked_ws_of(
        r#"
value Sample { x: float, y: float }
value Wide { x: float, y: float, z: float }
nobox value Inner { x: float, y: float }
nobox value Outer { unused: float, kept: Inner, tail: float }

fn leaf(v: Wide) -> float { return v.y }
fn leaf_caller() -> float { return leaf(Wide { x: 1.0, y: 2.0, z: 3.0 }) }
fn nested(v: Outer) -> float { return v.kept.y }
fn nested_caller(v: Outer) -> float { return nested(v) }
fn identity(v: Sample) -> Sample { return v }
fn mutate(v: Sample) -> float { v = Sample { x: v.y, y: v.x }; return v.x }
fn captured(v: Sample) -> fn() -> float { return () => v.x }
fn whole_caller(v: Sample) -> float { return mutate(identity(v)) }
fn unused(v: Sample) -> float { return 5.0 }
fn unused_caller() -> float { return unused(Sample { x: 1.0, y: 2.0 }) }
@nomangle fn demand_fixed(v: Sample) -> float { return v.y }
fn demand_default(v: Sample = Sample { x: 1.0, y: 2.0 }) -> float { return v.y }
fn demand_nullable(v: Sample?) -> Sample? { return v }
fn demand_method(v: Sample) -> float { return v.y }
fn safe_caller(v: Sample?) -> float { return v?.demand_method() ?? 0.0 }
"#,
    );
    let roots = [
        "leaf_caller",
        "nested_caller",
        "whole_caller",
        "captured",
        "unused_caller",
        "demand_fixed",
        "demand_default",
        "demand_nullable",
        "safe_caller",
    ]
    .map(|name| func_ref(&ws, name));
    let fixture = MirFixture::new(&ws, &roots);
    for (name, width) in [
        ("leaf", 1),
        ("nested", 2),
        ("identity", 2),
        ("mutate", 2),
        ("captured", 2),
        ("unused", 0),
        ("demand_fixed", 1),
        ("demand_default", 2),
        ("demand_nullable", 3),
        ("demand_method", 1),
    ] {
        assert_eq!(fixture.named(name).params.len(), width, "{name}");
    }
    let nested = fixture.named("nested");
    let abi = nested.abi_plan.as_ref().expect("nested parameter ABI");
    let rep = abi.rep_plan(abi.representation().params()[0]);
    assert!(rep.nobox());
    let paths = rep.leaf_paths().expect("nobox transport has leaves");
    assert_eq!(paths, ["kept.x", "kept.y"].map(Ustr::from));

    for (caller, callee, width) in [
        ("leaf_caller", "leaf", 1),
        ("unused_caller", "unused", 0),
        ("safe_caller", "demand_method", 1),
    ] {
        let mir = fixture.named(caller);
        let callee_ref = func_ref(&ws, callee);
        let args = mir
            .insts
            .values()
            .find_map(|inst| match inst {
                Inst::Call { func, args } if *func == callee_ref => Some(args),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{caller} must call {callee}"));
        assert_eq!(args.len(), width, "{caller}");
        if callee == "leaf" {
            assert!(matches!(mir.insts[args[0]], Inst::Const(Lit::Float(v)) if v == 2.0));
        }
    }
}

#[test]
fn nullable_child_expansion_respects_the_signature_lane_budget() {
    let budget = skull_mir::lower::SIGNATURE_SCALAR_LANE_BUDGET;
    let count = budget / 5 + 1;
    let fields = (0..count)
        .map(|i| format!("child{i}: Token?"))
        .collect::<Vec<_>>()
        .join(", ");
    let ws = checked_ws_of(&format!(
        r#"
value Token {{ a: int, b: int, c: int, d: int }}
nobox value Many {{ {fields} }}
fn consume(value: Many) -> int {{ let whole = value; return whole.child0?.a ?? -1 }}
"#
    ));
    let root = func_ref(&ws, "consume");
    let fixture = MirFixture::new(&ws, &[root]);
    let mir = fixture.lower(root).expect("lower test root");
    assert_eq!(
        mir.params.len(),
        count,
        "expansion must retain opaque children when scalar lanes exceed the budget"
    );
    assert!(mir.params.len() < budget);
}
