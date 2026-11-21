use crate::common::{edit, global_body, run_luajit, ws_of};

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
