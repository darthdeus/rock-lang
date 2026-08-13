//! Expansion validation follows retained concrete instances, not generic source bodies.

use crate::common::{function_body, run_luajit, try_emit_module};

const UNIFORM_BLOCK_SOURCE: &str = include_str!("../../../stone/engine/rendering/uniform_block.rock");

const STRUCT_VALID: &str = r#"
enum E { F(float), I(int) }
value S { x: float }

@struct_expand inline(lua) fn pairs<T>(u: T) -> [(string, E)] = """
    error("pairs must be compiler-expanded")
"""

fn wrapped<T>(u: T) -> int {
    return pairs(u).len()
}

fn main() {
    print(wrapped(S { x: 1.0 }))
}
"#;

const STRUCT_INVALID: &str = r#"
enum E { F(float), I(int) }
value Bad { flag: bool }

@struct_expand inline(lua) fn pairs<T>(u: T) -> [(string, E)] = """
    error("pairs must be compiler-expanded")
"""

fn wrapped<T>(u: T) -> int {
    return pairs(u).len()
}

fn main() {
    print(wrapped(Bad { flag: true }))
}
"#;

const STRUCT_UNREACHABLE: &str = r#"
enum E { F(float), I(int) }

@struct_expand inline(lua) fn pairs<T>(u: T) -> [(string, E)] = """
    error("pairs must be compiler-expanded")
"""

fn unused<T>(u: T) -> int {
    return pairs(u).len()
}

fn main() {
    print(1)
}
"#;

const VERTEX_VALID: &str = r#"
value V { pos: Vec2, rot: float }

@vertex_expand inline(lua) fn stride<T>(buf: FlatVec<T>) -> int = """
    error("stride must be compiler-expanded")
"""

fn wrapped<T>(buf: FlatVec<T>) -> int {
    return stride(buf)
}

fn main() {
    let xs: FlatVec<V> = flat_vec_new()
    print(wrapped(xs))
}
"#;

const VERTEX_INVALID: &str = r#"
value WithId { pos: Vec2, id: int }

@vertex_expand inline(lua) fn stride<T>(buf: FlatVec<T>) -> int = """
    error("stride must be compiler-expanded")
"""

fn wrapped<T>(buf: FlatVec<T>) -> int {
    return stride(buf)
}

fn main() {
    let xs: FlatVec<WithId> = flat_vec_new()
    print(wrapped(xs))
}
"#;

const UNIFORM_VALID: &str = r#"
value U { radius: float, direction: Vec2 }

fn generic_id<T>(u: T) -> int {
    return uniform_block_id(u)
}

fn direct_id(u: U) -> int {
    return uniform_block_id(u)
}

fn main() {
    let u = U { radius: 1.0, direction: v(2.0, 3.0) }
    let direct = direct_id(u)
    let generic = generic_id(u)
    print(direct != 0)
    print(generic == direct)
}
"#;

const UNIFORM_INVALID: &str = r#"
value Inner { a: float }
value Outer { i: Inner, b: float }

fn generic_id<T>(u: T) -> int {
    return uniform_block_id(u)
}

fn main() {
    print(generic_id(Outer { i: Inner { a: 1.0 }, b: 2.0 }))
}
"#;

const UNIFORM_VALUES_VALID: &str = r#"
value U {
    radius: float,
    direction: Vec2,
    enabled: bool,
    count: int,
    packed: uint,
}

fn generic_values<T>(u: T) -> SemanticUniformBlock {
    return uniform_values(u)
}

fn main() {
    let packet = generic_values(U {
        radius: 1.0,
        direction: v(2.0, 3.0),
        enabled: true,
        count: 7,
        packed: 0xffffffff,
    })
    print(packet.fingerprint != 0)
    print(packet.leaves.len())
}
"#;

const UNIFORM_VALUES_INVALID: &str = r#"
value Bad { text: string }

fn generic_values<T>(u: T) -> SemanticUniformBlock {
    return uniform_values(u)
}

fn main() {
    print(generic_values(Bad { text: "bad" }).leaves.len())
}
"#;

#[test]
fn generic_struct_wrapper_uses_the_reached_concrete_instance() {
    let lua = try_emit_module(STRUCT_VALID).expect("valid concrete struct expansion");
    assert!(lua.matches("function wrapped_").count() > 0, "no retained concrete wrapper body");
    assert!(lua.contains("__variant = \"F\""), "no concrete struct expansion was emitted:\n{lua}");
    assert_eq!(run_luajit(&format!("{lua}\nmain()\n"), "generic_struct_expand"), "1");
}

#[test]
fn invalid_concrete_struct_wrapper_rejects_at_the_expander() {
    let err = try_emit_module(STRUCT_INVALID).expect_err("invalid concrete struct expansion");
    assert!(err.message.contains("no variant"), "wrong rejection:\n{}", err.message);
    assert!(err.message.contains("flag: bool"), "rejection lost the concrete field:\n{}", err.message);
}

#[test]
fn unreachable_generic_struct_wrapper_is_not_source_rejected() {
    let lua = try_emit_module(STRUCT_UNREACHABLE).expect("unreachable generic expansion is inert");
    assert!(lua.contains("function main"), "the reachable control did not emit:\n{lua}");
}

#[test]
fn generic_vertex_wrapper_uses_the_reached_concrete_instance() {
    let lua = try_emit_module(VERTEX_VALID).expect("valid concrete vertex expansion");
    assert!(lua.matches("function wrapped_").count() > 0, "no retained concrete wrapper body");
    assert!(lua.contains("do return (3) end"), "no concrete vertex stride was emitted:\n{lua}");
    assert_eq!(run_luajit(&format!("{lua}\nmain()\n"), "generic_vertex_expand"), "3");
}

#[test]
fn invalid_concrete_vertex_wrapper_rejects_at_the_expander() {
    let err = try_emit_module(VERTEX_INVALID).expect_err("invalid concrete vertex expansion");
    assert!(err.message.contains("integer attribute"), "wrong rejection:\n{}", err.message);
    assert!(err.message.contains("id"), "rejection lost the concrete field:\n{}", err.message);
}

#[test]
fn generic_uniform_wrapper_matches_the_direct_fingerprint() {
    let lua = try_emit_module(UNIFORM_VALID).expect("valid concrete uniform expansion");
    assert!(lua.matches("function generic_id_").count() > 0, "no retained concrete wrapper body");
    assert!(
        !function_body(&lua, "generic_id_").contains("uniform_block_id("),
        "the concrete wrapper still calls the compiler-owned expansion:\n{lua}"
    );
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "generic_uniform_expand"),
        "true\ntrue"
    );
}

#[test]
fn invalid_concrete_uniform_wrapper_rejects_at_the_expander() {
    let err = try_emit_module(UNIFORM_INVALID).expect_err("invalid concrete uniform expansion");
    assert!(err.message.contains("@uniform_expand"), "wrong rejection:\n{}", err.message);
    assert!(err.message.contains("Outer"), "rejection lost the concrete subject:\n{}", err.message);
}

#[test]
fn generic_uniform_values_wrapper_uses_the_reached_concrete_instance() {
    let lua = try_emit_module(UNIFORM_VALUES_VALID).expect("valid concrete uniform-values expansion");
    assert!(lua.matches("function generic_values_").count() > 0, "no retained concrete wrapper body");
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "generic_uniform_values"),
        "true\n6"
    );
}

#[test]
fn invalid_concrete_uniform_values_wrapper_rejects_at_the_expander() {
    let err = try_emit_module(UNIFORM_VALUES_INVALID).expect_err("invalid concrete uniform-values expansion");
    assert!(err.message.contains("@uniform_values"), "wrong rejection:\n{}", err.message);
    assert!(err.message.contains("Bad"), "rejection lost the concrete subject:\n{}", err.message);
}

#[test]
fn shader_uniform_setter_is_an_ordinary_generic_function() {
    assert!(
        UNIFORM_BLOCK_SOURCE.contains("fn set_uniform_block<T>(shader_name: string, u: T)"),
        "the shader setter still has no concrete-instance generic function"
    );
    assert!(
        !UNIFORM_BLOCK_SOURCE.contains("macro set_uniform_block("),
        "the obsolete source-body macro workaround remains"
    );
    assert!(
        UNIFORM_BLOCK_SOURCE.contains("fn __upload_resolved_block<T>("),
        "the generic upload helper still uses the obsolete macro workaround"
    );
    assert!(
        !UNIFORM_BLOCK_SOURCE.contains("macro __upload_resolved_block("),
        "the generic upload helper remains a macro"
    );
}
