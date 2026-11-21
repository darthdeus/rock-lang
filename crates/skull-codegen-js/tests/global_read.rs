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
