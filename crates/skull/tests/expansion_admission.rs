use crate::common::try_emit_module;
use crate::common::ws_with_stdlib;
use ustr::Ustr;

const MIXED_INVALID: &str = r#"
enum Pairs { F(float), I(int) }

@struct_expand inline(lua) fn pairs<T>(u: T) -> int = """
    error("pairs must be compiler-expanded")
"""

@vertex_expand inline(lua) fn stride<T>(u: T) -> int = """
    error("stride must be compiler-expanded")
"""

fn main() {
    // Lexically first, but lower priority than the struct expansion below.
    let _ = stride(3)
    let _ = pairs(3)
}
"#;

const VERTEX_INVALID: &str = r#"
@vertex_expand inline(lua) fn stride<T>(u: T) -> float = """
    error("stride must be compiler-expanded")
"""

fn main() {
    let _ = stride(3)
}
"#;

const SHADER_AND_STRUCT_INVALID: &str = r#"
enum Pairs { F(float), I(int) }

@struct_expand inline(lua) fn pairs<T>(u: T) -> int = """
    error("pairs must be compiler-expanded")
"""

fn main() {
    let _ = rock_shader(3)
    let _ = pairs(3)
}
"#;

fn direct_admission_error(src: &str) -> skull_parser::CompilerError {
    let ws = ws_with_stdlib(src);
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("test module");
    skull_mir::lower::expansion_rejection(&ws, mid).expect("fixture must be rejected by admission")
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

#[test]
fn struct_priority_beats_invalid_shader_pipeline() {
    let err = direct_admission_error(SHADER_AND_STRUCT_INVALID);
    assert!(
        err.message.contains("`@struct_expand`"),
        "struct expansion must retain the historical priority over shader expansion:\n{}",
        err.message
    );
    assert!(
        !err.message.contains("`rock_shader` takes either one argument"),
        "shader expansion incorrectly won over the higher-priority struct rejection:\n{}",
        err.message
    );
}
