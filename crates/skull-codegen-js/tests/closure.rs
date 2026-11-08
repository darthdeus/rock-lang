//! Per-root transitive callee-closure emission (`emit::emit_with_closure`):
//! the generalization of the nb_differential co-emission that `rock test
//! --backend js` uses per @test fn. A root's plain-Rock callees are lowered and
//! co-emitted under FuncRef-deterministic names; mapped intrinsics stay
//! templates; anything else rejects with the emitter's own message.

/// FuncRef of the unique fn named `name` in the `<test>` module.
#[test]
fn co_emits_plain_rock_callee_transitively() {
    let ws = crate::common::ws_with(
        "fn leaf(x: int) -> int { x + 1 }\n\
         fn helper(x: int) -> int { leaf(x) * 2 }\n\
         fn root(y: int) -> int { helper(y) + leaf(y) }\n",
    );
    let root = crate::common::test_fn_ref(&ws, "root");
    let js = skull_codegen_js::emit::emit_with_closure(&ws, &[(root, "root".to_string())])
        .expect("closure emit");
    assert!(
        js.contains("function root("),
        "root under its given name:\n{js}"
    );
    // Callees under the shared (name + signature) mangle, one definition each
    // even though `leaf` is reached twice.
    assert_eq!(js.matches("function helper_int").count(), 1, "{js}");
    assert_eq!(js.matches("function leaf_int").count(), 1, "{js}");
}

#[test]
fn rejects_unmapped_inline_lua_callee_with_emitter_message() {
    let ws = crate::common::ws_with(
        "inline(lua) fn my_lua_thing(x: int) -> int = \"\"\" return x \"\"\"\n\
         fn root(y: int) -> int { my_lua_thing(y) }\n",
    );
    let root = crate::common::test_fn_ref(&ws, "root");
    let err = skull_codegen_js::emit::emit_with_closure(&ws, &[(root, "root".to_string())])
        .expect_err("inline(lua) callee must reject");
    assert!(
        err.message.contains("my_lua_thing") && err.message.contains("not a mapped JS intrinsic"),
        "reject reason names the callee: {}",
        err.message
    );
}

#[test]
fn user_fn_shadowing_intrinsic_name_is_co_emitted_not_mapped() {
    // A user fn sharing a table entry's (name, arity) -- `abs`/1, on a bool
    // param so overload resolution picks it over the stdlib float one -- must
    // NOT silently render as Math.abs: the template applies only to the
    // stdlib @intrinsic decl.
    let ws = crate::common::ws_with(
        "fn abs(b: bool) -> int { if b { 1 } else { 0 } }\n\
         fn root() -> int { abs(true) }\n",
    );
    let root = crate::common::test_fn_ref(&ws, "root");
    let js = skull_codegen_js::emit::emit_with_closure(&ws, &[(root, "root".to_string())])
        .expect("closure emit");
    assert!(
        !js.contains("Math.abs"),
        "user `abs` must not map to Math.abs:\n{js}"
    );
    assert!(
        js.contains("function abs_bool"),
        "user `abs` co-emitted:\n{js}"
    );
}

#[test]
fn stdlib_intrinsic_calls_stay_templates() {
    let ws = crate::common::ws_with("fn root(x: float) -> float { floor(x) + sqrt(x) }\n");
    let root = crate::common::test_fn_ref(&ws, "root");
    let js = skull_codegen_js::emit::emit_with_closure(&ws, &[(root, "root".to_string())])
        .expect("closure emit");
    assert!(js.contains("Math.floor"), "{js}");
    assert!(js.contains("Math.sqrt"), "{js}");
    // No co-emitted defs beyond the root.
    assert_eq!(js.matches("function ").count(), 1, "{js}");
}
