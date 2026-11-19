use crate::common::emit_module;

#[test]
fn struct_field_hole_emits_a_runtime_error_and_keeps_the_function() {
    let lua = emit_module(
        "struct P { x: int, y: int }\nfn boom() -> int {\n    let p = P { x: 1, y: ? }\n    return p.y\n}\nfn main() {}\n",
    );
    assert!(!crate::common::definitions_matching(&lua, "boom").is_empty());
    assert!(lua.contains("error(") && lua.contains("unfilled hole"));
}

#[test]
fn typed_let_hole_emits_a_runtime_error() {
    let lua = emit_module("fn boom() {\n    let x: int = ?\n    print(\"{x}\")\n}\nfn main() {}\n");
    assert!(!crate::common::definitions_matching(&lua, "boom").is_empty());
    assert!(lua.contains("error(") && lua.contains("unfilled hole"));
}

#[test]
fn named_hole_reports_its_name() {
    let lua = emit_module("fn boom() -> int {\n    return ?width\n}\nfn main() {}\n");
    assert!(!crate::common::definitions_matching(&lua, "boom").is_empty());
    assert!(lua.contains("?width"));
}
