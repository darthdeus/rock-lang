use crate::common::{key, ws_with_stdlib};
use bone_semantic::inference::Workspace;
use skull_mir::lower::{ContainerFamily, ContainerPlan, RepArena};
use skull_mir::monoty::{MonoCtor, MonoTy};
use skull_parser::ast::FnBackend;
const SRC: &str = r#"
value ColPoint { x: int, y: int }
struct NotAValue { x: int }

fn use_them(p: ColPoint, n: NotAValue) -> int { return p.x + n.x }
"#;

fn nominal(ws: &Workspace, name: &str) -> MonoTy {
    MonoTy::App {
        ctor: MonoCtor::Nominal(key(ws, name)),
        params: Vec::new(),
    }
}

fn arena_of(ws: &Workspace, elem: MonoTy) -> MonoTy {
    MonoTy::App {
        ctor: MonoCtor::Nominal(key(ws, "Arena")),
        params: vec![elem],
    }
}

fn vec_of(elem: MonoTy) -> MonoTy {
    MonoTy::App {
        ctor: MonoCtor::Array,
        params: vec![elem],
    }
}

fn column_elem(
    ws: &Workspace,
    target: FnBackend,
    family: ContainerFamily,
    ty: &MonoTy,
) -> Option<skull_mir::mir::ColumnElem> {
    let arena = RepArena::default();
    ContainerPlan::for_type(ws, target, &arena, family, ty)
        .expect("well-formed container type must have a plan")
        .column_elem(&arena)
}

#[test]
fn the_container_plan_admits_and_rejects_on_the_vec_rule() {
    let ws = ws_with_stdlib(SRC);
    let t = FnBackend::Lua;
    let admitted = column_elem(
        &ws,
        t,
        ContainerFamily::Arena,
        &arena_of(&ws, nominal(&ws, "ColPoint")),
    )
    .expect("an all-scalar value struct is column-eligible");
    assert_eq!(admitted.name, key(&ws, "ColPoint"));
    assert!(!admitted.nullable);

    assert!(
        column_elem(
            &ws,
            t,
            ContainerFamily::Arena,
            &arena_of(&ws, nominal(&ws, "NotAValue"))
        )
        .is_none()
    );
    assert!(column_elem(&ws, t, ContainerFamily::Arena, &arena_of(&ws, MonoTy::Int)).is_none());
    // The `for`-over-arena walk reads occupied/generations for liveness and has
    // no presence concept, so a nullable element must stay boxed.
    assert!(
        column_elem(
            &ws,
            t,
            ContainerFamily::Arena,
            &arena_of(&ws, MonoTy::Nullable(Box::new(nominal(&ws, "ColPoint"))))
        )
        .is_none()
    );
    // A plan is target/family-specific; asking the Arena planner about a Vec is
    // an invalid boundary rather than a boxed storage answer.
    assert!(
        ContainerPlan::for_type(
            &ws,
            t,
            &RepArena::default(),
            ContainerFamily::Arena,
            &vec_of(nominal(&ws, "ColPoint")),
        )
        .is_err()
    );
}

#[test]
fn the_arena_and_vec_plans_cannot_disagree() {
    let ws = ws_with_stdlib(SRC);
    let mut admitted = 0;
    let mut rejected = 0;

    // EVERY backend, not just the one lowering happens to run: availability is
    // the per-target half of the decision, so a rule that reached Vec and not
    // Arena would only ever show up on the target it was written for.
    for target in [FnBackend::Any, FnBackend::Lua, FnBackend::Js] {
        for elem in [
            nominal(&ws, "ColPoint"),
            nominal(&ws, "NotAValue"),
            MonoTy::Int,
            MonoTy::String,
        ] {
            let a = column_elem(
                &ws,
                target,
                ContainerFamily::Arena,
                &arena_of(&ws, elem.clone()),
            );
            let v = column_elem(&ws, target, ContainerFamily::Vec, &vec_of(elem.clone()));
            assert_eq!(a, v);
            if a.is_some() {
                admitted += 1;
            } else {
                rejected += 1;
            }
        }
    }

    assert!(admitted > 0);
    assert!(rejected > 0);
}
