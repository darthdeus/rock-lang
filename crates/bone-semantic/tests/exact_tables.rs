//! Lowered bodies and their inferences live until the program is emitted, so
//! the growth slack of the tables built for them must not live as long.

use bone_semantic::prelude::*;

#[test]
fn stored_bodies_and_inferences_hold_no_spare_capacity() {
    let mut src = String::from("fn id<T>(x: T) -> T {\n    x\n}\n");
    for i in 0..40 {
        src.push_str(&format!(
            "fn f{i}(xs: [int], y: float) -> float {{\n    let total: float = id(y)\n    let seen: [int] = []\n    for x in xs {{\n        total = total + float(x) * {i}.0\n    }}\n    total\n}}\n"
        ));
    }
    let mut modules =
        [skull_parser::parse_module_clean(&src, Ustr::from("exact.rock")).expect("parse")];
    let ws = semantic_pass(&mut modules).expect("semantic");
    let mid = ws.module_ids[&Ustr::from("exact.rock")];
    let module = ws.module(mid);
    assert!(!module.bodies.is_empty());
    for body in module.bodies.values() {
        assert_eq!(body.expr_arena.capacity(), body.expr_arena.len());
        assert_eq!(body.scope_arena.capacity(), body.scope_arena.len());
    }
    let (_, inference) = ws
        .inferences_iter()
        .find(|(id, _)| *id == mid)
        .expect("module inference");
    let typed: Vec<_> = inference
        .per_fn
        .values()
        .filter(|body| !body.ty_arena.is_empty())
        .collect();
    assert!(!typed.is_empty());
    for body in typed {
        assert_eq!(body.ty_arena.capacity(), body.ty_arena.len());
    }
}
