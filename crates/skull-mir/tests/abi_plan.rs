//! Owned ABI and representation choices for bodies, boundaries and storage.
//! Expected physical layouts stay explicit so internally consistent but
//! incorrect representation choices still fail.

use crate::common::{checked_ws_of, key, signature_plan};
use std::sync::Arc;

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

fn lower_test_roots(
    ws: &bone_core::workspace::Workspace,
    roots: &[FuncRef],
) -> (ScalarCtx, skull_graph::concrete_body::ConcreteBodies) {
    let interner = Arc::new(skull_mir::monoty::MonoInterner::new());
    let graph =
        skull_graph::instance_graph::build_in(ws, roots.to_vec(), false, Arc::clone(&interner));
    let bodies = skull_graph::concrete_body::build(ws, &graph);
    let scx = ScalarCtx::new(ws).sharing(interner);
    (scx, bodies)
}

fn lower_test_root(
    ws: &bone_core::workspace::Workspace,
    scx: &ScalarCtx,
    bodies: &skull_graph::concrete_body::ConcreteBodies,
    fref: FuncRef,
) -> skull_mir::mir::MirFunc {
    let plan = bodies
        .plans
        .get(&Instance::base(fref))
        .expect("concrete test plan");
    skull_mir::lower::lower_instance_with_plans(ws, scx, plan, &bodies.plans)
        .unwrap_or_else(|e| panic!("lower concrete test root: {}", e.message))
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

/// A generic call is planned from the concrete instance that the graph retained;
/// there is no declaration-shaped shared body left to supply a second ABI.
#[test]
fn a_concrete_generic_body_states_its_owned_abi() {
    let ws = checked_ws_of(SRC);
    let ident = func_ref(&ws, "ident");
    let main = func_ref(&ws, "main");
    let (scx, bodies) = lower_test_roots(&ws, &[main]);
    let (_, concrete) = bodies
        .plans
        .iter()
        .find(|(instance, _)| instance.func == ident && !instance.args.is_empty())
        .expect("the graph must retain ident<V2>");
    let spec = BackendFnAbiPlan::for_concrete_plan(&ws, &scx, concrete);
    assert_eq!(
        spec.param_leaf_widths(),
        Some(vec![2]),
        "`ident<V2>` takes V2's two float leaves"
    );
    assert_eq!(spec.result_lanes(), 2, "and returns them on two lanes");
    let caller = lower_test_root(&ws, &scx, &bodies, main);
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
}

/// The control: a non-generic declaration has exactly one emitted body, and the
/// base plan already states its scalarized ABI. Without this arm the test
/// above could pass on an oracle that answers only for generics.
#[test]
fn a_non_generic_body_states_its_abi_on_its_base_instance() {
    let ws = checked_ws_of(SRC);
    let shift = func_ref(&ws, "shift");

    let (scx, bodies) = lower_test_roots(&ws, &[shift]);
    let concrete = bodies
        .plans
        .get(&Instance::base(shift))
        .expect("the graph must retain shift's concrete body");
    let plan = BackendFnAbiPlan::for_concrete_plan(&ws, &scx, concrete);
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
    let (scx, bodies) = lower_test_roots(&ws, &[main]);
    let caller = lower_test_root(&ws, &scx, &bodies, main);
    for (name, parameter_width, result_lanes) in [
        ("scalar", None, 1),
        ("nested", Some(3), 3),
        ("tuple", Some(3), 3),
        ("nullable", Some(3), 3),
        ("boxed", None, 1),
    ] {
        let subject = Instance::base(func_ref(&ws, name));
        let concrete = bodies.plans.get(&subject).expect("concrete body");
        let owned = BackendFnAbiPlan::for_concrete_plan(&ws, &scx, concrete);
        let entry = lower_test_root(&ws, &scx, &bodies, subject.func);
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
    let (scx, bodies) = lower_test_roots(&ws, &[apply, nested_func]);
    let nested_instance = Instance::base(nested_func);
    let nested_plan = bodies
        .plans
        .get(&nested_instance)
        .expect("nested concrete plan");
    let nested = BackendFnAbiPlan::for_concrete_plan(&ws, &scx, nested_plan);
    let leaf = nominal(&ws, "Leaf");
    let fn_value = BackendFnAbiPlan::for_function_value(
        &ws,
        &scx,
        &MonoTy::Function {
            params: vec![leaf.clone()],
            ret: Box::new(leaf),
        },
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
    let lowered = lower_test_root(&ws, &scx, &bodies, apply);
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
    let RepLayout::Tuple { slots, .. } = outer_plan.layout() else {
        panic!("outer tuple must retain its top-level return lanes")
    };
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
fn container_storage_uses_the_element_representation_plan() {
    let ws = checked_ws_of("nobox value V2 { x: float, y: float }");
    let arena = RepArena::default();
    let element = nominal(&ws, "V2");
    let ty = MonoTy::App {
        ctor: MonoCtor::Array,
        params: vec![element],
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
}

#[test]
fn container_columns_carry_owned_element_identity() {
    let ws = checked_ws_of("value V2 { x: float, y: float }\nfn main() { print(\"ok\") }\n");
    let scx = ScalarCtx::new(&ws);
    let arena = RepArena::default();
    let ty = MonoTy::App {
        ctor: MonoCtor::Array,
        params: vec![nominal(&ws, "V2")],
    };
    let plan = ContainerPlan::for_type(&ws, scx.target(), &arena, ContainerFamily::Vec, &ty)
        .expect("value vector must have a container plan");
    let elem = plan
        .column_elem(&arena)
        .expect("V2 vector must use columns");
    assert_eq!(elem.representation(), plan.element_rep());
    assert!(
        plan.element_shape(&arena).is_some(),
        "the owned element plan must expose its shape"
    );
}

#[test]
fn element_rep_plan_owns_ffi_column_admission() {
    let ws = checked_ws_of("value V2 { x: float, y: float }\nenum E { A(int), B(int), None }\n");
    let arena = RepArena::default();
    let vec_ty = MonoTy::App {
        ctor: MonoCtor::Array,
        params: vec![nominal(&ws, "V2")],
    };
    let vec = ContainerPlan::for_type(
        &ws,
        bone_core::hir::FnBackend::Lua,
        &arena,
        ContainerFamily::Vec,
        &vec_ty,
    )
    .expect("V2 vector must have a container plan");
    assert!(
        arena.get(vec.element_rep()).column_eligible(),
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
fn column_element_plan_carries_the_nobox_guarantee() {
    let ws = checked_ws_of(
        "nobox value N { x: float, y: float }\n\
         value B { x: float, y: float }\n",
    );
    let arena = RepArena::default();
    let marked = MonoTy::App {
        ctor: MonoCtor::Array,
        params: vec![nominal(&ws, "N")],
    };
    let unmarked = MonoTy::App {
        ctor: MonoCtor::Array,
        params: vec![nominal(&ws, "B")],
    };
    let marked = ContainerPlan::for_type(
        &ws,
        bone_core::hir::FnBackend::Lua,
        &arena,
        ContainerFamily::Vec,
        &marked,
    )
    .expect("marked value vector must have a plan")
    .column_elem(&arena)
    .expect("marked value vector must use columns");
    let unmarked = ContainerPlan::for_type(
        &ws,
        bone_core::hir::FnBackend::Lua,
        &arena,
        ContainerFamily::Vec,
        &unmarked,
    )
    .expect("unmarked value vector must have a plan")
    .column_elem(&arena)
    .expect("unmarked value vector must use columns");
    assert!(
        marked.is_nobox(),
        "the marked element population must be nonzero"
    );
    assert!(
        !unmarked.is_nobox(),
        "the unmarked control must stay boxed at element use sites"
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

#[test]
fn leaf_places_carry_the_container_element_representation() {
    let ws = checked_ws_of("nobox value V2 { x: float, y: float }");
    let arena = RepArena::default();
    let ty = MonoTy::App {
        ctor: MonoCtor::Nominal(key(&ws, "NativeBuffer")),
        params: vec![nominal(&ws, "V2")],
    };
    let plan = ContainerPlan::for_type(
        &ws,
        bone_core::hir::FnBackend::Lua,
        &arena,
        ContainerFamily::NativeBuffer,
        &ty,
    )
    .expect("a NativeBuffer value must have a concrete container plan");
    assert!(plan.is_leaf_addressed());
    let address = arena
        .nominal_leaf_addresses(&ws, bone_core::hir::FnBackend::Lua, key(&ws, "V2"))
        .expect("V2 has a leaf address plan")
        .into_iter()
        .next()
        .expect("V2 has a nonzero leaf population");
    let place = plan
        .place(address.clone())
        .expect("leaf storage owns the place");
    assert_eq!(place, address);
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
    let direct = BackendFnAbiPlan::for_function_value_with_mode(&ws, &scx, &ty, false)
        .expect("direct nested function mode must plan");
    let taken = BackendFnAbiPlan::for_function_value_with_mode(&ws, &scx, &ty, true)
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
    let (scx, bodies) = lower_test_roots(&ws, &[main]);
    let mir = lower_test_root(&ws, &scx, &bodies, main);
    let mut plans = std::collections::BTreeMap::new();
    skull_mir::mir::column_elem_rep_plans(&mir, &mut plans);
    assert_eq!(plans.get(&key(&ws, "V2")).map(|p| p.0), Some(false));
    assert_eq!(plans.get(&key(&ws, "NV2")).map(|p| p.0), Some(true));
    assert!(
        plans.len() >= 2,
        "the column plan population must be nonzero"
    );
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
fn nominal_value_leaf_paths_are_owned_by_the_rep_plan() {
    let ws =
        checked_ws_of("value V2 { x: float, y: float }\nvalue Boxed { child: V2, tag: int }\n");
    let arena = RepArena::default();
    assert_eq!(
        arena
            .nominal_value_leaf_paths_required(
                &ws,
                bone_core::hir::FnBackend::Lua,
                key(&ws, "Boxed"),
            )
            .expect("the required leaf-path plan must exist"),
        vec![
            Ustr::from("child.x"),
            Ustr::from("child.y"),
            Ustr::from("tag")
        ]
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
fn nominal_leaf_addresses_are_owned_by_the_rep_plan() {
    let ws = checked_ws_of("value V2 { x: float, y: float }\nvalue Boxed { pos: V2, tag: int }\n");
    let arena = RepArena::default();
    let addresses = arena
        .nominal_leaf_addresses(&ws, bone_core::hir::FnBackend::Lua, key(&ws, "Boxed"))
        .expect("the concrete value plan must own nonzero leaf addresses");
    assert_eq!(
        addresses
            .iter()
            .map(|address| address.path)
            .collect::<Vec<_>>(),
        vec![Ustr::from("pos.x"), Ustr::from("pos.y"), Ustr::from("tag")]
    );
    assert!(addresses.iter().all(|address| address.stride > 0));
}

#[test]
fn nominal_c_layout_is_owned_by_the_rep_plan() {
    let ws = checked_ws_of("value Mixed { on: bool, count: int, scale: f32 }");
    let arena = RepArena::default();
    let (leaves, size, align) = arena
        .nominal_leaf_layout(&ws, bone_core::hir::FnBackend::Lua, key(&ws, "Mixed"))
        .expect("the concrete value plan must own the validated C layout");
    assert_eq!(
        leaves.iter().map(|leaf| leaf.path).collect::<Vec<_>>(),
        vec![Ustr::from("on"), Ustr::from("count"), Ustr::from("scale")]
    );
    assert_eq!(size, 12);
    assert_eq!(align, 4);
}

#[test]
fn nominal_c_layout_aligns_nested_fields_before_recurse() {
    let ws = checked_ws_of(
        "value V2 { x: f32, y: f32 }\n\
         value V4 { x: f32, y: f32, z: f32, w: f32 }\n\
         value SpriteFragU {\
             lights: bool, unlit: bool, count: int, ambient: V4,\
             glow: bool, normal: bool, params: V2\
         }",
    );
    let arena = RepArena::default();
    let (leaves, size, align) = arena
        .nominal_leaf_layout(&ws, bone_core::hir::FnBackend::Lua, key(&ws, "SpriteFragU"))
        .expect("the nested C layout must be representable");
    assert_eq!(
        leaves
            .iter()
            .map(|leaf| (leaf.path, leaf.byte_off))
            .collect::<Vec<_>>(),
        vec![
            (Ustr::from("lights"), 0),
            (Ustr::from("unlit"), 1),
            (Ustr::from("count"), 4),
            (Ustr::from("ambient.x"), 8),
            (Ustr::from("ambient.y"), 12),
            (Ustr::from("ambient.z"), 16),
            (Ustr::from("ambient.w"), 20),
            (Ustr::from("glow"), 24),
            (Ustr::from("normal"), 25),
            (Ustr::from("params.x"), 28),
            (Ustr::from("params.y"), 32),
        ]
    );
    assert_eq!(size, 36);
    assert_eq!(align, 4);
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
    let holder = key(&ws, "Holder");
    let child = Ustr::from("child");
    let direct = Ustr::from("direct");
    let plan = struct_field_plan(&ws, bone_core::hir::FnBackend::Lua, holder, child, false)
        .expect("known struct field must have a plan");
    assert_eq!(plan.inline_child(), Some(key(&ws, "Child")));
    assert!(
        plan.nullable(),
        "the shared plan must retain field presence"
    );
    assert_eq!(
        struct_field_plan(
            &ws,
            bone_core::hir::FnBackend::Lua,
            holder,
            Ustr::from("n"),
            false
        )
        .expect("known nobox field must have a plan")
        .inline_child(),
        Some(key(&ws, "N"))
    );
    let direct_plan = struct_field_plan(&ws, bone_core::hir::FnBackend::Lua, holder, direct, false)
        .expect("known primitive field must have a plan");
    assert!(direct_plan.is_direct());
    assert_eq!(direct_plan.direct_child(), None);
    let nested_plan = struct_field_plan(
        &ws,
        bone_core::hir::FnBackend::Lua,
        key(&ws, "Outer"),
        Ustr::from("inner"),
        false,
    )
    .expect("known direct child must have a plan");
    assert!(nested_plan.is_direct());
    assert_eq!(nested_plan.direct_child(), Some(key(&ws, "Inner")));
    assert_eq!(
        struct_field_plan(
            &ws,
            bone_core::hir::FnBackend::Lua,
            holder,
            Ustr::from("coroutine"),
            false,
        )
        .expect("compiler-owned opaque nominal must have a direct plan")
        .direct_child(),
        Some(Ustr::from("Coroutine"))
    );

    let missing_struct = struct_field_plan(
        &ws,
        bone_core::hir::FnBackend::Lua,
        Ustr::from("Missing"),
        child,
        false,
    )
    .expect_err("a missing parent struct must reject");
    assert!(
        missing_struct.message.contains("struct `Missing`"),
        "{missing_struct}"
    );

    let missing_field = struct_field_plan(
        &ws,
        bone_core::hir::FnBackend::Lua,
        holder,
        Ustr::from("missing"),
        false,
    )
    .expect_err("a missing field must reject");
    assert!(
        missing_field.message.contains("field `missing`"),
        "{missing_field}"
    );

    let sid = *ws.struct_by_name.get(&holder).expect("holder declaration");
    ws.modules[sid.module.index()].structs[sid.def].fields[0].ty = bone_core::prelude::TyId::Local(
        comfy_la_arena::Idx::from_raw(comfy_la_arena::RawIdx::from(0)),
    );
    let unresolved = struct_field_plan(&ws, bone_core::hir::FnBackend::Lua, holder, child, false)
        .expect_err("an unresolved field type must reject");
    assert!(unresolved.message.contains("resolved type"), "{unresolved}");
}

#[test]
fn direct_leaf_reader_compacts_parameter_transport() {
    let ws = checked_ws_of(
        r#"
value Sample { x: float, y: float, z: float }
fn selected(v: Sample) -> float { return v.y }
fn main() -> float { return selected(Sample { x: 1.0, y: 2.0, z: 3.0 }) }
"#,
    );
    let selected = func_ref(&ws, "selected");
    let main = func_ref(&ws, "main");
    let (scx, bodies) = lower_test_roots(&ws, &[main]);
    let caller = lower_test_root(&ws, &scx, &bodies, main);
    let entry = lower_test_root(&ws, &scx, &bodies, selected);
    assert_eq!(
        entry.params.len(),
        1,
        "entry binds only the demanded interior field"
    );
    let args = caller
        .insts
        .iter()
        .find_map(|(_, inst)| match inst {
            skull_mir::mir::Inst::Call { func, args } if *func == selected => Some(args),
            _ => None,
        })
        .expect("direct call");
    assert_eq!(args.len(), 1, "caller uses the same compact transport");
    assert!(
        matches!(caller.insts[args[0]], skull_mir::mir::Inst::Const(skull_mir::mir::Lit::Float(v)) if v == 2.0)
    );
}

#[test]
fn demanded_top_level_field_retains_nested_layout_and_nobox_guarantee() {
    let ws = checked_ws_of(
        r#"
nobox value Inner { x: float, y: float }
nobox value Outer { unused: float, kept: Inner, tail: float }
fn selected(v: Outer) -> float { return v.kept.y }
fn main(v: Outer) -> float { return selected(v) }
"#,
    );
    let selected = func_ref(&ws, "selected");
    let main = func_ref(&ws, "main");
    let (scx, bodies) = lower_test_roots(&ws, &[main]);
    let entry = lower_test_root(&ws, &scx, &bodies, selected);
    assert_eq!(
        entry.params.len(),
        2,
        "selected nested field retains both lanes"
    );
    let plan = entry.abi_plan.as_ref().unwrap();
    let rep = plan.rep_plan(plan.representation().params()[0]);
    assert!(rep.nobox());
    assert!(
        !rep.boxable(),
        "partial transport is not a complete boxable value"
    );
}

#[test]
fn whole_use_and_mutation_keep_complete_parameter_transport() {
    let ws = checked_ws_of(
        r#"
value Sample { x: float, y: float }
fn identity(v: Sample) -> Sample { return v }
fn mutate(v: Sample) -> float { v = Sample { x: v.y, y: v.x }; return v.x }
fn captured(v: Sample) -> fn() -> float { return () => v.x }
fn main(v: Sample) -> float { return mutate(identity(v)) }
"#,
    );
    let main = func_ref(&ws, "main");
    let identity = func_ref(&ws, "identity");
    let mutate = func_ref(&ws, "mutate");
    let captured = func_ref(&ws, "captured");
    let (scx, bodies) = lower_test_roots(&ws, &[main, captured]);
    for f in [identity, mutate, captured] {
        assert_eq!(lower_test_root(&ws, &scx, &bodies, f).params.len(), 2);
    }
}

#[test]
fn unused_aggregate_parameter_has_zero_transport_but_keeps_eager_call() {
    let ws = checked_ws_of(
        r#"
value Sample { x: float, y: float }
fn selected(v: Sample) -> float { return 5.0 }
fn main() -> float { return selected(Sample { x: 1.0, y: 2.0 }) }
"#,
    );
    let selected = func_ref(&ws, "selected");
    let main = func_ref(&ws, "main");
    let (scx, bodies) = lower_test_roots(&ws, &[main]);
    assert_eq!(
        lower_test_root(&ws, &scx, &bodies, selected).params.len(),
        0
    );
    let caller = lower_test_root(&ws, &scx, &bodies, main);
    assert!(caller.insts.iter().any(|(_, inst)| matches!(inst, skull_mir::mir::Inst::Call { func, args } if *func == selected && args.is_empty())));
}

#[test]
fn fixed_names_nullable_and_defaulted_parameters_keep_complete_transport() {
    let ws = checked_ws_of(
        r#"
value Sample { x: float, y: float }
@nomangle fn demand_fixed(v: Sample) -> float { return v.y }
fn demand_default(v: Sample = Sample { x: 1.0, y: 2.0 }) -> float { return v.y }
fn demand_nullable(v: Sample?) -> Sample? { return v }
"#,
    );
    let roots = [
        func_ref(&ws, "demand_fixed"),
        func_ref(&ws, "demand_default"),
        func_ref(&ws, "demand_nullable"),
    ];
    let (scx, bodies) = lower_test_roots(&ws, &roots);
    // Fixed-name boundaries use the existing boxed ABI; defaulted and nullable
    // internal parameters retain their complete declared leaf layouts.
    for (f, width) in roots.into_iter().zip([1, 2, 3]) {
        assert_eq!(lower_test_root(&ws, &scx, &bodies, f).params.len(), width);
    }
}

#[test]
fn safe_navigation_receiver_uses_the_same_compact_transport() {
    let ws = checked_ws_of(
        r#"
value Sample { x: float, y: float }
fn demand_method(v: Sample) -> float { return v.y }
fn main(v: Sample?) -> float { return v?.demand_method() ?? 0.0 }
"#,
    );
    let selected = func_ref(&ws, "demand_method");
    let main = func_ref(&ws, "main");
    let (scx, bodies) = lower_test_roots(&ws, &[main]);
    assert_eq!(
        lower_test_root(&ws, &scx, &bodies, selected).params.len(),
        1
    );
    let caller = lower_test_root(&ws, &scx, &bodies, main);
    assert!(caller.insts.iter().any(|(_, inst)| matches!(inst, skull_mir::mir::Inst::Call { func, args } if *func == selected && args.len() == 1)));
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
    let (scx, bodies) = lower_test_roots(&ws, &roots);
    for (root, expected) in roots.into_iter().zip([5, 6, 5, 5]) {
        let mir = lower_test_root(&ws, &scx, &bodies, root);
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
    let (scx, bodies) = lower_test_roots(&ws, &[caller]);
    let entry = lower_test_root(&ws, &scx, &bodies, selected);
    let caller = lower_test_root(&ws, &scx, &bodies, caller);
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
    let (scx, bodies) = lower_test_roots(&ws, &[root]);
    let mir = lower_test_root(&ws, &scx, &bodies, root);
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
