use std::sync::Arc;

use crate::common::{MirFixture, decl, key};
use comfy_la_arena::Idx;
use skull_graph::instance_graph;
use skull_mir::instance::Instance;
use skull_mir::lower::{
    RepArena, RepGuarantees, RepId, RepUse, RepUseFacts, ScalarCtx, lower_instance_with_plans,
};
use skull_mir::mir::{ContainerKind, FuncRef, Inst, Lit, MirFunc, Term, ValueCtorForm};
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
        let root = decl(&ws, "main");
        for backend in [
            bone_core::hir::FnBackend::Lua,
            bone_core::hir::FnBackend::Js,
        ] {
            let err =
                MirFixture::with_context(&ws, &[root], ScalarCtx::new(&ws).with_target(backend))
                    .lower(root)
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
    let root = FuncRef {
        module: mid,
        func: fid,
    };
    MirFixture::new(&ws, &[root])
        .lower(root)
        .expect("safe-navigation assignments lower");

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

#[test]
#[cfg(debug_assertions)]
fn checked_lowering_rejects_incomplete_and_stale_type_tables() {
    use bone_core::hir::BodyOwner;

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
    let root = decl(&ws, "update");
    let mir = MirFixture::with_context(&ws, &[root], ScalarCtx::new_values_first_class(&ws))
        .lower(root)
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
    let root = FuncRef {
        module: mid,
        func: fid,
    };
    let mut mir = MirFixture::new(&ws, &[root]).lower(root).expect("lower");

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

use crate::common::lower as mir;

fn mir_std(src: &str, name: &str) -> MirFunc {
    let ws = crate::common::ws_of(src);
    MirFixture::new(&ws, &[decl(&ws, name)]).named(name)
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

mir_properties! {
    enum_tuple_variant_carries_payload(mir, "enum Shape { Circle(int), Square(int, int) }\nfn f() -> Shape { Shape::Circle(5) }", "f") |m| {
        assert_int_return(&m, &[0, 5, 0]);
    }
    struct_literal_injects_omitted_field_default(mir, "struct P { x: int, y: int = 0 }\nfn f() -> P { P { x: 1 } }", "f") |m| {
        let v = values(&m);
        assert_eq!(m.insts[v[0]], Inst::Const(Lit::Int(1)));
        assert_eq!(m.insts[v[1]], Inst::Const(Lit::Int(0)));
        assert_xy_fields(&m, "P");
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
