use crate::common::{run_node, test_mid, ws_with};

const SOURCE: &str = r#"global LATE: int
global MAYBE: int?
@nomangle fn read() -> int { LATE }
@nomangle fn assign() { LATE = 42 }
@nomangle fn optional() -> int? { MAYBE }
@nomangle fn callback() -> fn() -> int { () => LATE }
fn main() { read(); assign(); optional(); callback() }
"#;

fn programs(source: &str) -> (String, String) {
    let ws = ws_with(source);
    emit_programs(&ws, test_mid(&ws))
}

fn emit_programs(
    ws: &bone_core::workspace::Workspace,
    mid: bone_core::prelude::ModuleId,
) -> (String, String) {
    assert!(skull_mir::verify::checks_enabled());
    let lua = skull::mir_emit::emit_workspace_mir(ws, mid)
        .expect("Lua emit")
        .lua;
    let js = skull_codegen_js::emit::emit_program(ws, mid)
        .expect("JS emit")
        .program;
    (
        lua,
        format!(
            "{}\n{}\nvar __G = {{}};\n{js}",
            skull_codegen_js::JS_GLOBALS_RUNTIME,
            skull_codegen_js::JS_CALL_HELPER
        ),
    )
}

#[test]
fn uninitialized_global_reads_fail_on_both_backends() {
    let (lua, js) = programs(SOURCE);
    let lua_result = crate::common::run_file(
        "luajit",
        &format!("{lua}\nlocal ok, err = pcall(read); print(ok); print(err)"),
    );
    let js_result = run_node(&format!(
        "{js}\ntry {{ read(); console.log('true'); }} catch (e) {{ console.log('false'); console.log(e.message); }}"
    ));
    for result in [lua_result, js_result] {
        assert!(
            result.starts_with("false\n"),
            "early read succeeded: {result}"
        );
        assert!(result.contains("uninitialized global `LATE`"), "{result}");
        assert!(result.contains("<test>:3:30"), "{result}");
    }
}

#[test]
fn assigned_and_optional_globals_survive_reload_on_both_backends() {
    let (lua, js) = programs(SOURCE);
    assert_eq!(
        crate::common::run_file(
            "luajit",
            &format!(
                "{lua}\nassert(optional() == nil); assign(); assert(read() == 42)\n{lua}\nassert(read() == 42); print('ok')"
            )
        ),
        "ok"
    );
    let reload = js.replacen(skull_codegen_js::JS_GLOBALS_RUNTIME, "", 1);
    assert_eq!(run_node(&format!(
        "{js}\nif (optional() != null) throw Error('optional'); assign(); if (read() !== 42) throw Error('assigned');\n{reload}\nif (read() !== 42) throw Error('reload'); console.log('ok');"
    )).trim(), "ok");
}

#[test]
fn lifecycle_callback_read_checks_when_called_on_both_backends() {
    let (lua, js) = programs(SOURCE);
    let lua_result = crate::common::run_file(
        "luajit",
        &format!(
            "{lua}\nlocal cb = callback(); local ok, err = pcall(cb); assert(not ok); print(err)"
        ),
    );
    let js_result = run_node(&format!(
        "{js}\nlet cb = callback(); try {{ __rock_call(cb); console.log('missed'); }} catch (e) {{ console.log(e.message); }}"
    ));
    for result in [lua_result, js_result] {
        assert!(result.contains("uninitialized global `LATE`"), "{result}");
        assert!(result.contains("<test>:6:"), "{result}");
    }
}

#[test]
fn cross_module_global_read_reports_the_consumer_location() {
    let ws = crate::common::ws_of_modules(vec![
        crate::common::package_module(
            "app",
            "<test>",
            "use state::LATE;\n@nomangle fn read() -> int { LATE }\nfn main() { read() }\n",
        ),
        crate::common::package_module("app", "state.rock", "mod state;\npub global LATE: int\n"),
    ]);
    let (lua, js) = emit_programs(&ws, test_mid(&ws));
    let lua_result = crate::common::run_file(
        "luajit",
        &format!("{lua}\nlocal ok, err = pcall(read); assert(not ok); print(err)"),
    );
    let js_result = run_node(&format!(
        "{js}\ntry {{ read(); console.log('missed'); }} catch (e) {{ console.log(e.message); }}"
    ));
    for result in [lua_result, js_result] {
        assert!(result.contains("uninitialized global `LATE`"), "{result}");
        assert!(result.contains("<test>:2:30"), "{result}");
    }
}

#[test]
fn a_same_block_assignment_elides_the_read_check() {
    let (lua, js) = programs("global LATE: int\nfn main() { LATE = 1; let unused = LATE }\n");
    for program in [lua, js] {
        assert!(
            !program.contains("uninitialized global `LATE`"),
            "{program}"
        );
    }
}

#[test]
fn flattened_global_initialization_survives_compatible_reload() {
    let (lua, js) = programs(
        "nobox value Pair { a: int?, b: bool? }\n\
         global LATE: Pair\n\
         @nomangle fn assign() { LATE = Pair { a: null, b: null } }\n\
         @nomangle fn read() -> bool { LATE.a == null && LATE.b == null }\n\
         fn main() { assign(); read() }\n",
    );
    assert_eq!(
        crate::common::run_file(
            "luajit",
            &format!("{lua}\nassign(); assert(read());\n{lua}\nassert(read()); print('ok')")
        ),
        "ok"
    );
    let reload = js.replacen(skull_codegen_js::JS_GLOBALS_RUNTIME, "", 1);
    assert_eq!(run_node(&format!(
        "{js}\nassign(); if (!read()) throw Error('assigned');\n{reload}\nif (!read()) throw Error('reload'); console.log('ok');"
    )).trim(), "ok");
}

#[test]
fn a_checked_global_read_without_stdlib_uses_the_js_runtime_error() {
    let source = "global LATE: int\n@nomangle fn read() -> int { LATE }\nfn main() { read() }\n";
    let mut modules =
        vec![skull_parser::parse_module_clean(source, ustr::Ustr::from("<test>")).unwrap()];
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).unwrap();
    let js = skull_codegen_js::emit::emit_program(&ws, test_mid(&ws))
        .unwrap()
        .program;
    let result = run_node(&format!(
        "{}\n{}\n{}\nvar __G = {{}};\n{js}\ntry {{ read(); console.log('missed'); }} catch (e) {{ console.log(e.message); }}",
        skull_codegen_js::JS_GLOBALS_RUNTIME,
        skull_codegen_js::JS_CALL_HELPER,
        skull_codegen_js::JS_CORO_RUNTIME,
    ));
    assert!(result.contains("uninitialized global `LATE`"), "{result}");
    assert!(result.contains("<test>:2:30"), "{result}");
}
