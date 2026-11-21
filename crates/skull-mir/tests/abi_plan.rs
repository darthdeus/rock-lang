//! Owned ABI and representation choices for bodies, boundaries and storage.
//! Expected physical layouts stay explicit so internally consistent but
//! incorrect representation choices still fail.

use crate::common::{MirFixture, checked_ws_of, key, signature_plan};

use skull_mir::concrete::ConcreteSignature;
use skull_mir::instance::Instance;
use skull_mir::lower::{
    BackendFnAbiPlan, BoundaryClass, ContainerFamily, ContainerPlan, ContainerStorage, EnumLayout,
    EnumPlan, GlobalPlan, RepArena, RepGuarantees, RepLayout, RepUse, RepUseFacts, ScalarCtx,
    ValueStorage, bound_next_shape_from_concrete, boundary_requires_boxed_representation,
    declared_boundary_class, struct_field_plan,
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

/// A generic whose value instantiation splats, plus a non-generic control that
/// carries the same ABI unconditionally.
const SRC: &str = "\
value V2 { x: float, y: float }

fn ident<T>(v: T) -> T {
    return v
}

fn shift(v: V2) -> V2 {
    return V2 { x: v.x + 1.0, y: v.y }
}

fn main() {
    let a = ident(V2 { x: 1.0, y: 2.0 })
    let b = shift(a)
    print(\"{b.x}\")
}
";

/// Concrete generic and declaration-shaped bodies each own the ABI used by
/// their callers.
#[test]
fn concrete_and_base_bodies_state_their_owned_abi() {
    let ws = checked_ws_of(SRC);
    let ident = func_ref(&ws, "ident");
    let main = func_ref(&ws, "main");
    let fixture = MirFixture::new(&ws, &[main]);
    let (_, concrete) = fixture
        .bodies
        .plans
        .iter()
        .find(|(instance, _)| instance.func == ident && !instance.args.is_empty())
        .expect("the graph must retain ident<V2>");
    let spec = BackendFnAbiPlan::for_concrete_plan(&ws, &fixture.scx, concrete);
    assert_eq!(
        spec.param_leaf_widths(),
        Some(vec![2]),
        "`ident<V2>` takes V2's two float leaves"
    );
    assert_eq!(spec.result_lanes(), 2, "and returns them on two lanes");
    let caller = fixture.lower(main).expect("lower test root");
    let concrete_instance = concrete
        .function_instance()
        .expect("function concrete owner");
    let call = caller
        .call_abi_plans
        .values()
        .find(|plan| plan.subject == *concrete_instance)
        .expect("the direct call must retain ident<V2>'s ABI plan");
    assert_eq!(
        call.as_ref(),
        &spec,
        "the generic call must consume ident<V2>'s owned plan"
    );
    let shift = func_ref(&ws, "shift");
    let concrete = fixture
        .bodies
        .plans
        .get(&Instance::base(shift))
        .expect("the graph must retain shift's concrete body");
    let plan = BackendFnAbiPlan::for_concrete_plan(&ws, &fixture.scx, concrete);
    assert_eq!(plan.param_leaf_widths(), Some(vec![2]));
    assert_eq!(plan.result_lanes(), 2);
}

#[test]
fn concrete_plan_applies_fixed_name_guards_before_fn_value_narrowing() {
    let ws = checked_ws_of(
        r#"
nobox value V2 { x: float, y: float }
@nomangle fn sink(v: V2) -> V2 { return v }
fn apply(f: (V2) -> V2, v: V2) -> V2 { return f(v) }
fn main() {
    let v = V2 { x: 1.0, y: 2.0 }
    print("{apply(sink, v).x}")
}
"#,
    );
    let scx = ScalarCtx::new(&ws);
    let sink = func_ref(&ws, "sink");
    let v2 = scx.interner().intern(nominal(&ws, "V2"));
    let plan = signature_plan(
        Instance::base(sink),
        ConcreteSignature {
            params: vec![v2],
            param_tuple_spelled: vec![false],
            result: Some(v2),
        },
    );
    let abi = BackendFnAbiPlan::for_concrete_plan(&ws, &scx, &plan);
    assert_eq!(
        abi.param_leaf_widths(),
        None,
        "a fixed-name body must retain its boxed ABI even when taken by reference"
    );
    assert_eq!(abi.result_lanes(), 1);
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

/// Transport choices for scalar, nested, tuple, nullable and boxed signatures.
#[test]
fn owned_signature_shapes_have_the_expected_transport() {
    let ws = checked_ws_of(
        r#"
nobox value Leaf { x: float, y: float }
nobox value Nested { leaf: Leaf, z: float }
struct Boxed { text: string, tag: int }

@scalar_abi fn scalar(v: int) -> int { return v }
@scalar_abi fn nested(v: Nested) -> Nested { return v }
@scalar_abi fn tuple(v: (Leaf, float)) -> (Leaf, float) { return v }
@scalar_abi fn nullable(v: Leaf?) -> Leaf? { return v }
fn boxed(v: Boxed) -> Boxed { return v }

fn main() {
    let leaf = Leaf { x: 1.0, y: 2.0 }
    let nested_value = Nested { leaf: leaf, z: 3.0 }
    let _ = scalar(1)
    let _ = nested(nested_value)
    let _ = tuple((leaf, 4.0))
    let _ = nullable(leaf)
    let _ = boxed(Boxed { text: "boxed", tag: 5 })
}

"#,
    );
    let main = func_ref(&ws, "main");
    let fixture = MirFixture::new(&ws, &[main]);
    let caller = fixture.lower(main).expect("lower test root");
    for (name, parameter_width, result_lanes) in [
        ("scalar", None, 1),
        ("nested", Some(3), 3),
        ("tuple", Some(3), 3),
        ("nullable", Some(3), 3),
        ("boxed", None, 1),
    ] {
        let subject = Instance::base(func_ref(&ws, name));
        let concrete = fixture.bodies.plans.get(&subject).expect("concrete body");
        let owned = BackendFnAbiPlan::for_concrete_plan(&ws, &fixture.scx, concrete);
        let entry = fixture.lower(subject.func).expect("lower test root");
        let call = caller
            .call_abi_plans
            .values()
            .find(|plan| plan.subject == subject)
            .unwrap_or_else(|| panic!("caller must retain {name}'s concrete ABI plan"));
        assert_eq!(
            owned.param_leaf_widths(),
            parameter_width.map(|width| vec![width]),
            "{name}: parameter transport"
        );
        assert_eq!(
            owned.result_lanes(),
            result_lanes,
            "{name}: result transport"
        );
        assert_eq!(entry.params.len(), parameter_width.unwrap_or(1), "{name}");
        assert_eq!(entry.abi_plan.as_deref(), Some(&owned), "{name}");
        assert_eq!(call.as_ref(), &owned, "{name}");
        let transport = caller.insts.iter().find_map(|(_, inst)| match inst {
            skull_mir::mir::Inst::Call { func, args } if *func == subject.func => {
                Some((args.len(), 1))
            }
            skull_mir::mir::Inst::CallMulti { func, args, dests } if *func == subject.func => {
                Some((args.len(), dests.len()))
            }
            _ => None,
        });
        assert_eq!(
            transport,
            Some((parameter_width.unwrap_or(1), result_lanes)),
            "{name}"
        );
    }
}

#[test]
fn function_value_uses_the_owned_signature_representation() {
    let ws = checked_ws_of(
        r#"
nobox value Leaf { x: float, y: float }
@scalar_abi fn nested(v: Leaf) -> Leaf { return v }
fn apply(f: (Leaf) -> Leaf, v: Leaf) -> Leaf { return f(v) }
fn main() {
    let v = Leaf { x: 1.0, y: 2.0 }
    print("{apply(nested, v).x}")
}
"#,
    );
    let apply = func_ref(&ws, "apply");
    let nested_func = func_ref(&ws, "nested");
    let fixture = MirFixture::new(&ws, &[apply, nested_func]);
    let nested_instance = Instance::base(nested_func);
    let nested_plan = fixture
        .bodies
        .plans
        .get(&nested_instance)
        .expect("nested concrete plan");
    let nested = BackendFnAbiPlan::for_concrete_plan(&ws, &fixture.scx, nested_plan);
    let leaf = nominal(&ws, "Leaf");
    let fn_value = BackendFnAbiPlan::for_function_value(
        &ws,
        &fixture.scx,
        &MonoTy::Function {
            params: vec![leaf.clone()],
            ret: Box::new(leaf),
        },
        true,
    )
    .expect("function type must have a total physical plan");
    assert_eq!(
        fn_value.params(),
        nested.representation().params(),
        "an indirect caller must use the callee's parameter RepIds"
    );
    assert_eq!(
        fn_value.result(),
        nested.representation().result(),
        "an indirect caller must use the callee's result RepId"
    );
    let lowered = fixture.lower(apply).expect("lower test root");
    let call = lowered.insts.iter().find_map(|(_, inst)| match inst {
        skull_mir::mir::Inst::CallValueMulti { args, .. } => Some(args.len()),
        _ => None,
    });
    assert_eq!(
        call,
        Some(2),
        "the indirect callee must splat Leaf through its owned plan"
    );
}

#[test]
fn unit_function_values_share_the_no_result_abi() {
    let ws = checked_ws_of("fn sink() { print(\"ok\") }\n");
    let scx = ScalarCtx::new(&ws);
    let sink = func_ref(&ws, "sink");
    let declaration = BackendFnAbiPlan::for_boundary_declaration(&ws, &scx, sink);
    let value = BackendFnAbiPlan::for_function_value(
        &ws,
        &scx,
        &MonoTy::Function {
            params: Vec::new(),
            ret: Box::new(MonoTy::Unit),
        },
        true,
    )
    .expect("a unit function value must have a total physical plan");

    assert_eq!(declaration.representation().result(), None);
    assert_eq!(value.result(), None);
}

#[test]
fn nested_opaque_tuple_elements_keep_one_physical_slot() {
    let ws = checked_ws_of("fn main() { print(\"ok\") }\n");
    let arena = RepArena::default();
    let nested = MonoTy::Tuple(vec![MonoTy::Int, MonoTy::Float]);
    let outer = MonoTy::Tuple(vec![nested, MonoTy::Bool]);
    let id = arena
        .plan(
            &ws,
            bone_core::hir::FnBackend::Lua,
            &outer,
            RepUse::Return,
            &RepUseFacts {
                tuple_spelled: true,
                span: None,
            },
            RepGuarantees::default(),
        )
        .expect("nested tuple representation must be total");
    let outer_plan = arena.get(id);
    let RepLayout::Tuple(tuple) = outer_plan.layout() else {
        panic!("outer tuple must retain its top-level return lanes")
    };
    let slots = &tuple.slots;
    assert_eq!(slots.len(), 2, "the outer tuple population must be nonzero");
    assert!(matches!(arena.get(slots[0]).layout(), RepLayout::Opaque));
}

#[test]
fn a_representation_id_cannot_cross_build_arenas() {
    let ws = checked_ws_of("fn main() { print(\"ok\") }\n");
    let left = RepArena::default();
    let right = RepArena::default();
    let left_id = left
        .plan(
            &ws,
            bone_core::hir::FnBackend::Lua,
            &MonoTy::Float,
            RepUse::Value,
            &RepUseFacts::default(),
            RepGuarantees::default(),
        )
        .expect("the left arena must plan the primitive");
    right
        .plan(
            &ws,
            bone_core::hir::FnBackend::Lua,
            &MonoTy::Float,
            RepUse::Value,
            &RepUseFacts::default(),
            RepGuarantees::default(),
        )
        .expect("the right arena must have the same local shape");

    assert!(
        right.try_get(left_id).is_none(),
        "a plan ID from another build must not be accepted by a catalog with the same local index"
    );
}

#[test]
fn container_storage_uses_the_owned_element_representation() {
    let ws = checked_ws_of("value V2 { x: float, y: float }\nenum E { A(int), B(int), None }\n");
    let arena = RepArena::default();
    let ty = MonoTy::App {
        ctor: MonoCtor::Array,
        params: vec![nominal(&ws, "V2")],
    };
    let plan = ContainerPlan::for_type(
        &ws,
        bone_core::hir::FnBackend::Lua,
        &arena,
        ContainerFamily::Vec,
        &ty,
    )
    .expect("a concrete Vec type must have a total container plan");
    assert!(
        matches!(plan.storage(), ContainerStorage::Columns { .. }),
        "the Vec storage decision must be owned by the container plan"
    );
    let elem = plan.column_elem(&arena).expect("column element");
    assert_eq!(elem.name, key(&ws, "V2"));
    assert!(!elem.nullable);
    assert_eq!(elem.representation(), plan.element_rep());
    assert!(
        plan.element_shape(&arena).is_some(),
        "the owned element plan must expose its shape"
    );
    assert!(
        arena.get(plan.element_rep()).column_eligible(),
        "the owned V2 plan must admit a nonzero column population"
    );

    let enum_rep = arena
        .plan(
            &ws,
            bone_core::hir::FnBackend::Lua,
            &nominal(&ws, "E"),
            RepUse::Boundary,
            &RepUseFacts::default(),
            RepGuarantees::default(),
        )
        .expect("the cdata enum must have an explicit representation plan");
    assert!(
        !arena.get(enum_rep).column_eligible(),
        "the enum control must remain outside the value-struct column population"
    );
}

#[test]
fn missing_bound_result_plan_is_an_explicit_error() {
    let ws = checked_ws_of("fn main() { print(\"ok\") }\n");
    let scx = ScalarCtx::new(&ws);
    let interner = skull_mir::monoty::MonoInterner::new();
    let result =
        bound_next_shape_from_concrete(&ws, &scx, &interner, &ConcreteSignature::default());
    assert!(
        result.is_err(),
        "a missing concrete bound result must not become a no-op"
    );
}

fn native_buffer_plan(
    ws: &bone_core::workspace::Workspace,
    arena: &RepArena,
    element: &str,
) -> ContainerPlan {
    let ty = MonoTy::App {
        ctor: MonoCtor::Nominal(key(ws, "NativeBuffer")),
        params: vec![nominal(ws, element)],
    };
    ContainerPlan::for_type(
        ws,
        bone_core::hir::FnBackend::Lua,
        arena,
        ContainerFamily::NativeBuffer,
        &ty,
    )
    .expect("a NativeBuffer value must have a concrete container plan")
}

#[test]
fn leaf_places_carry_the_container_element_representation() {
    let ws = checked_ws_of("nobox value V2 { x: float, y: float }");
    let plan = native_buffer_plan(&ws, &RepArena::default(), "V2");
    assert!(plan.is_leaf_addressed());
    let addresses = plan.leaf_places().expect("V2 has a leaf address plan");
    let places: Vec<_> = addresses
        .iter()
        .map(|a| (a.path.as_str(), a.view_field.as_str(), a.stride, a.offset))
        .collect();
    assert_eq!(places, [("x", "__vd", 2, 0), ("y", "__vd", 2, 1)]);
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
fn boundary_class_and_transport_contract_have_one_owner() {
    let ws = checked_ws_of(
        r#"
@nomangle inline(lua) fn boxed(v: int) -> int = "return v"
@nomangle @scalar_abi inline(lua) fn scalar(v: int) -> int = "return v"
fn main() { print("{boxed(1)} {scalar(2)}") }
"#,
    );
    let boxed = &ws.module(func_ref(&ws, "boxed").module).functions[func_ref(&ws, "boxed").func];
    let scalar = &ws.module(func_ref(&ws, "scalar").module).functions[func_ref(&ws, "scalar").func];
    assert_eq!(
        declared_boundary_class(boxed),
        Some(BoundaryClass::InlineBody)
    );
    assert_eq!(
        declared_boundary_class(scalar),
        Some(BoundaryClass::InlineBody)
    );
    assert!(boundary_requires_boxed_representation(boxed));
    assert!(!boundary_requires_boxed_representation(scalar));
}

#[test]
fn an_opaque_boundary_uses_an_explicit_boundary_abi_plan() {
    let ws = checked_ws_of(
        r#"
nobox value V2 { x: float, y: float }
@scalar_abi inline(lua) fn boundary(v: V2) -> V2 = "return v"
fn main() { boundary(V2 { x: 1.0, y: 2.0 }) }
"#,
    );
    let scx = ScalarCtx::new(&ws);
    let boundary = Instance::base(func_ref(&ws, "boundary"));
    let abi = BackendFnAbiPlan::for_boundary_instance(&ws, &scx, &boundary);
    assert_eq!(abi.param_leaf_widths(), Some(vec![2]));
    assert_eq!(abi.result_lanes(), 2);
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
fn function_value_plan_keeps_opaque_reference_parameters_explicit() {
    let ws = checked_ws_of("fn main() { print(\"ok\") }\n");
    let scx = ScalarCtx::new(&ws);
    let vec_int = MonoTy::App {
        ctor: MonoCtor::Array,
        params: vec![MonoTy::Int],
    };
    let plan = BackendFnAbiPlan::for_function_value(
        &ws,
        &scx,
        &MonoTy::Function {
            params: vec![vec_int],
            ret: Box::new(MonoTy::Int),
        },
        true,
    )
    .expect("a function value may carry an opaque container parameter");
    assert_eq!(
        plan.params().len(),
        1,
        "the function-value population must be nonzero"
    );
    assert!(
        !plan.uses_scalar_abi(),
        "an opaque container must not acquire a scalar ABI"
    );
}

#[test]
fn function_value_plan_mode_is_part_of_the_owned_signature() {
    let ws = checked_ws_of("value V2 { x: float, y: float }\nfn main() { print(\"ok\") }\n");
    let scx = ScalarCtx::new(&ws);
    let ty = MonoTy::Function {
        params: vec![nominal(&ws, "V2")],
        ret: Box::new(nominal(&ws, "V2")),
    };
    let direct = BackendFnAbiPlan::for_function_value(&ws, &scx, &ty, false)
        .expect("direct nested function mode must plan");
    let taken = BackendFnAbiPlan::for_function_value(&ws, &scx, &ty, true)
        .expect("taken nested function mode must plan");
    assert!(
        direct.uses_scalar_abi(),
        "direct-only value functions may use their concrete leaves"
    );
    assert!(
        !taken.uses_scalar_abi(),
        "taken functions must keep an ordinary value opaque"
    );
    assert_ne!(
        direct.params(),
        taken.params(),
        "the two physical modes must have distinct plans"
    );
}

#[test]
fn first_class_function_values_match_first_class_boundary_declarations() {
    let ws = checked_ws_of(
        r#"
nobox value Leaf { x: float, y: float }
fn named(value: Leaf) { print("ok") }
fn main() { print("main") }
"#,
    );
    let scx = ScalarCtx::new_values_first_class(&ws);
    let named = func_ref(&ws, "named");
    let declaration = BackendFnAbiPlan::for_boundary_declaration(&ws, &scx, named);
    let value = BackendFnAbiPlan::for_function_value(
        &ws,
        &scx,
        &MonoTy::Function {
            params: vec![nominal(&ws, "Leaf")],
            ret: Box::new(MonoTy::Unit),
        },
        true,
    )
    .expect("a first-class function value must have a physical plan");
    assert_eq!(
        value,
        declaration.representation().clone(),
        "a first-class indirect caller must use the declaration's opaque aggregate lane"
    );
    assert!(!value.uses_scalar_abi());
}

#[test]
fn aggregate_value_projection_has_an_owned_value_context() {
    let ws = checked_ws_of("nobox value V2 { x: float, y: float }\nfn main() { print(\"ok\") }\n");
    let arena = RepArena::default();
    let id = arena
        .plan(
            &ws,
            bone_core::hir::FnBackend::Lua,
            &nominal(&ws, "V2"),
            RepUse::Value,
            &RepUseFacts::default(),
            RepGuarantees {
                nobox: true,
                allow_opaque: true,
            },
        )
        .expect("a concrete aggregate value must have a physical plan");
    let plan = arena.get(id);
    assert_eq!(plan.use_context(), RepUse::Value);
    assert_eq!(plan.value_storage(), Some(ValueStorage::CData));
    assert!(
        matches!(plan.layout(), RepLayout::Leaves(_)),
        "aggregate plan must carry leaves"
    );
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
fn column_storage_mir_carries_the_owned_nobox_fact() {
    let ws = checked_ws_of(
        r#"
value V2 { x: float, y: float }
nobox value NV2 { x: float, y: float }

fn main() {
    let ordinary: [V2] = []
    ordinary.push(V2 { x: 1.0, y: 2.0 })
    let marked: [NV2] = []
    marked.push(NV2 { x: 3.0, y: 4.0 })
}
"#,
    );
    let main = func_ref(&ws, "main");
    let fixture = MirFixture::new(&ws, &[main]);
    let mir = fixture.lower(main).expect("lower test root");
    let mut plans = std::collections::BTreeMap::new();
    skull_mir::mir::column_elem_rep_plans(&mir, &mut plans);
    assert_eq!(plans.get(&key(&ws, "V2")).map(|p| p.0), Some(false));
    assert_eq!(plans.get(&key(&ws, "NV2")).map(|p| p.0), Some(true));
}

#[test]
fn nominal_value_storage_is_owned_by_the_rep_plan() {
    let ws = checked_ws_of("value Boxed { text: string, tag: int }\n");
    let arena = RepArena::default();
    let id = arena
        .plan_nominal_value(&ws, bone_core::hir::FnBackend::Lua, key(&ws, "Boxed"))
        .expect("a GC-carrying value must have an explicit physical plan");
    let plan = arena.get(id);
    assert_eq!(plan.value_storage(), Some(ValueStorage::GcTable));
    assert!(matches!(plan.layout(), RepLayout::Leaves(_)));
    assert_eq!(
        arena
            .nominal_value_storage_required(&ws, bone_core::hir::FnBackend::Lua, key(&ws, "Boxed"))
            .expect("the required storage plan must exist"),
        ValueStorage::GcTable
    );
}

#[test]
fn nominal_value_paths_and_addresses_are_owned_by_the_rep_plan() {
    let ws =
        checked_ws_of("value V2 { x: float, y: float }\nvalue Boxed { child: V2, tag: int }\n");
    let arena = RepArena::default();
    let expected = vec![
        Ustr::from("child.x"),
        Ustr::from("child.y"),
        Ustr::from("tag"),
    ];
    assert_eq!(
        arena
            .nominal_value_leaf_paths_required(
                &ws,
                bone_core::hir::FnBackend::Lua,
                key(&ws, "Boxed"),
            )
            .expect("the required leaf-path plan must exist"),
        expected
    );
    let plan = native_buffer_plan(&ws, &arena, "Boxed");
    let addresses = plan
        .leaf_places()
        .expect("the concrete value plan must own nonzero leaf addresses");
    assert_eq!(
        addresses
            .iter()
            .map(|address| address.path)
            .collect::<Vec<_>>(),
        expected
    );
    assert!(addresses.iter().all(|address| address.stride > 0));
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
fn nominal_c_layout_aligns_flat_and_nested_fields() {
    let ws = checked_ws_of(
        "value Mixed { on: bool, count: int, scale: f32 }\n\
         value V2 { x: f32, y: f32 }\n\
         value V4 { x: f32, y: f32, z: f32, w: f32 }\n\
         value SpriteFragU {\
             lights: bool, unlit: bool, count: int, ambient: V4,\
             glow: bool, normal: bool, params: V2\n\
         }",
    );
    let arena = RepArena::default();
    let cases: &[(&str, usize, &[(&str, usize)])] = &[
        ("Mixed", 12, &[("on", 0), ("count", 4), ("scale", 8)]),
        (
            "SpriteFragU",
            36,
            &[
                ("lights", 0),
                ("unlit", 1),
                ("count", 4),
                ("ambient.x", 8),
                ("ambient.y", 12),
                ("ambient.z", 16),
                ("ambient.w", 20),
                ("glow", 24),
                ("normal", 25),
                ("params.x", 28),
                ("params.y", 32),
            ],
        ),
    ];
    for &(name, expected_size, expected_leaves) in cases {
        let (leaves, size, align) = arena
            .nominal_leaf_layout(&ws, bone_core::hir::FnBackend::Lua, key(&ws, name))
            .unwrap_or_else(|err| panic!("{name}: {err}"));
        let offsets: Vec<_> = leaves
            .iter()
            .map(|leaf| (leaf.path.as_str(), leaf.byte_off))
            .collect();
        assert_eq!(offsets, expected_leaves, "{name}");
        assert_eq!((size, align), (expected_size, 4), "{name}");
    }
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
    assert!(
        !rep.boxable(),
        "transport omits fields required for a complete box"
    );

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
fn match_parameter_demand_retains_nested_payload_and_guard_fields() {
    let ws = checked_ws_of(
        r#"
nobox value DemandPayload { a: int, b: int, c: int, d: int }
enum DemandSubject { Present(DemandPayload), Empty }
nobox value DemandEvent { unused: int, subject: DemandSubject, guard_value: int }
fn demand_match_selected(event: DemandEvent) -> int {
    return match event.subject {
        DemandSubject::Present(payload) => payload.c,
        DemandSubject::Empty => 0,
    }
}
fn demand_match_guarded(event: DemandEvent) -> int {
    return match event.subject {
        DemandSubject::Present(payload) if event.guard_value > 0 => payload.c,
        _ => 0,
    }
}
fn demand_match_nested_pattern(event: DemandEvent) -> int {
    return match event.subject {
        DemandSubject::Present(DemandPayload { a: 0..=10, c: kept, .. }) => kept,
        DemandSubject::Empty => 0,
        _ => -1,
    }
}
global demand_match_calls: int = 0
fn demand_match_effect(n: int) -> int { demand_match_calls += 1; return n }
fn demand_match_call(event: DemandEvent) -> int {
    return match event.subject {
        DemandSubject::Present(payload) => demand_match_effect(payload.c),
        _ => 0,
    }
}
"#,
    );
    let roots = [
        "demand_match_selected",
        "demand_match_guarded",
        "demand_match_call",
        "demand_match_nested_pattern",
    ]
    .map(|name| func_ref(&ws, name));
    let fixture = MirFixture::new(&ws, &roots);
    for (root, expected) in roots.into_iter().zip([5, 6, 5, 5]) {
        let mir = fixture.lower(root).expect("lower test root");
        assert_eq!(mir.params.len(), expected, "{}", mir.name);
    }
}

#[test]
fn match_to_nullable_unit_enum_compacts_wide_event_subject() {
    let padding = (0..63)
        .map(|i| format!("cold_{i}: int"))
        .collect::<Vec<_>>()
        .join(", ");
    let src = format!(
        r#"
nobox value UnitMatchIdentity {{ scene: int, arena: int, index: int, generation: int }}
enum UnitMatchSubject {{ Absent, Primary, First(UnitMatchIdentity), Second(UnitMatchIdentity) }}
enum UnitMatchDirection {{ ToPrimary, ToEntity }}
nobox value UnitMatchEvent {{ {padding}, subject: UnitMatchSubject }}
fn demand_unit_match_direction(event: UnitMatchEvent) -> UnitMatchDirection? {{
    return match event.subject {{
        UnitMatchSubject::Primary => UnitMatchDirection::ToPrimary,
        UnitMatchSubject::First(_) => UnitMatchDirection::ToEntity,
        UnitMatchSubject::Second(_) => UnitMatchDirection::ToEntity,
        UnitMatchSubject::Absent => null,
    }}
}}
fn demand_unit_match_caller(event: UnitMatchEvent) -> UnitMatchDirection? {{
    return demand_unit_match_direction(event)
}}
"#
    );
    let ws = checked_ws_of(&src);
    let caller = func_ref(&ws, "demand_unit_match_caller");
    let selected = func_ref(&ws, "demand_unit_match_direction");
    let fixture = MirFixture::new(&ws, &[caller]);
    let entry = fixture.lower(selected).expect("lower test root");
    let caller = fixture.lower(caller).expect("lower test root");
    assert_eq!(
        caller.params.len(),
        68,
        "unsupported forwarding body keeps original event layout"
    );
    assert_eq!(
        entry.params.len(),
        5,
        "subject keeps tag and all identity payload lanes"
    );
    assert!(caller.insts.iter().any(|(_, inst)| matches!(inst,
        skull_mir::mir::Inst::Call { func, args } if *func == selected && args.len() == 5)));
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

/// `T?` at `T = V2?` is `V2?` itself, so it can only plan as `V2?` does.
#[test]
fn a_nullable_over_a_nullable_value_plans_as_the_single_nullable() {
    let ws = checked_ws_of("nobox value V2 { x: float, y: float }");
    let once = MonoTy::nullable(nominal(&ws, "V2"));
    assert_eq!(MonoTy::nullable(once.clone()), once);
    let arena = RepArena::default();
    let id = arena
        .plan(
            &ws,
            bone_core::hir::FnBackend::Lua,
            &once,
            RepUse::Return,
            &RepUseFacts::default(),
            RepGuarantees::default(),
        )
        .expect("a nullable nobox value must plan");
    let plan = arena.get(id);
    assert_eq!(
        (plan.value_storage(), plan.column_eligible(), plan.nobox()),
        (Some(ValueStorage::CData), true, true)
    );
}
