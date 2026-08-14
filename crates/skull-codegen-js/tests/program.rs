//! Module-driven whole-program emission (`emit_program`, M1): the JS analog of
//! skull's Lua `assemble` -- deco def fns, fn defs, ginit fns + ordered init
//! lines, and decorator APPLICATION lines, assembled from the workspace (roots
//! = `main` + every decorated fn + every `deco` body). Unlike the per-test
//! `emit_with_closure`, a decorator-populated global read does NOT reject here
//! (the applications ARE emitted). Node execution is the correctness oracle
//! (`crates/rock-cli/tests/js_run_gate.rs`); this file pins the assembly ORDER
//! and application-line CONTENT in the emitted string.

use crate::common::{node, ws_with};

fn emit_program(ws: &bone_semantic::inference::Workspace) -> String {
    let mid = *ws.module_ids.get(&ustr::Ustr::from("<test>")).expect("test module");
    skull_codegen_js::emit::emit_program(ws, mid)
        .unwrap_or_else(|e| panic!("emit_program rejected: {}", e.message))
        .program
}

/// `<a>` appears before `<b>` in `s` (both must be present).
fn ordered(s: &str, a: &str, b: &str) {
    let ia = s.find(a).unwrap_or_else(|| panic!("missing `{a}` in:\n{s}"));
    let ib = s.find(b).unwrap_or_else(|| panic!("missing `{b}` in:\n{s}"));
    assert!(ia < ib, "expected `{a}` before `{b}`:\n{s}");
}

const PROG: &str = r#"
struct Sys { func: fn() -> (), name: string, stage: int }
global SYSTEMS: [Sys] = []
global BASE: int = 10
global DERIVED: int = BASE * 2
deco init system(stage: int = 100) {
    SYSTEMS.push(Sys { func: transmute(target.func), name: transmute(target.name), stage: stage })
}
@system(stage = 5)
fn alpha() { print("alpha") }
@system
fn beta() { print("beta") }
fn main() {
    print("derived={DERIVED}")
    for s in SYSTEMS { s.func() }
}
"#;

// Assembly order: deco def fn -> fn defs -> ginit fns -> init lines -> apps.
#[test]
fn assembly_order() {
    let ws = ws_with(PROG);
    let js = emit_program(&ws);
    ordered(&js, "function __deco_system(", "function main(");
    ordered(&js, "function main(", "function __ginit_DERIVED(");
    ordered(&js, "function __ginit_DERIVED(", "__G.DERIVED = __ginit_DERIVED();");
    // `__deco_system({` is the APPLICATION (descriptor obj); the bare
    // `__deco_system(c0` definition sorts earlier and is matched above.
    ordered(&js, "__G.DERIVED = __ginit_DERIVED();", "__deco_system({");
}

// Init lines are dependency-ordered: BASE before DERIVED (DERIVED reads BASE).
#[test]
fn init_dependency_order() {
    let ws = ws_with(PROG);
    let js = emit_program(&ws);
    ordered(&js, "__G.BASE = __ginit_BASE();", "__G.DERIVED = __ginit_DERIVED();");
}

// Application lines: the descriptor `{name, params, func}` mirror + arg
// selection (literal `stage = 5` -> `5`; defaulted `beta` -> `null`).
#[test]
fn application_line_content() {
    let ws = ws_with(PROG);
    let js = emit_program(&ws);
    // The decorated-fn JS names are the shared (name + signature) mangle.
    let alpha_app = js.lines().find(|l| l.contains("__deco_system(") && l.contains("\"alpha\""))
        .unwrap_or_else(|| panic!("no alpha application line:\n{js}"));
    assert!(alpha_app.contains("name: \"alpha\""), "descriptor name: {alpha_app}");
    assert!(alpha_app.contains("params: []"), "descriptor params: {alpha_app}");
    assert!(
        alpha_app.contains("func: __rock_code_slot_ref(\"named:<test>::alpha<>\")"),
        "decorator function values resolve through their logical CodeRef slot: {alpha_app}"
    );
    assert!(!alpha_app.contains("func: alpha_ret_unit"), "decorators must not capture a backend body: {alpha_app}");
    assert!(alpha_app.trim_end().ends_with(", 5);"), "literal stage arg: {alpha_app}");

    let beta_app = js.lines().find(|l| l.contains("__deco_system(") && l.contains("\"beta\""))
        .unwrap_or_else(|| panic!("no beta application line:\n{js}"));
    assert!(beta_app.trim_end().ends_with(", null);"), "defaulted stage -> null: {beta_app}");
}

#[test]
fn lifecycle_metadata_uses_the_shared_sorted_plan_in_js() {
    let ws = ws_with(r#"
@lifecycle(phase="update", owner="session", stage_param="stage")
deco init tick(stage: int = 100) {}
@tick(stage=7) fn first() {}
@tick(stage=7) fn second() {}
@tick fn defaulted() {}
fn main() {}
"#);
    let js = emit_program(&ws);
    assert!(!js.contains("function __deco_tick("));
    assert!(!js.contains("__deco_tick({"));
    let first = js.find("name: \"first\", stage: 7").expect("first plan entry");
    let second = js.find("name: \"second\", stage: 7").expect("second plan entry");
    let defaulted = js.find("name: \"defaulted\", stage: 100").expect("default plan entry");
    assert!(first < second && second < defaulted);
}

#[test]
fn artifact_definition_metadata_uses_the_shared_keyed_plan_in_js() {
    let ws = ws_with(r#"
@artifact_definition(catalog="widgets", key_param="key")
deco init widget(key: string) {}
@widget(key="alpha") fn make_alpha() -> int { return 1 }
@widget(key="beta") fn make_beta() -> int { return 2 }
fn main() {}
"#);
    let js = emit_program(&ws);
    assert!(!js.contains("function __deco_widget("));
    assert!(!js.contains("__deco_widget({"));
    assert!(js.contains("__rock_definition_plan_stage({"));
    let alpha = js.find("key: \"alpha\"").expect("alpha plan entry");
    let beta = js.find("key: \"beta\"").expect("beta plan entry");
    assert!(alpha < beta);
    assert!(js.contains("factory: __rock_code_slot_pending_ref(\"named:<test>::make_alpha<>\")"));
    assert!(js.contains("factory: __rock_code_slot_pending_ref(\"named:<test>::make_beta<>\")"));
    let source = format!(
        "var __G = {{}};\n{}\n{}\nconsole.log(__G.__ROCK_DEFINITION_PLAN.widgets.len + ':' + __G.__ROCK_DEFINITION_PLAN.widgets._data[0].key + ':' + __rock_call(__G.__ROCK_DEFINITION_PLAN.widgets._data[0].factory));",
        skull_codegen_js::JS_CALL_HELPER,
        js
    );
    let output = node()
        .args(["-e", &source])
        .output()
        .expect("node");
    assert!(output.status.success(), "node failed: {}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "2:alpha:1");
}

/// A concrete operation whose ONLY caller is a decorator body is still served.
///
/// A `deco` body is a third body kind beside plain fns and global initializers,
/// and it went through a hand-written subset of the shared discovery step --
/// callees and globals, but not the body's `spec_bindings`. So a specialization
/// no other body demanded was never queued while the deco body's call site still
/// spelled its specialized name, and emission rejected with `JS subset: no
/// specialized body emitted`.
///
/// `main` deliberately does NOT push, so the demand has exactly one origin.
/// The normal lane may inline the specialized call; a no-inline lane must retain
/// the standalone specialized definition and its concrete append.
#[test]
fn a_specialization_demanded_only_by_a_decorator_body_is_emitted() {
    let ws = ws_with(
        r#"
global XS: [int] = []
deco init reg() { XS.push(1) }
@reg
fn alpha() { print("alpha") }
fn main() { print("{XS.len}") }
"#,
    );
    let js = emit_program(&ws);
    let body = &js[js.find("function __deco_reg(").expect("decorator body")..];
    assert!(
        body.contains("_data[v0.len] = 1") && body.contains("v0.len = v0.len + 1"),
        "decorator lost the concrete inline push operation: {body}"
    );

    let _inline = crate::common::InlineOffGuard::on();
    let standalone = emit_program(&ws);
    assert!(
        standalone.contains("function push_"),
        "decorator-only specialization did not emit the concrete push definition:\n{standalone}"
    );
    let push = &standalone[standalone.find("function push_").expect("push definition")..];
    assert!(
        push.contains("_data[v0.len] = v1") && push.contains("v0.len = v0.len + 1"),
        "standalone decorator specialization lost the concrete push operation: {push}"
    );
}
