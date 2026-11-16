//! Whole-program emission (`emit_program`): the host-facing plans and code
//! slots it publishes, and Lua/JS parity of those plans. Decorator arguments,
//! global init order and definition catalogs run in the corpus on both
//! backends (annotations/decorator_args.rock, functions/js_module_globals.rock,
//! values/definition_catalog.rock).

use crate::common::{node, ws_with, ws_with_named};

fn emit_program(ws: &bone_core::workspace::Workspace) -> String {
    let mid = crate::common::test_mid(ws);
    skull_codegen_js::emit::emit_program(ws, mid)
        .unwrap_or_else(|e| panic!("emit_program rejected: {}", e.message))
        .program
}

// A hot reload swaps code slots, so a decorator descriptor must name the
// logical slot rather than capture the current backend body.
#[test]
fn decorator_descriptors_reference_code_slots() {
    let ws = ws_with(
        r#"
struct Sys { func: fn() -> (), name: string }
global SYSTEMS: [Sys] = []
deco init system() {
    SYSTEMS.push(Sys { func: transmute(target.func), name: transmute(target.name) })
}
@system
fn alpha() { print("alpha") }
fn main() { for s in SYSTEMS { s.func() } }
"#,
    );
    let js = emit_program(&ws);
    let alpha_app = js
        .lines()
        .find(|l| l.contains("__deco_system(") && l.contains("\"alpha\""))
        .unwrap_or_else(|| panic!("no alpha application line:\n{js}"));
    assert!(
        alpha_app.contains("func: __rock_code_slot_ref(\"named:<test>::alpha<>\")"),
        "{alpha_app}"
    );
    assert!(!alpha_app.contains("func: alpha"), "{alpha_app}");
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
/// `main` deliberately does NOT push or set, so the demand has exactly one
/// origin. The normal lane may inline the specialized call; a no-inline lane
/// must retain the standalone specialized `set` and its concrete store. `push`
/// is the backend's own append in both lanes.
#[test]
fn a_specialization_demanded_only_by_a_decorator_body_is_emitted() {
    let ws = ws_with(
        r#"
global XS: [int] = []
deco init reg() {
    XS.push(1)
    XS.set(0, 2)
}
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
        standalone.contains("function set_"),
        "decorator-only specialization did not emit the concrete set definition:\n{standalone}"
    );
    let set = &standalone[standalone.find("function set_").expect("set definition")..];
    let set = &set[..set.find("\n}").expect("end of the set definition")];
    assert!(
        set.contains("vec_set"),
        "standalone decorator specialization lost the concrete set operation: {set}"
    );
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
