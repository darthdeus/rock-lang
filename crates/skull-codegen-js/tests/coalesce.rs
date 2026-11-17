use crate::common::{emit_closure, run_node, ws_with};

#[test]
fn nullable_map_coalesce_keeps_only_the_insert_box() {
    let ws = ws_with(
        r#"
value P { a: int, b: string }
fn root(seed: int) -> int {
    let values: Map<string, P?> = map_new()
    values["p"] = P { a: seed, b: "x" }
    let p = values.get("p") ?? P { a: 0, b: "fallback" }
    return p.a
}
"#,
    );
    let js = emit_closure(&ws, "root").expect("coalesce emits");
    assert_eq!(js.matches("__ty: \"P\"").count(), 1, "{js}");
    assert_eq!(
        run_node(&format!(
            "{}{}{}{js}\nconsole.log(root(7));",
            skull_codegen_js::JS_VALUE_INTEGER_CHECK,
            skull_codegen_js::JS_STRUCTURAL_EQ,
            skull_codegen_js::JS_TABLE_PRIM
        ))
        .trim(),
        "7"
    );
}
