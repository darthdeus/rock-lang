//! `@nomangle` on a struct or value type keeps its declared name as the
//! emitted type symbol under modules v1: the C typedef, `ffi.new`/`ffi.sizeof`
//! strings and the `__ty` tag. Hand-written inline Lua that spells the type
//! (host engine's graphics-stats cdef, vertex buffers, snapshot's Font remap) relies
//! on that name.

/// Inline Lua names the exempt types by their declared names; the program only
/// runs if the typedef and the runtime tag use those names too.
#[test]
fn a_nomangle_v1_type_keeps_its_declared_symbol() {
    let emitted = crate::common::emit_v1_package(
        "v1abi",
        &[crate::common::V1Fragment {
            file: "main.rock",
            src: r#"
pub @nomangle value AbiStats { hits: int, ratio: float }
pub @nomangle struct AbiHandle { handle: int }

inline(lua) fn stats_ratio() -> float = """
    local s = ffi.new("AbiStats[1]")
    s[0].ratio = 0.5
    return s[0].ratio * (ffi.sizeof("AbiStats") > 0 and 1 or 0)
"""

inline(lua) fn handle_tag(h: AbiHandle) -> string = """
    return h.__ty
"""

pub @nomangle fn probe() -> string {
    let h = AbiHandle { handle: 3 }
    return "{stats_ratio()} {handle_tag(h)} {h.handle}"
}
fn main() {}
"#,
        }],
    );
    let out = crate::common::run_luajit(
        &format!("{}\nprint(probe())\n", emitted.lua),
        "v1_nomangle_type",
    );
    assert_eq!(out, "0.5 AbiHandle 3");
}

/// A `@nomangle` struct owns its declared name among type symbols. A legacy
/// type of that name (the stdlib's `Vec2`) would share the typedef, the
/// constructor and the runtime tag, so emission rejects the pair.
#[test]
fn a_nomangle_type_cannot_take_a_legacy_types_symbol() {
    let ws = crate::common::ws_v1_package(
        "v1clash",
        &[crate::common::V1Fragment {
            file: "main.rock",
            src: r#"
pub @nomangle value Vec2 { a: float, b: float }
fn main() {}
"#,
        }],
    );
    let mid = *ws.module_ids.get(&ustr::Ustr::from("main.rock")).expect("module id");
    let err = match skull::mir_emit::emit_workspace_mir(&ws, mid) {
        Ok(_) => panic!("a @nomangle `Vec2` must not share the stdlib Vec2's symbol"),
        Err(e) => e.message,
    };
    assert!(err.contains("would both emit as `Vec2`"), "{err}");
}
