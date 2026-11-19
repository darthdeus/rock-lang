use crate::common::{emit_module, run_fn};

const SRC: &str = r#"
enum Shape { Circle(int), Square(int), Point }

fn classify(n: int) -> int {
    let s = Shape::Circle(n)
    if n % 3 == 1 { s = Shape::Square(n * 2) }
    if n % 3 == 2 { s = Shape::Point }
    return match s {
        Shape::Circle(rad) => rad + 1,
        Shape::Square(side) => side * 2,
        Shape::Point => 7,
    }
}

fn eq_escaping() -> int {
    let xs = [Shape::Circle(5), Shape::Circle(5), Shape::Square(5)]
    let a = if xs[0] == xs[1] { 1 } else { 0 }
    let b = if xs[0] == xs[2] { 1 } else { 0 }
    return a * 10 + b
}

@test(output = "1 4 7 4 | 10")
fn t() {
    print("{classify(0)} {classify(1)} {classify(2)} {classify(3)} | {eq_escaping()}")
}

fn main() { t() }
"#;

#[test]
fn cdata_enum_emits_ctype_and_no_table() {
    let lua = emit_module(SRC);
    assert!(lua.contains("} Shape;") && lua.contains("Shape_ct = ffi.typeof"));
    assert!(!lua.contains("__variant = \"Circle\""));
    assert!(lua.contains("ffi.metatype, \"Shape\""));
    assert!(lua.contains(".__tag"));
    assert_eq!(run_fn(&lua, "t"), "1 4 7 4 | 10");
}

const FIELD_SRC: &str = r#"
enum Blend { Normal, Add(int), Mul(int) }
value Cmd { z: int, blend: Blend }

fn apply(c: Cmd) -> int {
    return c.z + match c.blend {
        Blend::Normal => 0,
        Blend::Add(n) => n,
        Blend::Mul(n) => c.z * n,
    }
}

@test(output = "15 30")
fn t() {
    let a = Cmd { z: 5, blend: Blend::Add(10) }
    let b = Cmd { z: 5, blend: Blend::Mul(5) }
    print("{apply(a)} {apply(b)}")
}

fn main() { t() }
"#;

const MULTI_SRC: &str = r#"
enum Shape { Circle(int), Rect(int, int), Point }
fn area(s: Shape) -> int {
    return match s {
        Shape::Circle(r) => r * 3,
        Shape::Rect(w, h) => w * h,
        Shape::Point => 0,
    }
}
@test(output = "15 12 0")
fn t() {
    let a = area(Shape::Circle(5))
    let b = area(Shape::Rect(3, 4))
    let c = area(Shape::Point)
    print("{a} {b} {c}")
}
fn main() { t() }
"#;

const RECORD_SRC: &str = r#"
enum Shape { Circle(int), Box { w: int, h: int }, Point }
fn area(s: Shape) -> int {
    return match s {
        Shape::Circle(r) => r * 3,
        Shape::Box { w, h } => w * h,
        Shape::Point => 0,
    }
}
@test(output = "15 12 0 | Shape::Box { h = 4, w = 3 }")
fn t() {
    let a = area(Shape::Circle(5))
    let b = area(Shape::Box { w: 3, h: 4 })
    let c = area(Shape::Point)
    let d = Shape::Box { w: 3, h: 4 }
    print("{a} {b} {c} | {inspect(d)}")
}
fn main() { t() }
"#;

const PARAM_MATCH_SRC: &str = r#"
enum Target { Screen, Canvas(int) }
fn route(t: Target) -> int {
    return match t {
        Target::Screen => 0,
        Target::Canvas(id) => 100 + id,
    }
}
@test(output = "0 105")
fn t() {
    let a = route(Target::Screen)
    let b = route(Target::Canvas(5))
    print("{a} {b}")
}
fn main() { t() }
"#;

#[test]
fn matched_scalar_abi_enum_param_does_not_repack() {
    let lua = emit_module(PARAM_MATCH_SRC);
    let body = crate::common::function_body(&lua, "route");
    assert!(!body.contains("Target_ct("));
}

#[test]
fn cdata_enum_payloads_share_one_ctype() {
    for (src, cdef, expected) in [
        (FIELD_SRC, "Blend blend;", "15 30"),
        (MULTI_SRC, "int _0; int _1; } Shape;", "15 12 0"),
        (
            RECORD_SRC,
            "int _0; int _w; int _h; } Shape;",
            "15 12 0 | Shape::Box { h = 4, w = 3 }",
        ),
    ] {
        let lua = emit_module(src);
        assert!(lua.contains(cdef), "{cdef}");
        assert_eq!(run_fn(&lua, "t"), expected);
    }
}
