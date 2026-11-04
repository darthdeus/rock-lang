use crate::common::ws_with;
use bone_semantic::hir::BodyOwner;

const FIXTURE: &str = r#"
global COUNTER: int = 0

fn bump() -> int {
    COUNTER = COUNTER + 1
    return COUNTER
}

fn main() {
    bump()
}
"#;

#[test]
fn mutable_global_accesses_use_the_global_storage() {
    let ws = ws_with(FIXTURE);
    let mid = *ws
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("test module");
    let emitted = skull_codegen_js::emit::emit_program(&ws, mid).expect("JS emit");
    let slot = "__rock_globals[\"global:<test>::COUNTER::value\"]";
    assert!(
        emitted.program.contains(&format!("{slot} = ")),
        "mutable global writes must use the global storage"
    );
    assert!(
        emitted.program.matches(slot).count()
            > emitted.program.matches(&format!("{slot} = ")).count(),
        "mutable global reads must use the global storage"
    );
}

fn node_available() -> bool {
    crate::common::node()
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn run_node(program: &str) -> String {
    let output = crate::common::node()
        .arg("-e")
        .arg(program)
        .output()
        .expect("spawn node");
    assert!(
        output.status.success(),
        "node failed:\n{program}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[test]
fn opaque_js_global_reads_and_writes_go_through_the_bridge() {
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
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
    let mid = *ws
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("test module");
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
    assert_eq!(run_node(&program), "3:3");
}

#[test]
fn opaque_js_nobox_physical_leaves_route_through_the_bridge() {
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
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
        run_node(&format!(
            "{}\n{}",
            skull_codegen_js::JS_GLOBALS_RUNTIME,
            program
        )),
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
    let mid = *before
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("test module");
    let (_, mut cache) =
        skull_codegen_js::emit::emit_program_cached(&before, mid).expect("full JS emit");
    let after_mid = *after
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("test module");
    let other = after
        .module(after_mid)
        .globals
        .iter()
        .find(|(_, global)| global.name.as_str() == "OTHER")
        .map(|(gid, _)| gid)
        .expect("OTHER global");
    let old_layout = cache.global_layout().to_string();
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
    assert_eq!(cache.global_layout(), old_layout);
}
