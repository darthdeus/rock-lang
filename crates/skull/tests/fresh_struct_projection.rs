use crate::common::{emit_module, function_body, run_luajit};

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
        assert!(!body.contains("__ty ="), "{body}");
        assert!(!body.contains("FreshPair_ct("), "{body}");
    }
}

#[test]
fn fresh_struct_projection_keeps_operand_effects_and_escaping_mutation() {
    let lua = emit_module(SOURCE);
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "fresh_struct_projection"),
        "7 2\n7 23\n7 23\n26 2"
    );
}

#[test]
fn fresh_default_constant_updates_through_incremental_and_overlay_emission() {
    use bone_semantic::hir::BodyOwner;
    use skull::mir_emit::{emit_full, emit_incremental, emit_overlay};
    let source = "const FRESH_DEFAULT: int = 7\nstruct FreshDefaults { selected: int = FRESH_DEFAULT, spare: int = 9 }\n@nomangle fn selected_default() -> int { return FreshDefaults {}.selected }";
    let before = crate::common::ws_of(source);
    let mid = skull::user_module_id(&before).unwrap();
    let (full, mut incremental_cache) = emit_full(&before, mid).unwrap();
    let (_, mut overlay_cache) = emit_full(&before, mid).unwrap();
    let after = crate::common::ws_of(&source.replace("= 7", "= 13"));
    let id = after
        .module(mid)
        .globals
        .iter()
        .find(|(_, g)| g.name.as_str() == "FRESH_DEFAULT")
        .unwrap()
        .0;
    let edited = [(mid, BodyOwner::Global(id))];
    let incremental = emit_incremental(&after, &mut incremental_cache, &edited).unwrap();
    assert_eq!(incremental.lua, emit_full(&after, mid).unwrap().0.lua);
    let overlay = emit_overlay(&after, &mut overlay_cache, &edited).unwrap();
    let run = format!(
        "{}\nassert(selected_default() == 7)\n{}\nassert(selected_default() == 13)\nprint('ok')",
        full.lua, overlay.lua
    );
    assert_eq!(run_luajit(&run, "fresh_default_overlay"), "ok");
}
