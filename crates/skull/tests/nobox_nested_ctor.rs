use crate::common::{emit_module, function_body, mangled_name, run_main};

const GLOBAL_OUTER: &str = r#"
nobox value NV2 { x: float, y: float }
value E5 { pos: NV2, a: float, b: float, c: float }
global G: E5
fn setup() { G = E5 { pos: NV2 { x: 1.0, y: 2.0 }, a: 3.0, b: 4.0, c: 5.0 } }
fn main() {
    setup()
    print("{G.pos.x} {G.pos.y} {G.a} {G.b} {G.c}")
}
"#;

const TWO_LEVEL: &str = r#"
nobox value NV2 { x: float, y: float }
value Box2 { lo: NV2, hi: NV2 }
value Outer { b: Box2, a: float, c: float }
global G: Outer
fn setup() {
    G = Outer { b: Box2 { lo: NV2 { x: 1.0, y: 2.0 }, hi: NV2 { x: 3.0, y: 4.0 } }, a: 5.0, c: 6.0 }
}
fn main() {
    setup()
    print("{G.b.lo.x} {G.b.lo.y} {G.b.hi.x} {G.b.hi.y} {G.a} {G.c}")
}
"#;

const MIXED_LITERAL_AND_OPAQUE: &str = r#"
nobox value NV2 { x: float, y: float }
value Pair { lo: NV2, hi: NV2, w: float }
global G: Pair
global H: Pair
fn setup() {
    G = Pair { lo: NV2 { x: 1.0, y: 2.0 }, hi: NV2 { x: 3.0, y: 4.0 }, w: 5.0 }
    H = Pair { lo: NV2 { x: 6.0, y: 7.0 }, hi: G.lo, w: 8.0 }
}
fn main() {
    setup()
    print("{H.lo.x} {H.lo.y} {H.hi.x} {H.hi.y} {H.w}")
}
"#;

const NULLABLE_FIELD: &str = r#"
nobox value NV2 { x: float, y: float }
value Opt { pos: NV2? }
global G: Opt = Opt { pos: NV2 { x: 1.0, y: 2.0 } }
fn main() { print("{G.pos?.x ?? -1.0}") }
"#;

const SROA_SPLIT_REASSEMBLY: &str = r#"
nobox value NV2 { x: float, y: float }
value E3 { pos: NV2, a: float }
enum Part { One(E3) }
global FLAG: bool = true
fn sink(p: Part) -> float {
    match p { Part::One(e) => return e.a + e.pos.x + e.pos.y }
}
fn main() {
    let e = E3 { pos: NV2 { x: 1.0, y: 2.0 }, a: 4.0 }
    if FLAG { print("boundary") }
    print("{sink(Part::One(E3 { ..e, a: 8.0 }))}")
}
"#;

const NESTED_NOBOX_ACROSS_COROUTINE_FRAME: &str = r#"
nobox value NV2 { x: float, y: float }
value E3 { pos: NV2, a: float }
enum Part { One(E3) }
global FLAG: bool = true
fn sink(p: Part) -> float {
    match p { Part::One(e) => return e.a + e.pos.x + e.pos.y }
}
co fn build() -> Coroutine<float> {
    let e = E3 { pos: NV2 { x: 1.0, y: 2.0 }, a: 4.0 }
    yield 0.0
    if FLAG { print("boundary") }
    yield sink(Part::One(E3 { ..e, a: 8.0 }))
}
fn main() {
    let c = build()
    for n in c { print("{n as int}") }
}
"#;

#[test]
fn a_coroutine_frame_carries_a_value_with_a_nested_nobox_field() {
    let lua = emit_module(NESTED_NOBOX_ACROSS_COROUTINE_FRAME);
    let build = mangled_name(&lua, "build");
    assert!(!function_body(&lua, &format!("{build}__step")).contains("NV2_ct("));
    assert_eq!(run_main(&lua), "0\nboundary\n11");
}
