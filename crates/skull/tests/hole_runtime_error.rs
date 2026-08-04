//! A reached value/expression hole (`?`) must compile to a readable runtime
//! error, NOT silently become `nil`. Before this, a hole in a body hit the
//! `lower_expr` catch-all, the whole enclosing function was dropped from the
//! skipped list, and the program ran doing nothing -- a swallowed
//! not-implemented marker (violates "NEVER swallow errors").

use std::sync::LazyLock;
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

fn emit_module(src: &str) -> String {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    skull::mir_emit::emit_workspace_mir(&ws, mid).expect("emit").lua
}

#[test]
fn struct_field_hole_emits_a_runtime_error_and_keeps_the_function() {
    // The canonical case: `P { x: 1, y: ? }` -- a missing field value. Before,
    // the whole function was dropped and `p.y` silently read `nil`.
    let lua = emit_module(
        "struct P { x: int, y: int }\nfn boom() -> int {\n    let p = P { x: 1, y: ? }\n    return p.y\n}\nfn main() {}\n",
    );
    assert!(lua.contains("function boom"), "boom must be emitted, not skipped:\n{lua}");
    assert!(
        lua.contains("error(") && lua.contains("unfilled hole"),
        "hole must lower to a readable runtime error:\n{lua}"
    );
}

#[test]
fn typed_let_hole_emits_a_runtime_error() {
    let lua = emit_module("fn boom() {\n    let x: int = ?\n    print(\"{x}\")\n}\nfn main() {}\n");
    assert!(lua.contains("function boom"), "boom emitted:\n{lua}");
    assert!(
        lua.contains("error(") && lua.contains("unfilled hole"),
        "typed let hole -> runtime error:\n{lua}"
    );
}

#[test]
fn named_hole_reports_its_name() {
    let lua = emit_module("fn boom() -> int {\n    return ?width\n}\nfn main() {}\n");
    assert!(lua.contains("function boom"), "boom emitted:\n{lua}");
    assert!(lua.contains("?width"), "named hole must surface its name:\n{lua}");
}
