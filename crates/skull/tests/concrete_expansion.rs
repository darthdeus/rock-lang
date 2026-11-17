use crate::common::{run_main, try_emit_module};

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

const VERTEX_VALID: &str = r#"
value V { pos: Vec2f, rot: f32 }

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

#[test]
fn concrete_expansion_uses_reached_generic_instances() {
    for (source, expected) in [
        (STRUCT_VALID, "1"),
        (VERTEX_VALID, "3"),
        (UNIFORM_VALID, "true\ntrue"),
        (UNIFORM_VALUES_VALID, "true\n6"),
    ] {
        assert_eq!(run_main(&try_emit_module(source).unwrap()), expected);
    }
    let unreachable = STRUCT_VALID
        .replace("print(wrapped(S { x: 1.0 }))", "print(1)")
        .replace("x: float", "flag: bool");
    assert_eq!(run_main(&try_emit_module(&unreachable).unwrap()), "1");
}

#[test]
fn concrete_expansion_preserves_diagnostics() {
    for (source, messages) in [
        (
            STRUCT_VALID
                .replace("x: float", "flag: bool")
                .replace("x: 1.0", "flag: true"),
            &["no variant", "flag: bool"] as &[_],
        ),
        (
            VERTEX_VALID.replace("rot: f32", "id: int"),
            &["integer attribute", "id"],
        ),
        (
            UNIFORM_VALID
                .replace(
                    "value U { radius: float, direction: Vec2 }",
                    "value Inner { a: float }\nvalue U { i: Inner, b: float }",
                )
                .replace(
                    "U { radius: 1.0, direction: v(2.0, 3.0) }",
                    "U { i: Inner { a: 1.0 }, b: 2.0 }",
                ),
            &["@uniform_expand", "U"],
        ),
        (
            UNIFORM_VALUES_VALID
                .replace("packed: uint", "packed: string")
                .replace("packed: 0xffffffff", "packed: \"bad\""),
            &["@uniform_values", "U"],
        ),
    ] {
        let error = try_emit_module(&source).unwrap_err();
        for message in messages {
            assert!(error.message.contains(message), "{error:?}");
        }
    }
}
