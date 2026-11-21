use crate::common::{emit_module, function_body, mangled_name, run_main};

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
