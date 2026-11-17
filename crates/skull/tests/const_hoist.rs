use crate::common::{V1Fragment, emit_module, emit_v1_package, function_body, run_main};

fn count(hay: &str, needle: &str) -> usize {
    hay.matches(needle).count()
}

const TWO_CONST_NESTED: &str = r#"
value FixVec2 { x: float, y: float }
value Outer { a: FixVec2, b: FixVec2, c: float }

// A `[Outer]` Vec, NOT a NativeBuffer: `Outer` has 5 leaves, over
// `MAX_COLUMN_LEAVES`, so the Vec keeps boxed elements and the ctor genuinely
// has to be built. (A NativeBuffer element is leaf-addressed now and writes the
// constant leaves straight through its views, so it builds no ctor to hoist --
// which would make this test vacuous rather than exercise hoisting.)
fn fill(out: [Outer], n: int) {
    for i in 0..n {
        out.push(Outer { a: FixVec2 { x: 3.0, y: 7.0 }, b: FixVec2 { x: 3.0, y: 7.0 }, c: i as float })
    }
}

fn main() {
    let out: [Outer] = []
    fill(out, 4)
    print("{out.len}")
}
"#;

#[test]
fn const_value_struct_ctor_is_hoisted_to_module_scope() {
    let lua = emit_module(TWO_CONST_NESTED);
    // The distinctive ctor is BUILT exactly once, in a module-scope hoist def.
    let builds = count(&lua, "FixVec2_ct(3.0, 7.0)");
    assert_eq!(builds, 1);
    // The def is a `__skull_kc_*` global assignment, not inside the loop.
    assert!(lua.contains("__skull_kc_") && lua.contains("= FixVec2_ct(3.0, 7.0)"));
    // The hot function rebuilds nothing -- it references the global instead.
    let body = function_body(&lua, "fill");
    assert_eq!(count(body, "FixVec2_ct("), 0);
    assert!(body.contains("__skull_kc_"));
}

const PACKAGE_MAIN: &str = r#"
value Seg { a: a::Pt, b: a::Pt, c: float }
fn main() {
    let out: [Seg] = []
    for i in 0..2 {
        out.push(Seg { a: a::Pt { x: 3.0, y: 7.0 }, b: a::make(i as float), c: 1.0 })
    }
    print("{a::label(out[1].a)} {a::label(out[1].b)}")
}
"#;

const PACKAGE_MOD_A: &str = r#"
mod a;

pub value Pt {
    x: float,
    y: float,
}

pub fn make(x: float) -> Pt {
    return Pt { x, y: x }
}

pub fn label(p: Pt) -> string {
    return "{p.x}/{p.y}"
}
"#;

#[test]
fn hoisted_package_value_ctor_uses_the_emitted_type_symbol() {
    let emitted = emit_v1_package(
        "kcpkg",
        &[
            V1Fragment {
                file: "main.rock",
                src: PACKAGE_MAIN,
            },
            V1Fragment {
                file: "a.rock",
                src: PACKAGE_MOD_A,
            },
        ],
    );
    assert!(emitted.lua.contains("__skull_kc_"));
    let out = run_main(&emitted.lua);
    assert_eq!(out, "3/7 1/1");
}
