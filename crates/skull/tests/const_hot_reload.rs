//! `const` globals re-initialize on hot reload, while mutable globals keep their
//! live value in the global storage.
use crate::common::{run_luajit, ws_of};

/// Compile `src` to a full skull Lua program (no stdlib -- int/empty-main only).
fn emit_program(src: &str) -> String {
    let ws = ws_of(src);
    let mid = skull::user_module_id(&ws).unwrap();
    skull::mir_emit::emit_workspace_mir(&ws, mid)
        .expect("emit")
        .lua
}

fn global_slot(lua: &str, name: &str) -> u32 {
    crate::common::global_slots(lua, &format!("global:<t>::{name}"))[0]
}

#[test]
fn const_reinits_on_hot_reload_but_session_global_is_preserved() {
    // A computed initializer: a literal one emits no global at all.
    let ws = ws_of(
        "inline(lua) fn five() -> int = \"return 5\"\n\
         const FOO: int = five()\nglobal bar: int = 7\nfn main() {}\n",
    );
    let mid = skull::user_module_id(&ws).unwrap();
    let lua = skull::mir_emit::emit_full(&ws, mid)
        .expect("emit_full")
        .0
        .lua;

    let line = |needle: &str| {
        lua.lines()
            .find(|l| l.contains(needle) && !l.starts_with("function"))
            .unwrap_or_else(|| panic!("no init line for {needle} in:\n{lua}"))
            .to_string()
    };

    // const: no preserve guard -> re-runs init every reload.
    let foo = line("FOO =");
    assert!(
        !foo.contains("HOT_RELOAD"),
        "const FOO must re-init unconditionally on reload (no HOT_RELOAD guard); got: {foo}"
    );
    // mutable global: the fresh-storage guard preserves the live value.
    let bar_slot = global_slot(&lua, "bar");
    let bar = line(&format!("[{bar_slot}] = "));
    assert!(
        bar.contains("if __rock_globals_fresh then"),
        "mutable global bar must guard its initializer; got: {bar}"
    );
}

/// RUNTIME proof (not just emit shape): simulate a hot reload under luajit.
/// Edit `const FOO: 5 -> 10`, mutate the mutable global mid-session, re-dofile
/// with HOT_RELOAD set. FOO must show the NEW literal (10); the mutable global's
/// live value (set to 99 by "gameplay") must survive.
#[test]
fn const_value_edit_takes_effect_on_reload_runtime() {
    let program = |foo: i32| {
        emit_program(&format!(
            "const FOO: int = {foo}\nglobal bar: int = 7\nfn foo() -> int {{ FOO }}\nfn main() {{}}\n"
        ))
    };
    let before = program(5);
    let after = program(10);
    let bar_slot = global_slot(&before, "bar");
    let foo = crate::common::mangled_name(&after, "foo");

    let script = format!(
        "-- initial cold load\n_G.HOT_RELOAD = false\n{before}\n\
         -- gameplay mutates the live mutable global\n\
         __rock_globals[{bar_slot}] = 99\n\
         -- hot reload: re-dofile the edited program\n_G.HOT_RELOAD = true\n{after}\n\
         -- verdict\n\
         if {foo}() == 10 and __rock_globals[{bar_slot}] == 99 then print(\"PASS\")\n\
         else print(\"FAIL FOO=\"..tostring({foo}())..\" bar=\"..tostring(__rock_globals[{bar_slot}])); os.exit(1) end\n"
    );

    let out = run_luajit(&script);
    assert!(out.trim().starts_with("PASS"), "got: {out}");
}

/// Every read of a scalar `const` is emitted as its literal, so nothing names
/// the global: it gets neither an initializer function nor an assignment.
#[test]
fn a_const_whose_reads_are_all_literals_emits_no_global() {
    let lua = emit_program("const FOO: int = 5\nfn foo() -> int { FOO }\nfn main() {}\n");
    assert!(!lua.contains("__skull_ginit_FOO"), "{lua}");
    assert!(!lua.contains("FOO ="), "{lua}");
    let foo = crate::common::mangled_name(&lua, "foo");
    let out = run_luajit(&format!("{lua}\nprint({foo}())\n"));
    assert_eq!(out.trim(), "5");
}

/// A const initializer runs at load even when nothing reads the value, so one
/// that is not a literal keeps its initializer.
#[test]
fn a_const_with_a_computed_initializer_keeps_it() {
    let lua = emit_program(
        "inline(lua) fn setup() -> int = \"SETUP_RAN = true return 1\"\n\
         const READY: int = setup()\nfn main() {}\n",
    );
    assert!(lua.contains("READY = __skull_ginit_READY()"), "{lua}");
    let out = run_luajit(&format!("{lua}\nprint(SETUP_RAN)\n"));
    assert_eq!(out.trim(), "true");
}

/// Inline Lua reaches a global by spelling its emitted name, which the
/// compiler does not see as a read, so a const it spells stays a global.
#[test]
fn a_const_spelled_by_inline_lua_stays_a_global() {
    let lua = emit_program(
        "const FOO: int = 5\ninline(lua) fn foo() -> int = \"return FOO\"\n\
         fn bar() -> int { foo() }\nfn main() {}\n",
    );
    let bar = crate::common::mangled_name(&lua, "bar");
    let out = run_luajit(&format!("{lua}\nprint({bar}())\n"));
    assert_eq!(out.trim(), "5");
}
