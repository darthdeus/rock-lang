//! Callee-closure emission (`emit::emit_with_closure`), the entry `rock test
//! --backend js` uses per @test: mapped stdlib intrinsics stay inline templates.
//! Unmapped callees reject as `multi_rejection` pins.

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
