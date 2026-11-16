//! Callee-closure emission (`emit::emit_with_closure`), the entry `rock test
//! --backend js` uses per @test: mapped stdlib intrinsics stay inline templates,
//! and an unmapped `inline(lua)` callee rejects naming itself.

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
