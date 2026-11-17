use crate::common::{emit_mir, run_luajit};

#[test]
fn lifecycle_metadata_emits_a_sorted_plan_without_executing_the_decorator() {
    let emit = emit_mir(
        r#"
@lifecycle(phase="update", owner="process", stage_param="stage")
deco init tick(stage: int = 100) {}
@tick(stage=7) fn first() {}
@tick(stage=7) fn second() {}
@tick fn defaulted() {}
fn main() {}
"#,
    );
    assert!(crate::common::definitions_matching(&emit.lua, "__deco_tick").is_empty());
    assert!(!emit.lua.contains("__deco_tick({"));
    let first = emit
        .lua
        .find("name = \"first\", stage = 7")
        .expect("first plan entry");
    let second = emit
        .lua
        .find("name = \"second\", stage = 7")
        .expect("second plan entry");
    let defaulted = emit
        .lua
        .find("name = \"defaulted\", stage = 100")
        .expect("defaulted plan entry");
    assert!(first < second && second < defaulted);
    // `func` is now a compiler-synthesized uniform thunk (`scene_id -> ()`),
    // not the raw CodeRef slot -- calling it is what proves the slot behind
    // it resolved (an unadmitted key throws "missing Rock code slot" at plan
    // construction, before this snippet even runs); `key` on the entry
    // itself carries the same identity string the thunk closed over.
    let out = run_luajit(&format!(
        "{}\nlocal p = _G.__ROCK_LIFECYCLE_PLAN.update; p._data[1].func(0); print(p.len .. ':' .. p._data[1].key)",
        emit.lua
    ));
    assert!(out.starts_with("3:named:<test>::first_0<>"));
}

#[test]
fn scene_creation_uses_the_monomorphized_type_key_without_runtime_discovery() {
    let emit = emit_mir(
        r#"
struct Room { ticks: int }
@lifecycle(phase="update", owner="scene")
deco init scene_update {}
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
    let key = "a_n4_Room_0_z";
    // Through the shape-aware helper: under the bound callee shape the
    // definition header is `__rock_mk_scene_create.. = function(`, so splitting
    // on `\nfunction ` finds nothing.
    let scene_create_body = crate::common::function_body(&emit.lua, "scene_create");
    assert!(scene_create_body.contains(key));
    assert!(!scene_create_body.contains("__rock_get_type"));
    assert!(emit.lua.contains(&format!("root_type = \"{key}\"")));
}

#[test]
fn artifact_definition_metadata_emits_a_keyed_plan_without_running_the_decorator() {
    let emit = emit_mir(
        r#"
@artifact_definition(catalog="widgets", key_param="key")
deco init widget(key: string) {}
@widget(key="alpha") fn make_alpha() -> int { return 1 }
@widget(key="beta") fn make_beta() -> int { return 2 }
fn main() {}
"#,
    );
    assert!(crate::common::definitions_matching(&emit.lua, "__deco_widget").is_empty());
    assert!(!emit.lua.contains("__deco_widget({"));
    assert!(emit.lua.contains("__rock_definition_plan_stage({"));
    let alpha = emit.lua.find("key = \"alpha\"").expect("alpha plan entry");
    let beta = emit.lua.find("key = \"beta\"").expect("beta plan entry");
    assert!(alpha < beta);
    assert!(
        emit.lua
            .contains("factory = __rock_code_slot_pending_ref(\"named:<test>::make_alpha_0<>\")")
    );
    assert!(
        emit.lua
            .contains("factory = __rock_code_slot_pending_ref(\"named:<test>::make_beta_0<>\")")
    );
    let out = run_luajit(&format!(
        "{}\nlocal p = _G.__ROCK_DEFINITION_PLAN.widgets; print(p.len .. ':' .. p._data[1].key .. ':' .. p._data[1].factory())",
        emit.lua
    ));
    assert_eq!(out, "2:alpha:1");
}

#[test]
fn typed_catalog_accessor_is_compiler_owned_and_returns_the_published_view() {
    let emit = emit_mir(
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
    assert!(crate::common::definitions_matching(&emit.lua, "widget_definition_plan").len() == 1);
    assert!(
        emit.lua
            .contains("return __rock_definition_catalog(\"widgets\")")
    );
    assert!(!emit.lua.contains("__ROCK_DEFINITION_PLAN or {}"));
    let out = run_luajit(&format!(
        "{}\nlocal p = widget_definition_plan(); local e = empty_definition_plan(); local missing = not pcall(function() return __rock_definition_catalog('missing') end); print(p.len .. ':' .. p._data[1].key .. ':' .. p._data[1].definition.value .. ':' .. e.len .. ':' .. tostring(missing))",
        emit.lua
    ));
    assert_eq!(out, "1:alpha:1:0:true");
}

#[test]
fn full_publication_replaces_lifecycle_membership_by_logical_key() {
    let v0 = emit_mir(
        r#"
@lifecycle(phase="update", owner="process") deco init tick {}
@tick fn removed() {}
@tick fn retained() {}
fn main() {}
"#,
    );
    let v1 = emit_mir(
        r#"
@lifecycle(phase="update", owner="process") deco init tick {}
@tick fn retained() {}
@tick fn added() {}
fn main() {}
"#,
    );
    let script = format!(
        r#"
{}
local old = _G.__ROCK_LIFECYCLE_PLAN.update
_G.HOT_RELOAD = true
{}
local new = _G.__ROCK_LIFECYCLE_PLAN.update
print(old.len .. ":" .. new.len .. ":" .. old._data[1].name .. ":" .. new._data[1].name .. ":" .. new._data[2].name)
"#,
        v0.lua, v1.lua
    );
    let out = run_luajit(&script);
    assert_eq!(out, "2:2:removed:retained:added");
}
