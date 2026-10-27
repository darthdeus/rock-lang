use crate::common::ws_with;
use bone_semantic::hir::BodyOwner;
use skull_mir::mir::FuncRef;

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
fn compiler_generated_mutable_global_accesses_require_the_active_session() {
    let ws = ws_with(FIXTURE);
    let mid = *ws
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("test module");
    let emitted = skull_codegen_js::emit::emit_program(&ws, mid).expect("JS emit");
    assert!(
        emitted.program.contains("__rock_state_get("),
        "mutable global reads must use the active Session"
    );
    assert!(
        emitted.program.contains("__rock_state_set("),
        "mutable global writes must use the active Session"
    );
}

fn node_available() -> bool {
    crate::common::node()
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn emit_root(src: &str, root: &str) -> String {
    let ws = ws_with(src);
    let mid = *ws
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("test module");
    let fid = ws
        .module(mid)
        .functions
        .iter()
        .find_map(|(fid, function)| (function.name.as_str() == root).then_some(fid))
        .expect("root function");
    let program = skull_codegen_js::emit::emit_program_with_roots(
        &ws,
        &[(FuncRef { module: mid, func: fid }, root.to_string())],
    )
    .expect("JS emit");
    format!(
        "{}\nvar __G = {{}};\n{}\n",
        skull_codegen_js::JS_SESSION_RUNTIME,
        program
    )
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
fn opaque_js_global_reads_and_writes_follow_the_active_session() {
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
    let grid_entry = emitted
        .state_schema
        .entries
        .iter()
        .find(|entry| entry.identity.ends_with("GRID"))
        .expect("GRID schema entry");
    let key = skull_mir::state_schema::entry_state_keys(grid_entry)[0].clone();
    let mut program = format!(
        "{}\nvar __G = {{}};\n{}\n",
        skull_codegen_js::JS_SESSION_RUNTIME,
        emitted.program
    );
    program.push_str(&format!(
        "{main}(); {main}(); let a = __rock_active_session; let schema = __rock_state_schema_id; "
    ));
    program.push_str("let b = __rock_session_new(schema); __rock_session_activate(b); __rock_state_initialize(); ");
    program.push_str(&format!("{main}(); let bValue = __rock_state_get({key:?})._data[0]; __rock_session_activate(a); console.log(bValue + ':' + __G.GRID._data[0]);"));
    assert_eq!(run_node(&program), "2:3");
}

#[test]
fn opaque_js_nobox_physical_leaves_follow_the_active_session() {
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
    let program = r#"
var __G = {};
var __ROCK_STATE_SCHEMA = [{
  kind: "global",
  name: "FLAT",
  identity: "global:<test>::FLAT",
  physical: ["FLAT_x", "FLAT_y"],
  session_keys: ["global:<test>::FLAT::leaf0", "global:<test>::FLAT::leaf1"]
}];
__rock_state_publish_schema("flat-schema");
__rock_state_install_global_bridge();
let a = __rock_active_session;
__G.FLAT_x = 3;
__G.FLAT_y = 4;
let b = __rock_session_new("flat-schema");
__rock_session_activate(b);
__G.FLAT_x = 8;
__G.FLAT_y = 9;
let bValue = __G.FLAT_x + __G.FLAT_y;
__rock_session_activate(a);
console.log(bValue + ":" + (__G.FLAT_x + __G.FLAT_y));
"#;
    assert_eq!(run_node(&format!("{}\n{}", skull_codegen_js::JS_SESSION_RUNTIME, program)), "17:7");
}

#[test]
fn lifecycle_roots_run_against_the_active_session_in_js() {
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
    let source = r#"
global COUNTER: int = 0

@lifecycle(phase="init", owner="session")
deco init tick {}

@tick
fn lifecycle_bump() {
    COUNTER = COUNTER + 1
}

fn main() {}
"#;
    let ws = ws_with(source);
    let mid = *ws
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("test module");
    let emitted = skull_codegen_js::emit::emit_program(&ws, mid).expect("JS emit");
    let main = emitted.main_js_name.expect("main root");
    let key = skull_mir::state_schema::entry_state_keys(
        emitted
            .state_schema
            .entries
            .iter()
            .find(|entry| entry.identity.ends_with("COUNTER"))
            .expect("COUNTER schema entry"),
    )[0]
    .clone();
    let mut program = format!(
        "{}\n{}\nvar __G = {{}};\n{}\n",
        skull_codegen_js::JS_CALL_HELPER,
        skull_codegen_js::JS_SESSION_RUNTIME,
        emitted.program
    );
    program.push_str(&format!(
        "{main}(); let run = __G.__ROCK_LIFECYCLE_PLAN.init._data[0].func; "
    ));
    program.push_str("__rock_call(run); let a = __rock_active_session; let schema = __rock_state_schema_id; ");
    program.push_str("let b = __rock_session_new(schema); __rock_session_activate(b); __rock_state_initialize(); __rock_call(run); ");
    program.push_str(&format!("let bValue = __rock_state_get({key:?}); __rock_session_activate(a); __rock_call(run); console.log(bValue + ':' + __rock_state_get({key:?}));"));
    assert_eq!(run_node(&program), "1:2");
}

#[test]
fn schema_changing_edit_requires_a_full_js_publication() {
    let before = ws_with(
        "global STATE: int = 0\n@nomangle fn hot() -> int { STATE }\nfn main() {}\n",
    );
    let after = ws_with(
        "global STATE: int = 0\nglobal OTHER: int = 1\n@nomangle fn hot() -> int { STATE }\nfn main() {}\n",
    );
    let mid = *before
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("test module");
    let (_, mut cache) = skull_codegen_js::emit::emit_program_cached(&before, mid).expect("full JS emit");
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
    let old_schema = cache.state_schema().schema_id.clone();
    let reload = skull_codegen_js::emit::emit_overlay(
        &after,
        mid,
        &mut cache,
        &[(after_mid, BodyOwner::Global(other))],
    )
    .expect("overlay decision");
    assert!(matches!(reload, skull_codegen_js::emit::JsReload::FullRequired));
    assert_eq!(cache.state_schema().schema_id, old_schema);
}

#[test]
fn two_js_sessions_initialize_and_isolate_mutable_globals() {
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
    let mut program = emit_root(FIXTURE, "bump");
    program.push_str(
        "let a = __rock_active_session; let schema = __rock_state_schema_id; "
    );
    program.push_str("console.log(bump()); let b = __rock_session_new(schema); ");
    program.push_str("__rock_session_activate(b); __rock_state_initialize(); console.log(bump()); ");
    program.push_str("__rock_session_activate(a); console.log(bump());\n");
    assert_eq!(run_node(&program), "1\n1\n2");
}

#[test]
fn static_value_and_done_state_are_session_local_in_js() {
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
    let source = r#"
fn bump_static() -> int {
    static value = 10
    value = value + 1
    return value
}
"#;
    let mut program = emit_root(source, "bump_static");
    program.push_str("let a = __rock_active_session; let schema = __rock_state_schema_id; ");
    program.push_str("console.log(bump_static()); let b = __rock_session_new(schema); ");
    program.push_str("__rock_session_activate(b); console.log(bump_static()); ");
    program.push_str("__rock_session_activate(a); console.log(bump_static());\n");
    assert_eq!(run_node(&program), "11\n11\n12");
}

#[test]
fn failed_state_publication_restores_active_session_and_slots_in_js() {
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
    let source = r#"
global COUNTER: int = 0

fn bump_static() -> int {
    static value = 10
    value = value + 1
    return value
}
"#;
    let mut program = emit_root(source, "bump_static");
    program.push_str(
        r#"
let globalEntry = __ROCK_STATE_SCHEMA.find(function(entry) { return entry.identity.endsWith("COUNTER"); });
let staticEntry = __ROCK_STATE_SCHEMA.find(function(entry) { return entry.kind === "static"; });
let globalKey = globalEntry.session_keys[0];
let staticValueKey = staticEntry.session_keys[0];
let staticDoneKey = staticEntry.session_keys[1];
let a = __rock_active_session;
let schema = __rock_state_schema_id;
__rock_state_set(globalKey, 7);
let first = bump_static();
let b = __rock_session_new(schema);
__rock_session_activate(b);
__rock_state_initialize();
let bFirst = bump_static();
__rock_session_activate(a);
let beforeStatic = __rock_state_get(staticValueKey);
let oldSchema = __ROCK_STATE_SCHEMA;
__rock_state_transaction_begin();
__ROCK_STATE_SCHEMA = [];
__rock_state_set(globalKey, 99);
__rock_state_set(staticValueKey, 88);
__rock_state_set(staticDoneKey, false);
__rock_session_activate(b);
let ok = true;
try { throw new Error("late publication"); } catch (e) { ok = false; }
__rock_state_transaction_abort();
console.log(ok === false && __rock_active_session === a
  && __ROCK_STATE_SCHEMA === oldSchema
  && __rock_state_get(globalKey) === 7
  && __rock_state_get(staticValueKey) === beforeStatic
  && __rock_state_has(staticDoneKey)
  && __rock_state_get(staticDoneKey) === true
  && bFirst === 11);
"#,
    );
    assert_eq!(run_node(&program), "true");
}
