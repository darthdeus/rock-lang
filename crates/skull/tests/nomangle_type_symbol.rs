use crate::common::{emit_v1_package, module_id, run_luajit, ws_v1_package};

#[test]
fn a_nomangle_type_cannot_take_a_legacy_types_symbol() {
    let ws = ws_v1_package(
        "v1clash",
        &[(
            "main.rock",
            r#"
pub @nomangle value Vec2 { a: float, b: float }
fn main() {}
"#,
        )],
    );
    let mid = module_id(&ws, "main.rock");
    let Err(err) = skull::mir_emit::emit_workspace_mir(&ws, mid) else {
        panic!("a @nomangle `Vec2` must not share the stdlib Vec2's symbol");
    };
    assert!(err.message.contains("would both emit as `Vec2`"));
}
