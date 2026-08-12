//! The backend ABI of ONE EMITTED BODY (`docs/monomorphization-plan.md` cut
//! C+D, step A*).
//!
//! Pre-cutover, emission writes two populations of definition: one shared body
//! per declaration, plus a specialized body per `(FuncRef, Vec<MonoTy>)` the
//! lowerer decided to serve. Those are the emitted bodies, and each has its own
//! backend ABI -- a generic that takes and returns `T` is one boxed slot on the
//! declaration and two float leaves at `T = V2`. The declaration's answer is
//! not a narrower version of the instantiation's, it is a different question.
//!
//! What this pins is that ONE oracle answers it for whichever body is being
//! emitted, so a definition's parameter list and the call that targets it
//! cannot come from two derivations that merely agree today.

use std::sync::LazyLock;
use std::sync::Arc;

use skull_mir::instance::Instance;
use skull_mir::concrete::{ConcreteBodyPlan, ConcreteOwner, ConcreteSignature, ConcreteTypeTables};
use skull_mir::lower::{
    BackendFnAbiPlan, ContainerFamily, ContainerPlan, ContainerStorage, GlobalPlan, RepArena,
    RepGuarantees,
    declared_boundary_class, boundary_requires_boxed_representation, BoundaryClass, EnumLayout,
    EnumPlan, RepLayout, RepUse, RepUseFacts, ScalarCtx, ValueStorage,
};
use skull_mir::mir::FuncRef;
use skull_mir::monoty::{MonoCtor, MonoTy};
use ustr::Ustr;

static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_host::stdlib_loader::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(format!("<stdlib:{}>", f.name).as_str());
            skull_parser::parse_module_clean(f.contents, id).expect("stdlib parse")
        })
        .collect()
});

fn ws_of(src: &str) -> bone_semantic::inference::Workspace {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let errs = skull_mir::verdict::body_errors(&ws);
    assert!(errs.is_empty(), "fixture must type-check: {errs:?}");
    ws
}

fn func_ref(ws: &bone_semantic::inference::Workspace, name: &str) -> FuncRef {
    let overloads = ws.functions_by_name.get(&Ustr::from(name)).expect("declared");
    assert_eq!(overloads.len(), 1, "`{name}` must be unambiguous in the fixture");
    let (module, func) = overloads[0];
    FuncRef { module, func }
}

fn nominal(name: &str) -> MonoTy {
    MonoTy::App { ctor: MonoCtor::Nominal(Ustr::from(name)), params: Vec::new() }
}

fn lower_test_roots(
    ws: &bone_semantic::inference::Workspace,
    roots: &[FuncRef],
) -> (
    ScalarCtx,
    skull_graph::concrete_body::ConcreteBodies,
) {
    let interner = Arc::new(skull_mir::monoty::MonoInterner::new());
    let graph = skull_graph::instance_graph::build_in(
        ws,
        roots.to_vec(),
        false,
        Arc::clone(&interner),
    );
    let bodies = skull_graph::concrete_body::build(ws, &graph);
    let scx = ScalarCtx::new(ws).sharing(interner);
    (scx, bodies)
}

fn lower_test_root(
    ws: &bone_semantic::inference::Workspace,
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
    let ws = ws_of(SRC);
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
    let concrete_instance = concrete.function_instance().expect("function concrete owner");
    let call = caller
        .call_abi_plans
        .values()
        .find(|plan| plan.subject == *concrete_instance)
        .expect("the direct call must retain ident<V2>'s ABI plan");
    assert_eq!(call, &spec, "the generic call must consume ident<V2>'s owned plan");
}

/// The control: a non-generic declaration has exactly one emitted body, and the
/// base plan already states its scalarized ABI. Without this arm the test
/// above could pass on an oracle that answers only for generics.
#[test]
fn a_non_generic_body_states_its_abi_on_its_base_instance() {
    let ws = ws_of(SRC);
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
    let ws = ws_of(
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
    let v2 = scx.interner().intern(nominal("V2"));
    let plan = ConcreteBodyPlan {
        owner: ConcreteOwner::Function(Instance::base(sink)),
        types: ConcreteTypeTables::default(),
        foreign_domains: Default::default(),
        signature: ConcreteSignature {
            params: vec![v2],
            param_tuple_spelled: vec![false],
            result: Some(v2),
        },
        callees: Default::default(),
        callee_signatures: Default::default(),
        failed_sites: Default::default(),
        bound_tree: Default::default(),
    };
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
    let ws = ws_of(
        r#"
nobox value V2 { x: float, y: float }
@scalar_abi fn generic_tuple_take<T>(value: T) -> T { return value }
@scalar_abi fn written_tuple(value: (V2, float)) -> (V2, float) { return value }
fn main() { print("{generic_tuple_take((V2 { x: 1.0, y: 2.0 }, 3.0)).0.x}") }
"#,
    );
    let scx = ScalarCtx::new(&ws);
    let take = func_ref(&ws, "generic_tuple_take");
    let v2_ty = nominal("V2");
    let tuple = scx.interner().intern(MonoTy::Tuple(vec![v2_ty, MonoTy::Float]));
    let plan = ConcreteBodyPlan {
        owner: ConcreteOwner::Function(Instance::intern(
            scx.interner(),
            take,
            vec![MonoTy::Tuple(vec![MonoTy::App {
                ctor: MonoCtor::Nominal(Ustr::from("V2")),
                params: Vec::new(),
            }])],
        )),
        types: ConcreteTypeTables::default(),
        foreign_domains: Default::default(),
        signature: ConcreteSignature {
            params: vec![tuple],
            param_tuple_spelled: vec![false],
            result: Some(tuple),
        },
        callees: Default::default(),
        callee_signatures: Default::default(),
        failed_sites: Default::default(),
        bound_tree: Default::default(),
    };
    let abi = BackendFnAbiPlan::for_concrete_plan(&ws, &scx, &plan);
    assert_eq!(
        abi.param_leaf_widths(),
        None,
        "a tuple supplied through generic T must keep the generic boundary boxed"
    );
    assert_eq!(abi.result_lanes(), 3, "a generic tuple result keeps its multi-value return shape");

    let written = func_ref(&ws, "written_tuple");
    let written_plan = ConcreteBodyPlan {
        owner: ConcreteOwner::Function(Instance::base(written)),
        types: ConcreteTypeTables::default(),
        foreign_domains: Default::default(),
        signature: ConcreteSignature {
            params: vec![tuple],
            param_tuple_spelled: vec![true],
            result: Some(tuple),
        },
        callees: Default::default(),
        callee_signatures: Default::default(),
        failed_sites: Default::default(),
        bound_tree: Default::default(),
    };
    let written_abi = BackendFnAbiPlan::for_concrete_plan(&ws, &scx, &written_plan);
    assert_eq!(written_abi.param_leaf_widths(), Some(vec![3]));
}

/// The first RepPlan cut has to join every physical consumer of a concrete
/// function signature.  These are deliberately different shapes, not a
/// single scalar control: a plain scalar, a nested value, a tuple, a nullable
/// value, and a GC-carrying value exercise boxed and multi-lane answers.
#[test]
fn one_owned_representation_reaches_body_call_return_and_publisher() {
    let ws = ws_of(
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
    let cases = ["scalar", "nested", "tuple", "nullable", "boxed"];
    assert_eq!(cases.len(), 5, "the RED population must remain nonzero");

    for name in cases {
        let subject = Instance::base(func_ref(&ws, name));
        let concrete = bodies
            .plans
            .get(&subject)
            .unwrap_or_else(|| panic!("the graph must retain {name}'s concrete body"));
        let owned = BackendFnAbiPlan::for_concrete_plan(&ws, &scx, concrete);
        let entry = lower_test_root(&ws, &scx, &bodies, subject.func);
        let body = entry.abi_plan.as_ref().expect("callee entry must retain its ABI plan");
        let call = caller
            .call_abi_plans
            .values()
            .find(|plan| plan.subject == subject)
            .unwrap_or_else(|| panic!("caller must retain {name}'s concrete ABI plan"));

        assert_eq!(
            &owned,
            body,
            "body entry must consume the owned representation for {name}"
        );
        assert_eq!(
            &owned,
            call,
            "call and callee entry must consume one owned representation for {name}"
        );
        assert_eq!(
            owned.representation().result(),
            owned.representation().publisher_result(),
            "return lowering and the backend publisher must read the same result plan for {name}"
        );
        assert_eq!(
            owned.representation().return_lanes(),
            owned.result_lanes(),
            "publisher lane sizing must be derived from the owned result plan for {name}"
        );
    }

    let scalar = caller
        .call_abi_plans
        .values()
        .find(|plan| plan.subject.func == func_ref(&ws, "nested"))
        .expect("nested call ABI");
    let boxed = caller
        .call_abi_plans
        .values()
        .find(|plan| plan.subject.func == func_ref(&ws, "boxed"))
        .expect("boxed call ABI");
    assert!(scalar.param_leaf_widths().is_some(), "nested aggregate must use a scalar representation");
    assert!(
        boxed.param_leaf_widths().is_none(),
        "opaque aggregate must retain a boxed representation, got {:?}",
        boxed.param_leaf_widths()
    );
    assert!(
        caller
            .call_abi_plans
            .values()
            .find(|plan| plan.subject.func == func_ref(&ws, "tuple"))
            .expect("tuple call ABI")
            .result_lanes()
            > 1,
        "tuple return must have multiple physical lanes"
    );
    assert_eq!(
        caller
            .call_abi_plans
            .values()
            .find(|plan| plan.subject.func == func_ref(&ws, "tuple"))
            .expect("tuple call ABI")
            .param_leaf_widths(),
        Some(vec![3]),
        "the declaration-spelled tuple parameter must use its owned leaf plan"
    );

    let callee = func_ref(&ws, "nested");
    let main = func_ref(&ws, "main");
    let (scx, bodies) = lower_test_roots(&ws, &[main, callee]);
    let caller = lower_test_root(&ws, &scx, &bodies, main);
    let entry = lower_test_root(&ws, &scx, &bodies, callee);
    assert_eq!(entry.params.len(), 3, "callee entry must bind Nested's three owned lanes");
    let call = caller
        .call_abi_plans
        .values()
        .find(|plan| plan.subject == Instance::base(callee))
        .expect("caller must retain the concrete nested ABI plan");
    let nested_call_args = caller.insts.iter().find_map(|(_, inst)| match inst {
        skull_mir::mir::Inst::CallMulti { func, args, .. } if *func == callee => Some(args.len()),
        _ => None,
    });
    assert_eq!(nested_call_args, Some(3), "the direct call must consume Nested's three owned lanes");
    let entry_abi = entry.abi_plan.as_ref().expect("callee entry must retain its ABI plan");
    assert_eq!(entry_abi.representation(), call.representation());
}

#[test]
fn function_value_uses_the_owned_signature_representation() {
    let ws = ws_of(
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
    let nested_plan = bodies.plans.get(&nested_instance).expect("nested concrete plan");
    let nested = BackendFnAbiPlan::for_concrete_plan(&ws, &scx, nested_plan);
    let leaf = nominal("Leaf");
    let fn_value = BackendFnAbiPlan::for_function_value(
        &ws,
        &scx,
        &MonoTy::Function { params: vec![leaf.clone()], ret: Box::new(leaf) },
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
    assert_eq!(call, Some(2), "the indirect callee must splat Leaf through its owned plan");
}

#[test]
fn container_storage_uses_the_element_representation_plan() {
    let ws = ws_of("nobox value V2 { x: float, y: float }");
    let arena = RepArena::default();
    let element = nominal("V2");
    let ty = MonoTy::App { ctor: MonoCtor::Array, params: vec![element] };
    let plan = ContainerPlan::for_type(
        &ws,
        bone_semantic::hir::FnBackend::Lua,
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
    assert_eq!(elem.name, Ustr::from("V2"));
    assert!(!elem.nullable);
    assert_eq!(elem.representation(), plan.element_rep());
}

#[test]
fn container_columns_carry_owned_element_identity() {
    let ws = ws_of("value V2 { x: float, y: float }\nfn main() { print(\"ok\") }\n");
    let scx = ScalarCtx::new(&ws);
    let arena = RepArena::default();
    let ty = MonoTy::App {
        ctor: MonoCtor::Array,
        params: vec![nominal("V2")],
    };
    let plan = ContainerPlan::for_type(&ws, scx.target(), &arena, ContainerFamily::Vec, &ty)
        .expect("value vector must have a container plan");
    let elem = plan.column_elem(&arena).expect("V2 vector must use columns");
    assert_eq!(elem.representation(), plan.element_rep());
    assert!(plan.element_shape(&arena).is_some(), "the owned element plan must expose its shape");
}

#[test]
fn element_rep_plan_owns_ffi_column_admission() {
    let ws = ws_of("value V2 { x: float, y: float }\nenum E { A(int), B(int), None }\n");
    let arena = RepArena::default();
    let vec_ty = MonoTy::App {
        ctor: MonoCtor::Array,
        params: vec![nominal("V2")],
    };
    let vec = ContainerPlan::for_type(
        &ws,
        bone_semantic::hir::FnBackend::Lua,
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
            bone_semantic::hir::FnBackend::Lua,
            &nominal("E"),
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
fn leaf_places_carry_the_container_element_representation() {
    let ws = ws_of("nobox value V2 { x: float, y: float }");
    let arena = RepArena::default();
    let ty = MonoTy::App {
        ctor: MonoCtor::Nominal(Ustr::from("NativeBuffer")),
        params: vec![nominal("V2")],
    };
    let plan = ContainerPlan::for_type(
        &ws,
        bone_semantic::hir::FnBackend::Lua,
        &arena,
        ContainerFamily::NativeBuffer,
        &ty,
    )
    .expect("a NativeBuffer value must have a concrete container plan");
    assert!(plan.is_leaf_addressed());
    let address = skull_mir::lower::leaf_addr_plan(&ws, Ustr::from("V2"))
        .expect("V2 has a leaf address plan")
        .into_iter()
        .next()
        .expect("V2 has a nonzero leaf population");
    let place = plan.place(address).expect("leaf storage owns the place");
    assert_eq!(place.representation(), plan.element_rep());
}

#[test]
fn enum_layout_is_selected_by_one_owned_plan() {
    let cdata_ws = ws_of("enum E { A(int), B(int), None }");
    let cdata = EnumPlan::for_name(&cdata_ws, Ustr::from("E")).expect("enum plan");
    assert!(matches!(cdata.layout(), EnumLayout::Cdata(_)));
    assert!(cdata.cdata().is_some());
    assert!(cdata.flat().is_none());

    let flat_ws = ws_of(
        "nobox value P { x: int, y: int }\nenum E { Has(P), Nope }",
    );
    let flat = EnumPlan::for_name(&flat_ws, Ustr::from("E")).expect("enum plan");
    assert!(matches!(flat.layout(), EnumLayout::Flat(_)));
    assert!(flat.flat().is_some());
    assert!(flat.cdata().is_none());

    let unit_ws = ws_of("enum E { A, B }");
    let unit = EnumPlan::for_name(&unit_ws, Ustr::from("E")).expect("enum plan");
    assert!(unit.is_payload_free());
    assert!(matches!(unit.layout(), EnumLayout::PayloadFree));
}

#[test]
fn boundary_class_and_transport_contract_have_one_owner() {
    let ws = ws_of(
        r#"
@nomangle inline(lua) fn boxed(v: int) -> int = "return v"
@nomangle @scalar_abi inline(lua) fn scalar(v: int) -> int = "return v"
fn main() { print("{boxed(1)} {scalar(2)}") }
"#,
    );
    let boxed = &ws.module(func_ref(&ws, "boxed").module).functions[func_ref(&ws, "boxed").func];
    let scalar = &ws.module(func_ref(&ws, "scalar").module).functions[func_ref(&ws, "scalar").func];
    assert_eq!(declared_boundary_class(boxed), Some(BoundaryClass::InlineBody));
    assert_eq!(declared_boundary_class(scalar), Some(BoundaryClass::InlineBody));
    assert!(boundary_requires_boxed_representation(boxed));
    assert!(!boundary_requires_boxed_representation(scalar));
}

#[test]
fn an_opaque_boundary_uses_an_explicit_boundary_abi_plan() {
    let ws = ws_of(
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
    let ws = ws_of(
        "nobox value V2 { x: float, y: float }\nvalue Gc { text: string }",
    );
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
        MonoTy::App { ctor: MonoCtor::Array, params: vec![MonoTy::Int] },
        MonoTy::Function { params: vec![MonoTy::Int], ret: Box::new(MonoTy::Bool) },
        nominal("V2"),
    ];
    assert_eq!(valid.len(), 13, "the valid-type denominator must stay nonzero");
    let facts = RepUseFacts::default();
    let guarantees = RepGuarantees::default();
    for ty in &valid {
        arena
            .plan(
                &ws,
                bone_semantic::hir::FnBackend::Lua,
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
            bone_semantic::hir::FnBackend::Lua,
            &MonoTy::App { ctor: MonoCtor::Array, params: vec![MonoTy::Int] },
            RepUse::Parameter,
            &facts,
            RepGuarantees { nobox: true, allow_opaque: false },
        )
        .expect_err("a nobox container must not silently become an opaque slot");
    assert!(rejected.message.contains("nobox"));
    let rejected_nominal = arena
        .plan(
            &ws,
            bone_semantic::hir::FnBackend::Lua,
            &nominal("Gc"),
            RepUse::Parameter,
            &facts,
            RepGuarantees { nobox: true, allow_opaque: false },
        )
        .expect_err("a GC-carrying nominal must not silently become an opaque nobox slot");
    assert!(rejected_nominal.message.contains("nobox"));
}

#[test]
fn function_value_plan_keeps_opaque_reference_parameters_explicit() {
    let ws = ws_of("fn main() { print(\"ok\") }\n");
    let scx = ScalarCtx::new(&ws);
    let vec_int = MonoTy::App { ctor: MonoCtor::Array, params: vec![MonoTy::Int] };
    let plan = BackendFnAbiPlan::for_function_value(
        &ws,
        &scx,
        &MonoTy::Function { params: vec![vec_int], ret: Box::new(MonoTy::Int) },
    )
    .expect("a function value may carry an opaque container parameter");
    assert_eq!(plan.params().len(), 1, "the function-value population must be nonzero");
    assert!(!plan.uses_scalar_abi(), "an opaque container must not acquire a scalar ABI");
}

#[test]
fn function_value_plan_mode_is_part_of_the_owned_signature() {
    let ws = ws_of("value V2 { x: float, y: float }\nfn main() { print(\"ok\") }\n");
    let scx = ScalarCtx::new(&ws);
    let ty = MonoTy::Function {
        params: vec![nominal("V2")],
        ret: Box::new(nominal("V2")),
    };
    let direct = BackendFnAbiPlan::for_function_value_with_mode(&ws, &scx, &ty, false)
        .expect("direct nested function mode must plan");
    let taken = BackendFnAbiPlan::for_function_value_with_mode(&ws, &scx, &ty, true)
        .expect("taken nested function mode must plan");
    assert!(direct.uses_scalar_abi(), "direct-only value functions may use their concrete leaves");
    assert!(!taken.uses_scalar_abi(), "taken functions must keep an ordinary value opaque");
    assert_ne!(direct.params(), taken.params(), "the two physical modes must have distinct plans");
}

#[test]
fn aggregate_value_projection_has_an_owned_value_context() {
    let ws = ws_of("nobox value V2 { x: float, y: float }\nfn main() { print(\"ok\") }\n");
    let arena = RepArena::default();
    let id = arena
        .plan(
            &ws,
            bone_semantic::hir::FnBackend::Lua,
            &nominal("V2"),
            RepUse::Value,
            &RepUseFacts::default(),
            RepGuarantees { nobox: true, allow_opaque: true },
        )
        .expect("a concrete aggregate value must have a physical plan");
    let plan = arena.get(id);
    assert_eq!(plan.use_context(), RepUse::Value);
    assert_eq!(plan.value_storage(), Some(ValueStorage::CData));
    assert!(matches!(plan.layout(), RepLayout::Leaves(_)), "aggregate plan must carry leaves");
}

#[test]
fn global_storage_uses_an_owned_value_plan() {
    let ws = ws_of(
        "nobox value G { x: float, y: float }\n\
         global state: G = G { x: 1.0, y: 2.0 }\n\
         fn main() { print(\"ok\") }\n",
    );
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("test module");
    let gid = ws.module(mid).global_by_name[&Ustr::from("state")];
    let scx = ScalarCtx::new(&ws);
    let plan = GlobalPlan::for_declared_type(&ws, &scx, ws.module(mid).globals[gid].ty)
        .expect("a declared global must have a physical storage plan");
    assert_eq!(plan.use_context(), RepUse::Value);
    assert_eq!(plan.lanes(), 2, "the global plan must own both scalar leaves");
}
