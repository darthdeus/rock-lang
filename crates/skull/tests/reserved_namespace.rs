use crate::common::emit_module;

#[test]
fn global_r_bare_compiles() {
    let lua = emit_module(
        r#"
global r: int = 1
fn use_it(a: int) -> int { return a + r }
fn main() { print("{use_it(1)}") }
"#,
    );
    assert!(lua.contains("r = (1)") || lua.contains("r ="));
}

#[test]
fn global_rx1_compiles() {
    let lua = emit_module(
        r#"
global rx1: int = 1
fn use_it(a: int) -> int { return a + rx1 }
fn main() { print("{use_it(1)}") }
"#,
    );
    assert!(lua.contains("rx1"));
}

#[test]
fn global_r1x_compiles() {
    let lua = emit_module(
        r#"
global r1x: int = 1
fn use_it(a: int) -> int { return a + r1x }
fn main() { print("{use_it(1)}") }
"#,
    );
    assert!(lua.contains("r1x"));
}

#[test]
fn fn_named_radius_compiles() {
    let lua = emit_module(
        r#"
fn radius(a: int) -> int { return a * 2 }
fn main() { print("{radius(3)}") }
"#,
    );
    assert!(lua.contains("radius"));
}

#[test]
fn global_rock_prefixed_non_env_compiles() {
    let lua = emit_module(
        r#"
global __rock_foo: int = 9
fn use_it(a: int) -> int { return a + __rock_foo }
fn main() { print("{use_it(1)}") }
"#,
    );
    assert!(lua.contains("__rock_foo"));
}

#[test]
fn stdlib_rock_prefixed_defs_compile() {
    let lua = emit_module(r#"fn main() { print("ok") }"#);
    assert!(!crate::common::definitions_matching(&lua, "main").is_empty());
}
