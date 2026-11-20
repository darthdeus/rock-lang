use crate::common::{edit, emit_module, function_body, global_body, run_luajit, run_main, ws_of};

const SOURCE: &str = r#"
value FreshPair { x: int, y: int }
struct FreshEnvelope { pair: FreshPair, spare: int }
struct FreshScalar { selected: int, discarded: int }
global marks: int = 0
fn mark(n: int) -> int { marks = marks * 10 + n; return n }
fn read(p: FreshPair) -> int { return p.x }
@nomangle fn fresh_scalar() -> int {
    return FreshScalar { selected: 7, discarded: mark(2) }.selected
}
@nomangle fn fresh_call() -> int {
    return read(FreshEnvelope { pair: FreshPair { x: 7, y: mark(2) }, spare: mark(3) }.pair)
}
@nomangle fn fresh_nested() -> int {
    return FreshEnvelope { pair: FreshPair { x: 7, y: mark(2) }, spare: mark(3) }.pair.x
}
fn change(h: FreshScalar) { h.selected = 19 }
fn retained() -> int {
    let h = FreshScalar { selected: 7, discarded: mark(2) }
    let old = h.selected
    change(h)
    return old + h.selected
}
fn main() {
    marks = 0
    print("{fresh_scalar()} {marks}")
    marks = 0
    print("{fresh_call()} {marks}")
    marks = 0
    print("{fresh_nested()} {marks}")
    marks = 0
    print("{retained()} {marks}")
}
"#;

#[test]
fn fresh_struct_projection_eliminates_the_unobservable_container() {
    let lua = emit_module(SOURCE);
    for name in ["fresh_scalar", "fresh_call", "fresh_nested"] {
        let body = function_body(&lua, name);
        assert!(!body.contains("__rock_box_mts["));
        assert!(!body.contains("FreshPair_ct("));
    }
    assert_eq!(run_main(&lua), "7 2\n7 23\n7 23\n26 2");
}

#[test]
fn fresh_default_constant_updates_through_incremental_and_overlay_emission() {
    let source = "const FRESH_DEFAULT: int = 7\nstruct FreshDefaults { selected: int = FRESH_DEFAULT, spare: int = 9 }\n@nomangle fn selected_default() -> int { return FreshDefaults {}.selected }";
    let after = ws_of(&source.replace("= 7", "= 13"));
    let edit = edit(
        &ws_of(source),
        &after,
        &[global_body(&after, "FRESH_DEFAULT")],
    );
    let overlay = edit.overlay.expect("overlay");
    let run = format!(
        "{}\nassert(selected_default() == 7)\n{}\nassert(selected_default() == 13)\nprint('ok')",
        edit.base.lua, overlay.lua
    );
    assert_eq!(run_luajit(&run), "ok");
}
