const LOWER_SOURCE: &str = include_str!("../../skull-mir/src/lower/mod.rs");
const EMIT_SOURCE: &str = include_str!("../src/mir_emit.rs");

use crate::common::try_emit_module;
use crate::common::ws_with_stdlib;
use ustr::Ustr;

const MIXED_INVALID: &str = r#"
enum Pairs { F(float), I(int) }
value BadPairs { flag: bool }

@struct_expand inline(lua) fn pairs<T>(u: T) -> [(string, Pairs)] = """
    error("pairs must be compiler-expanded")
"""

@vertex_expand inline(lua) fn stride<T>(u: T) -> int = """
    error("stride must be compiler-expanded")
"""

fn main() {
    // Lexically first, but lower priority than the struct expansion below.
    let _ = stride(3)
    let _ = pairs(BadPairs { flag: true })
}
"#;

const VERTEX_INVALID: &str = r#"
@vertex_expand inline(lua) fn stride<T>(u: T) -> int = """
    error("stride must be compiler-expanded")
"""

fn main() {
    let _ = stride(3)
}
"#;

fn direct_admission_error(src: &str) -> skull_parser::CompilerError {
    let ws = ws_with_stdlib(src);
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("test module");
    skull_mir::lower::expansion_rejection(&ws, mid).expect("fixture must be rejected by admission")
}

#[test]
fn expansion_admission_has_one_module_traversal() {
    let start = LOWER_SOURCE
        .find("pub fn expansion_rejection")
        .expect("combined expansion admission validator");
    let end = LOWER_SOURCE[start..]
        .find("/// Every distinct `rock_shader` pair")
        .map(|offset| start + offset)
        .expect("combined validator boundary");
    let body = &LOWER_SOURCE[start..end];

    assert_eq!(
        body.matches("resolve_call_fn(").count(),
        1,
        "the combined validator must resolve each candidate in one traversal"
    );
    for old in [
        "struct_expand_rejection",
        "shader_expand_rejection",
        "vertex_expand_rejection",
        "uniform_expand_rejection",
        "uniform_values_rejection",
    ] {
        assert_eq!(
            LOWER_SOURCE.matches(&format!("pub fn {old}")).count(),
            0,
            "legacy top-level validator `{old}` still owns a traversal"
        );
        assert_eq!(
            EMIT_SOURCE.matches(&format!("lower::{old}")).count(),
            0,
            "driver still calls legacy validator `{old}`"
        );
    }
    assert_eq!(
        EMIT_SOURCE.matches("lower::expansion_rejection").count(),
        1,
        "driver must call the combined validator exactly once"
    );
}

#[test]
fn mixed_invalid_uses_category_priority_over_lexical_order() {
    let err = try_emit_module(MIXED_INVALID).expect_err("mixed expansion fixture must reject");
    assert!(
        err.message.contains("`@struct_expand`"),
        "struct expansion category must win over the lexically-first vertex error:\n{}",
        err.message
    );
    assert!(
        !err.message.contains("`@vertex_expand`"),
        "the lower-priority lexical error became primary:\n{}",
        err.message
    );
}

#[test]
fn direct_admission_covers_struct_and_vertex_kinds() {
    let mixed = direct_admission_error(MIXED_INVALID);
    assert!(mixed.message.contains("`@struct_expand`"), "{mixed:?}");

    let vertex = direct_admission_error(VERTEX_INVALID);
    assert!(vertex.message.contains("`@vertex_expand`"), "{vertex:?}");
}
