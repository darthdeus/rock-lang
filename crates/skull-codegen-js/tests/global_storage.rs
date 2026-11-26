use crate::common::ws_with;

#[test]
fn opaque_js_global_reads_and_writes_go_through_the_bridge() {
    let source = r#"
global GRID: [int] = [1]

inline(js) fn raw_bump() = """
    __G.GRID._data[0] = __G.GRID._data[0] + 1;
"""

fn main() {
    raw_bump()
}
"#;
    let ws = ws_with(source);
    let mid = crate::common::test_mid(&ws);
    let emitted = skull_codegen_js::emit::emit_program(&ws, mid).expect("JS emit");
    let main = emitted.main_js_name.expect("main root");
    let mut program = format!(
        "{}\nvar __G = {{}};\n{}\n",
        skull_codegen_js::JS_GLOBALS_RUNTIME,
        emitted.program
    );
    program.push_str(&format!(
        "{main}(); {main}(); let key = __rock_global_entry(\"GRID\").slots[0]; \
         console.log(__rock_globals[key]._data[0] + ':' + __G.GRID._data[0]);"
    ));
    assert_eq!(crate::common::run_node(&program).trim(), "3:3");
}

#[test]
fn opaque_js_nobox_physical_leaves_route_through_the_bridge() {
    let program = r#"
var __G = {};
__rock_globals_open("flat-layout");
__rock_globals_publish([{
  name: "FLAT",
  identity: "global:<test>::FLAT",
  physical: ["FLAT_x", "FLAT_y"],
  slots: ["global:<test>::FLAT::leaf0", "global:<test>::FLAT::leaf1"]
}]);
__G.FLAT_x = 3;
__G.FLAT_y = 4;
console.log(__rock_globals["global:<test>::FLAT::leaf0"] + ":" + (__G.FLAT_x + __G.FLAT_y));
"#;
    assert_eq!(
        crate::common::run_node(&format!(
            "{}\n{}",
            skull_codegen_js::JS_GLOBALS_RUNTIME,
            program
        ))
        .trim(),
        "3:7"
    );
}

#[test]
fn both_backends_diagnose_executed_initializer_cycles() {
    let ws = ws_with(
        r#"
global first: int = make()
global second: int = first + 1

@nomangle fn invoke(read: fn() -> int) -> int { return read() }
fn make() -> int { return invoke(() => second) }

fn main() { print(first) }
"#,
    );
    let mid = crate::common::test_mid(&ws);
    let lua = skull::mir_emit::emit_workspace_mir(&ws, mid)
        .err()
        .expect("Lua cycle");
    let js = skull_codegen_js::emit::emit_program(&ws, mid)
        .err()
        .expect("JS cycle");
    for diagnostic in [lua, js] {
        assert!(
            diagnostic
                .message
                .contains("global initializers are mutually recursive: first, second"),
            "{}",
            diagnostic.message
        );
    }
}
