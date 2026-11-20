use crate::common::{emit_v1_package, module_id, run_luajit, ws_v1_package};

#[test]
fn a_nomangle_v1_type_keeps_its_declared_symbol() {
    let emitted = emit_v1_package(
        "v1abi",
        &[(
            "main.rock",
            r#"
pub @nomangle value AbiStats { hits: int, ratio: float }
pub @nomangle struct AbiHandle { handle: int }

inline(lua) fn stats_ratio() -> float = """
    local s = ffi.new("AbiStats[1]")
    s[0].ratio = 0.5
    return s[0].ratio * (ffi.sizeof("AbiStats") > 0 and 1 or 0)
"""

inline(lua) fn handle_tag(h: AbiHandle) -> string = """
    return __rock_get_type(h)
"""

pub @nomangle fn probe() -> string {
    let h = AbiHandle { handle: 3 }
    return "{stats_ratio()} {handle_tag(h)} {h.handle}"
}
fn main() {}
"#,
        )],
    );
    assert_eq!(
        run_luajit(&format!("{}\nprint(probe())\n", emitted.lua)),
        "0.5 AbiHandle 3"
    );
}

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
