use crate::common::ws_with;
use bone_core::hir::BodyOwner;

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
fn layout_changing_edit_requires_a_full_js_publication() {
    let before =
        ws_with("global STATE: int = 0\n@nomangle fn hot() -> int { STATE }\nfn main() {}\n");
    let after = ws_with(
        "global STATE: int = 0\nglobal OTHER: int = 1\n@nomangle fn hot() -> int { STATE }\nfn main() {}\n",
    );
    let mid = crate::common::test_mid(&before);
    let (initial, mut cache) =
        skull_codegen_js::emit::emit_program_cached(&before, mid).expect("full JS emit");
    let after_mid = crate::common::test_mid(&after);
    let fresh = skull_codegen_js::emit::emit_program(&after, after_mid).expect("fresh JS emit");
    assert_ne!(fresh.global_layout, initial.global_layout);
    let other = after
        .module(after_mid)
        .globals
        .iter()
        .find(|(_, global)| skull_mir::mangle::declared_name(global.name.as_str()) == "OTHER")
        .map(|(gid, _)| gid)
        .expect("OTHER global");
    let reload = skull_codegen_js::emit::emit_overlay(
        &after,
        mid,
        &mut cache,
        &[(after_mid, BodyOwner::Global(other))],
    )
    .expect("overlay decision");
    assert!(matches!(
        reload,
        skull_codegen_js::emit::JsReload::FullRequired
    ));
    let retry = skull_codegen_js::emit::emit_overlay(
        &after,
        mid,
        &mut cache,
        &[(after_mid, BodyOwner::Global(other))],
    )
    .expect("repeated overlay decision");
    assert!(matches!(
        retry,
        skull_codegen_js::emit::JsReload::FullRequired
    ));
}
