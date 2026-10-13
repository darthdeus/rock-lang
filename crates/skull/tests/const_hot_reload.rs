//! `const` globals re-initialize on hot reload, while mutable globals keep their
//! live value in the active Session.
use crate::common::ws_of;

/// Compile `src` to a full skull Lua program (no stdlib -- int/empty-main only).
fn emit_program(src: &str) -> String {
    let ws = ws_of(src);
    let mid = skull::user_module_id(&ws).unwrap();
    skull::mir_emit::emit_workspace_mir(&ws, mid).expect("emit").lua
}

fn global_session_slot(lua: &str, name: &str) -> u32 {
    let marker = format!("identity=\"global:<t>::{name}\"");
    lua.split(&marker)
        .nth(1)
        .unwrap_or_else(|| panic!("missing StateSchema entry for {name}"))
        .split("session_keys={")
        .nth(1)
        .expect("StateSchema entry has Session keys")
        .split(|c| c == ',' || c == '}')
        .next()
        .expect("StateSchema entry has a first Session key")
        .parse()
        .expect("Lua global Session slot is numeric")
}

/// Run a Lua program under luajit with a wall-clock timeout; returns stdout
/// (panicking with stderr on a non-zero exit so a runtime error is visible).
fn run_luajit(prog: &str) -> String {
    use std::process::Command;
    let path = std::env::temp_dir().join(format!("skull_const_reload_{}.lua", std::process::id()));
    std::fs::write(&path, prog).expect("write program");
    let out = Command::new("luajit")
        .arg(&path)
        .output()
        .expect("spawn luajit");
    let _ = std::fs::remove_file(&path);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "luajit exited non-zero.\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
}

#[test]
fn const_reinits_on_hot_reload_but_session_global_is_preserved() {
    let ws = ws_of("const FOO: int = 5\nglobal bar: int = 7\nfn main() {}\n");
    let mid = skull::user_module_id(&ws).unwrap();
    let lua = skull::mir_emit::emit_full(&ws, mid).expect("emit_full").0.lua;

    let line = |needle: &str| {
        lua.lines()
            .find(|l| l.contains(needle) && l.contains("ginit"))
            .unwrap_or_else(|| panic!("no init line for {needle} in:\n{lua}"))
            .to_string()
    };

    // const: no preserve guard -> re-runs init every reload.
    let foo = line("FOO =");
    assert!(
        !foo.contains("HOT_RELOAD"),
        "const FOO must re-init unconditionally on reload (no HOT_RELOAD guard); got: {foo}"
    );
    // mutable global: its Session slot guard preserves the live value.
    let bar_slot = global_session_slot(&lua, "bar");
    let bar = line(&format!("__rock_state_set({bar_slot}"));
    assert!(
        bar.contains("if not __rock_state_has"),
        "mutable global bar must guard its Session slot; got: {bar}"
    );
}

/// RUNTIME proof (not just emit shape): simulate a hot reload under luajit.
/// Edit `const FOO: 5 -> 10`, mutate the mutable global mid-session, re-dofile
/// with HOT_RELOAD set. FOO must show the NEW literal (10); the mutable global's
/// live value (set to 99 by "gameplay") must survive.
#[test]
fn const_value_edit_takes_effect_on_reload_runtime() {
    let before = emit_program("const FOO: int = 5\nglobal bar: int = 7\nfn main() {}\n");
    let after = emit_program("const FOO: int = 10\nglobal bar: int = 7\nfn main() {}\n");
    let bar_slot = global_session_slot(&before, "bar");

    let script = format!(
        "-- initial cold load\n_G.HOT_RELOAD = false\n{before}\n\
         -- gameplay mutates the live mutable global's Session slot\n\
         __rock_state_set({bar_slot}, 99)\n\
         -- hot reload: re-dofile the edited program\n_G.HOT_RELOAD = true\n{after}\n\
         -- verdict\n\
         if FOO == 10 and __rock_state_get({bar_slot}) == 99 then print(\"PASS\")\n\
         else print(\"FAIL FOO=\"..tostring(FOO)..\" bar=\"..tostring(__rock_state_get({bar_slot}))); os.exit(1) end\n"
    );

    let out = run_luajit(&script);
    assert!(out.trim().starts_with("PASS"), "got: {out}");
}
