//! Module-driven whole-program emission (`emit_program`): the JS analog of
//! skull's Lua `assemble` -- deco def fns, fn defs, ginit fns + ordered init
//! lines, and decorator APPLICATION lines, assembled from the workspace (roots
//! = `main` + every decorated fn + every `deco` body). Unlike the per-test
//! `emit_with_closure`, a decorator-populated global read does NOT reject here
//! (the applications ARE emitted). Node execution is the correctness oracle
//! (`host.rs` runs whole programs); this file pins the assembly ORDER
//! and application-line CONTENT in the emitted string.

use crate::common::{node, ws_with, ws_with_named};

fn emit_program(ws: &bone_core::workspace::Workspace) -> String {
    let mid = crate::common::test_mid(ws);
    skull_codegen_js::emit::emit_program(ws, mid)
        .unwrap_or_else(|e| panic!("emit_program rejected: {}", e.message))
        .program
}

#[test]
fn nested_native_layout_values_keep_javascript_field_access() {
    let ws = ws_with(
        r#"
nobox value Cell { x: int, y: int }
nobox value Location { region: int, cell: Cell }
value Motion { active: bool, at: Location, weight: float }
struct Holder { motion: Motion }
fn read(owner: Holder) -> int {
    return owner.motion.at.region + owner.motion.at.cell.x + owner.motion.at.cell.y
}
fn main() -> int {
    let owner = Holder { motion: Motion { active: true,
        at: Location { region: 7, cell: Cell { x: 3, y: 5 } }, weight: 2.5 } }
    assert(read(owner) == 15)
    let prior = owner.motion.at
    owner.motion.at.cell.x = 11
    assert(read(owner) == 23)
    assert(prior.cell.x == 3 && prior.cell.y == 5)
    return read(owner)
}
"#,
    );
    let js = emit_program(&ws);
    let source = format!(
        "var __G = {{}};\n{}\n{}\n{js}\nconsole.log(main());",
        skull_codegen_js::JS_GLOBALS_RUNTIME,
        skull_codegen_js::JS_CALL_HELPER
    );
    let output = node()
        .arg("-e")
        .arg(source)
        .output()
        .expect("run nested value JavaScript");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "23");
}

/// `<a>` appears before `<b>` in `s` (both must be present).
fn ordered(s: &str, a: &str, b: &str) {
    let ia = s
        .find(a)
        .unwrap_or_else(|| panic!("missing `{a}` in:\n{s}"));
    let ib = s
        .find(b)
        .unwrap_or_else(|| panic!("missing `{b}` in:\n{s}"));
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
    ordered(
        &js,
        "function __ginit_DERIVED(",
        "__rock_globals[\"global:<test>::DERIVED::value\"] = ",
    );
    // `__deco_system({` is the APPLICATION (descriptor obj); the bare
    // `__deco_system(c0` definition sorts earlier and is matched above.
    ordered(
        &js,
        "__rock_globals[\"global:<test>::DERIVED::value\"] = ",
        "__deco_system({",
    );
}

// Init lines are dependency-ordered: BASE before DERIVED (DERIVED reads BASE).
#[test]
fn init_dependency_order() {
    let ws = ws_with(PROG);
    let js = emit_program(&ws);
    ordered(
        &js,
        "__rock_globals[\"global:<test>::BASE::value\"] = ",
        "__rock_globals[\"global:<test>::DERIVED::value\"] = ",
    );
}

// Application lines: the descriptor `{name, params, func}` mirror + arg
// selection (literal `stage = 5` -> `5`; defaulted `beta` -> `null`).
#[test]
fn application_line_content() {
    let ws = ws_with(PROG);
    let js = emit_program(&ws);
    // The decorated-fn JS names are the shared (name + signature) mangle.
    let alpha_app = js
        .lines()
        .find(|l| l.contains("__deco_system(") && l.contains("\"alpha\""))
        .unwrap_or_else(|| panic!("no alpha application line:\n{js}"));
    assert!(
        alpha_app.contains("name: \"alpha\""),
        "descriptor name: {alpha_app}"
    );
    assert!(
        alpha_app.contains("params: []"),
        "descriptor params: {alpha_app}"
    );
    assert!(
        alpha_app.contains("func: __rock_code_slot_ref(\"named:<test>::alpha<>\")"),
        "decorator function values resolve through their logical CodeRef slot: {alpha_app}"
    );
    assert!(
        !alpha_app.contains("func: alpha"),
        "decorators must not capture a backend body: {alpha_app}"
    );
    assert!(
        alpha_app.trim_end().ends_with(", 5);"),
        "literal stage arg: {alpha_app}"
    );

    let beta_app = js
        .lines()
        .find(|l| l.contains("__deco_system(") && l.contains("\"beta\""))
        .unwrap_or_else(|| panic!("no beta application line:\n{js}"));
    assert!(
        beta_app.trim_end().ends_with(", null);"),
        "defaulted stage -> null: {beta_app}"
    );
}

#[test]
fn lifecycle_metadata_uses_the_shared_sorted_plan_in_js() {
    let ws = ws_with(
        r#"
@lifecycle(phase="update", owner="process", stage_param="stage")
deco init tick(stage: int = 100) {}
@tick(stage=7) fn first() {}
@tick(stage=7) fn second() {}
@tick fn defaulted() {}
fn main() {}
"#,
    );
    let js = emit_program(&ws);
    assert!(!js.contains("function __deco_tick("));
    assert!(!js.contains("__deco_tick({"));
    let first = js
        .find("name: \"first\", stage: 7")
        .expect("first plan entry");
    let second = js
        .find("name: \"second\", stage: 7")
        .expect("second plan entry");
    let defaulted = js
        .find("name: \"defaulted\", stage: 100")
        .expect("default plan entry");
    assert!(first < second && second < defaulted);
}

#[test]
fn emitted_js_has_no_shadow_effect_plan() {
    let js = emit_program(&ws_with("fn main() {}"));
    assert!(
        !js.contains("ShadowEffectPlan"),
        "JS must not carry the removed effect-plan type"
    );
    assert!(
        !js.contains("__ROCK_SHADOW_EFFECT_PLAN"),
        "JS must not publish the removed effect plan"
    );
}

#[test]
fn lua_and_js_guard_frame_hooks_owned_by_a_sim_fixture_module() {
    let file = "/repo/game/tests/sims/scoped.rock";
    let ws = ws_with_named(
        file,
        r#"
        @lifecycle(phase="update", owner="process") deco init update {}
        fn register_sim(name: string, body: () -> ()) {}
        @update fn observed_tick() {}
        @sim("wire-name") fn selected_sim() {}
        fn main() {
            let keep: () -> () = selected_sim
            keep()
        }
        "#,
    );
    let mid = *ws
        .module_ids
        .get(&ustr::Ustr::from(file))
        .expect("fixture module");
    let lua = skull::mir_emit::emit_full(&ws, mid).expect("Lua emit").0;
    let js = skull_codegen_js::emit::emit_program(&ws, mid).expect("JS emit");
    assert_eq!(
        lua.lifecycle_plan, js.lifecycle_plan,
        "Lua and JS sim scopes drifted"
    );
    assert!(
        lua.lua.contains("_G.__ROCK_ACTIVE_SIM == \"wire-name\""),
        "Lua lifecycle thunk is not scoped to the fixture sim: {}",
        lua.lua
    );
    assert!(
        js.program
            .contains("__G.__ROCK_ACTIVE_SIM === \"wire-name\""),
        "JS lifecycle thunk is not scoped to the fixture sim: {}",
        js.program
    );
}

#[test]
fn scene_lifecycle_metadata_uses_the_same_canonical_root_key_in_js() {
    let ws = ws_with(
        r#"
struct Dungeon { ticks: int }
@lifecycle(phase="update", owner="scene") deco init scene_update {}
@scene_update fn tick(root: Dungeon) { root.ticks += 1 }
fn main() {}
"#,
    );
    let js = emit_program(&ws);
    assert!(
        js.contains("owner: \"scene\", root_type: \"a_n7_Dungeon_0_z\""),
        "scene plan lost its canonical typed root key: {js}",
    );
    assert!(
        js.contains("scene_owned: 1"),
        "scene phase population is zero"
    );
}

#[test]
fn scene_creation_uses_the_monomorphized_type_key_without_runtime_discovery_in_js() {
    let ws = ws_with(
        r#"
struct Room { ticks: int }
@lifecycle(phase="update", owner="scene") deco init scene_update {}
@scene_update fn tick(root: Room) { root.ticks += 1 }
global ROOT: Scene<Room>? = null
fn load() -> Scene<Room> {
    return scene_create((_b) => {
        let root: Room? = Room { ticks: 0 }
        return root
    }).unwrap()
}
fn main() { ROOT = load() }
"#,
    );
    let js = emit_program(&ws);
    let scene_create_body = js
        .split("\nfunction ")
        .find(|part| part.starts_with("scene_create"))
        .unwrap_or_else(|| panic!("no emitted scene_create specialization:\n{js}"));
    assert!(
        scene_create_body.contains("a_n4_Room_0_z"),
        "scene_create must receive the compiler's MonoTy-derived key: {scene_create_body}"
    );
    assert!(
        !scene_create_body.contains("__rock_get_type"),
        "Scene construction must not discover its root type from the published value: {scene_create_body}"
    );
}

#[test]
fn lua_and_js_publish_identical_dependency_gated_lifecycle_plans() {
    let ws = ws_with(
        r#"
        @lifecycle(phase="wire", owner="process") deco init wire {}
        const BASE: int = 1
        const DERIVED: int = BASE + 1
        const OTHER: int = 9
        @wire fn plain_wire() {}
        @wire(onchange=[DERIVED, OTHER]) fn gated_wire() {}
        fn main() {}
    "#,
    );
    let mid = crate::common::test_mid(&ws);
    let lua_plan = skull::mir_emit::emit_full(&ws, mid)
        .expect("Lua emit")
        .0
        .lifecycle_plan;
    let js_plan = skull_codegen_js::emit::emit_program(&ws, mid)
        .expect("JS emit")
        .lifecycle_plan;
    assert_eq!(lua_plan, js_plan, "Lua and JS lifecycle metadata drifted");
    assert!(
        lua_plan.census.plain > 0,
        "plain population must be nonzero"
    );
    assert!(
        lua_plan.census.gated > 0,
        "gated population must be nonzero"
    );
    assert!(
        lua_plan.census.dependency_edges > 2,
        "transitive dependency closure must be populated"
    );
    let gated = lua_plan
        .entries
        .iter()
        .find(|entry| entry.diagnostic_name.as_str() == "gated_wire")
        .expect("gated entry");
    let dependency_names = gated
        .qualification
        .dependencies()
        .iter()
        .map(|dependency| match &dependency.identity {
            skull_mir::code_identity::CodeIdentity::Artifact(
                skull_mir::code_identity::CodeOwner::Artifact { name, .. },
            ) => name.as_str(),
            identity => panic!("dependency is not a logical global identity: {identity:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(dependency_names, vec!["DERIVED", "OTHER", "BASE"]);
}

#[test]
fn lua_and_js_reject_the_same_unsupported_wire_dependencies() {
    let cases = [
        ("[MUTABLE]", "mutable"),
        ("[callable]", "names a function"),
        ("[UNKNOWN]", "is unknown"),
        ("[]", "requires a non-empty dependency list"),
        ("[GOOD.value]", "structured identifier list"),
    ];
    for (annotation, expected) in cases {
        let source = format!(
            r#"
            @lifecycle(phase="wire", owner="process") deco init wire {{}}
            const GOOD: int = 1
            global MUTABLE: int = 2
            fn callable() {{}}
            @wire(onchange={annotation}) fn apply_game_config() {{}}
            fn main() {{}}
            "#
        );
        let ws = ws_with(&source);
        let mid = crate::common::test_mid(&ws);
        let lua_error = match skull::mir_emit::emit_full(&ws, mid) {
            Ok(_) => panic!("Lua accepted unsupported dependency {annotation}"),
            Err(error) => error.message,
        };
        let js_error = match skull_codegen_js::emit::emit_program(&ws, mid) {
            Ok(_) => panic!("JS accepted unsupported dependency {annotation}"),
            Err(error) => error.message,
        };
        assert!(
            lua_error.contains(expected),
            "Lua {annotation}: {lua_error}"
        );
        assert!(js_error.contains(expected), "JS {annotation}: {js_error}");
    }
}

#[test]
fn artifact_definition_metadata_uses_the_shared_keyed_plan_in_js() {
    let ws = ws_with(
        r#"
@artifact_definition(catalog="widgets", key_param="key")
deco init widget(key: string) {}
@widget(key="alpha") fn make_alpha() -> int { return 1 }
@widget(key="beta") fn make_beta() -> int { return 2 }
fn main() {}
"#,
    );
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
        "var __G = {{}};\n{}\n{}\n{}\nconsole.log(__G.__ROCK_DEFINITION_PLAN.widgets.len + ':' + __G.__ROCK_DEFINITION_PLAN.widgets._data[0].key + ':' + __rock_call(__G.__ROCK_DEFINITION_PLAN.widgets._data[0].factory));",
        skull_codegen_js::JS_GLOBALS_RUNTIME,
        skull_codegen_js::JS_CALL_HELPER,
        js
    );
    let output = node().args(["-e", &source]).output().expect("node");
    assert!(
        output.status.success(),
        "node failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "2:alpha:1");
}

#[test]
fn typed_catalog_accessor_is_compiler_owned_and_returns_the_published_view_in_js() {
    let ws = ws_with(
        r#"
struct WidgetDef { value: int }
@artifact_definition(catalog="widgets", key_param="key")
deco init widget(key: string) {}
@artifact_definition(catalog="empty", key_param="key")
deco init empty(key: string) {}
@artifact_catalog(catalog="widgets")
@intrinsic extern(lua) fn widget_definition_plan() -> [ArtifactDefinition<WidgetDef>]
@artifact_catalog(catalog="empty")
@intrinsic extern(lua) fn empty_definition_plan() -> [ArtifactDefinition<WidgetDef>]
@widget(key="alpha") fn make_alpha() -> WidgetDef { return WidgetDef { value: 1 } }
fn main() {}
"#,
    );
    let js = emit_program(&ws);
    assert!(js.contains(
        "function widget_definition_plan() { return __rock_definition_catalog(\"widgets\"); }"
    ));
    assert!(!js.contains("__ROCK_DEFINITION_PLAN || {}"));
    let source = format!(
        "var __G = {{}};\n{}\n{}\n{}\nconst p = widget_definition_plan(); const e = empty_definition_plan(); let missing = false; try {{ __rock_definition_catalog('missing'); }} catch (_) {{ missing = true; }} console.log(p.len + ':' + p._data[0].key + ':' + p._data[0].definition.value + ':' + e.len + ':' + missing);",
        skull_codegen_js::JS_GLOBALS_RUNTIME,
        skull_codegen_js::JS_CALL_HELPER,
        js,
    );
    let output = node().args(["-e", &source]).output().expect("node");
    assert!(
        output.status.success(),
        "node failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "1:alpha:1:0:true"
    );
}

#[test]
fn typed_catalog_lua_and_js_emission_share_membership_order_and_type() {
    let ws = ws_with(
        r#"
struct WidgetDef { value: int }
@artifact_definition(catalog="widgets", key_param="key")
deco init widget(key: string) {}
@artifact_definition(catalog="empty", key_param="key")
deco init empty(key: string) {}
@artifact_catalog(catalog="widgets")
@intrinsic extern(lua) fn widget_definition_plan() -> [ArtifactDefinition<WidgetDef>]
@artifact_catalog(catalog="empty")
@intrinsic extern(lua) fn empty_definition_plan() -> [ArtifactDefinition<WidgetDef>]
@widget(key="beta") fn make_beta() -> WidgetDef { return WidgetDef { value: 2 } }
@widget(key="alpha") fn make_alpha() -> WidgetDef { return WidgetDef { value: 1 } }
fn main() {}
"#,
    );
    let plan = skull_mir::definition::collect(&ws).expect("typed parity plan");
    assert_eq!(plan.catalogs.len(), 2);
    assert_eq!(plan.catalogs[0].catalog.as_str(), "empty");
    assert_eq!(
        plan.catalogs[0]
            .definition_type
            .as_ref()
            .map(|t| t.render()),
        Some("WidgetDef".to_string())
    );
    assert_eq!(
        plan.entries
            .iter()
            .map(|e| e.key.as_str())
            .collect::<Vec<_>>(),
        ["beta", "alpha"]
    );

    let mid = crate::common::test_mid(&ws);
    let lua = skull::mir_emit::emit_full(&ws, mid)
        .expect("Lua parity emission")
        .0
        .lua;
    let js = emit_program(&ws);
    for output in [&lua, &js] {
        assert!(output.contains("key = \"beta\"") || output.contains("key: \"beta\""));
        assert!(output.contains("key = \"alpha\"") || output.contains("key: \"alpha\""));
        assert!(output.contains("len = 0") || output.contains("len: 0"));
    }
    assert!(lua.find("key = \"beta\"") < lua.find("key = \"alpha\""));
    assert!(js.find("key: \"beta\"") < js.find("key: \"alpha\""));
    assert!(lua.contains("function widget_definition_plan()"));
    assert!(js.contains("function widget_definition_plan()"));
}

#[test]
fn typed_definition_catalog_lookup_is_fail_closed_and_counts_population_in_js() {
    let source = format!(
        "var __G = {{}};\n{}\n{}\n{}\nconsole.log(result);",
        skull_codegen_js::JS_GLOBALS_RUNTIME,
        skull_codegen_js::JS_CALL_HELPER,
        r#"
let missingPlan = false;
try { __rock_definition_catalog("widgets"); } catch (_) { missingPlan = true; }
__rock_code_slot_begin();
__rock_definition_plan_stage({
  widgets: { len: 1, _data: [{ key: "alpha", factory: function() { return { value: 1 }; } }] },
  empty: { len: 0, _data: [] },
});
__rock_code_slot_publish();
let missingCatalog = false;
try { __rock_definition_catalog("missing"); } catch (_) { missingCatalog = true; }
const empty = __rock_definition_catalog("empty");
const nonempty = __rock_definition_catalog("widgets");
const result = missingPlan && missingCatalog && empty.len === 0 && nonempty.len === 1
  ? "4/4 PASS" : "0/4 FAIL";
"#,
    );
    let output = node().args(["-e", &source]).output().expect("node");
    assert!(
        output.status.success(),
        "node failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "4/4 PASS");
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

// The whole-program path defines `__rock_state_initialize()` and then calls it;
// the code-slot registry is staged after the bodies, so the CALL has to come
// after the staging. Called before it, a global initialized from a lambda throws
// "missing Rock code slot" at load and the program never runs.
#[test]
fn a_program_global_lambda_initializes_after_the_code_slots_are_staged() {
    let ws = ws_with(
        r#"
global SQUARE: fn(int) -> int = (x: int) => x * x
const OPS: [fn(int) -> int] = [(x: int) => x + 1]
fn main() -> int {
    assert(SQUARE(7) == 49)
    assert(OPS[0](4) == 5)
    return SQUARE(3) + OPS[0](1)
}
"#,
    );
    let js = emit_program(&ws);
    let source = format!(
        "var __G = {{}};\n{}{}{}\n{}\n{}\nconsole.log(main());",
        skull_codegen_js::JS_NB_CHECK_SHIMS,
        skull_codegen_js::JS_VEC_CHECK_SHIMS,
        skull_codegen_js::JS_GLOBALS_RUNTIME,
        skull_codegen_js::JS_CALL_HELPER,
        js
    );
    let output = node()
        .arg("-e")
        .arg(&source)
        .output()
        .expect("run global-lambda program");
    assert!(
        output.status.success(),
        "node failed: {}\n--- js ---\n{js}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "11");
}

/// Interned type ids follow worker timing: graph expansion interns each
/// owner's types on whichever worker expands it. Each `pick{i}` below is a
/// separate owner in one parallel wave and instantiates the `inline(js)`
/// generic at its own type, so an emission order keyed on those ids changes
/// between runs. Every emission of the same input must be byte-identical.
#[test]
fn inline_js_specializations_emit_in_a_stable_order() {
    let mut src = String::from("inline(js) fn tag<V>(value: V) -> int = \"return 1;\"\n");
    let mut body = String::new();
    for i in 0..32 {
        src.push_str(&format!(
            "struct Item{i} {{ v: int }}\nfn pick{i}() -> int {{ return tag(Item{i} {{ v: {i} }}) }}\n"
        ));
        body.push_str(&format!("    n += pick{i}()\n"));
    }
    src.push_str(&format!(
        "fn main() -> int {{\n    let n = 0\n{body}    return n\n}}\n"
    ));
    let ws = ws_with(&src);
    let first = emit_program(&ws);
    assert_eq!(first.matches("\nfunction tag_").count(), 32, "{first}");
    for _ in 0..16 {
        assert_eq!(
            emit_program(&ws),
            first,
            "JS emission order varies between runs"
        );
    }
}

// A test closure that calls a typed catalog accessor reaches it as an ordinary
// callee; the accessor has no Rock body and no intrinsic-table entry, because
// the definition-plan stage defines it under its own symbol.
#[test]
fn closure_calling_a_typed_catalog_accessor_emits() {
    let ws = ws_with(
        r#"
struct WidgetDef { value: int }
@artifact_definition(catalog="widgets", key_param="key")
deco init widget(key: string) {}
@artifact_catalog(catalog="widgets")
@intrinsic extern(lua) fn widget_definition_plan() -> [ArtifactDefinition<WidgetDef>]
@widget(key="alpha") fn make_alpha() -> WidgetDef { return WidgetDef { value: 1 } }
fn root() -> int { return widget_definition_plan().len }
"#,
    );
    let root = crate::common::test_fn_ref(&ws, "root");
    let js = skull_codegen_js::emit::emit_with_closure(&ws, &[(root, "root".to_string())])
        .map_err(|e| e.message)
        .expect("accessor call emits");
    assert!(js.contains("widget_definition_plan()"), "{js}");
}
