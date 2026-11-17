use crate::common::{emit_without_stdlib, run_luajit};

fn global_slot(lua: &str, name: &str) -> u32 {
    crate::common::global_slots(lua, &format!("global:<t>::{name}"))[0]
}

#[test]
fn const_value_edit_takes_effect_on_reload_runtime() {
    let program = |foo: i32| {
        emit_without_stdlib(&format!(
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
    assert!(out.trim().starts_with("PASS"));
}

#[test]
fn a_const_whose_reads_are_all_literals_emits_no_global() {
    let lua = emit_without_stdlib("const FOO: int = 5\nfn foo() -> int { FOO }\nfn main() {}\n");
    assert!(!lua.contains("__skull_ginit_FOO"));
    assert!(!lua.contains("FOO ="));
    let foo = crate::common::mangled_name(&lua, "foo");
    let out = run_luajit(&format!("{lua}\nprint({foo}())\n"));
    assert_eq!(out.trim(), "5");
}

#[test]
fn a_const_with_a_computed_initializer_keeps_it() {
    let lua = emit_without_stdlib(
        "inline(lua) fn setup() -> int = \"SETUP_RAN = true return 1\"\n\
         const READY: int = setup()\nfn main() {}\n",
    );
    assert!(lua.contains("READY = __skull_ginit_READY()"));
    let out = run_luajit(&format!("{lua}\nprint(SETUP_RAN)\n"));
    assert_eq!(out.trim(), "true");
}

#[test]
fn a_const_spelled_by_inline_lua_stays_a_global() {
    let lua = emit_without_stdlib(
        "const FOO: int = 5\ninline(lua) fn foo() -> int = \"return FOO\"\n\
         fn bar() -> int { foo() }\nfn main() {}\n",
    );
    let bar = crate::common::mangled_name(&lua, "bar");
    let out = run_luajit(&format!("{lua}\nprint({bar}())\n"));
    assert_eq!(out.trim(), "5");
}
