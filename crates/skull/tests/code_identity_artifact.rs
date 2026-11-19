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
    let out = run_luajit(&format!(
        "{}\nlocal p = _G.__ROCK_LIFECYCLE_PLAN.update; for i=1,p.len do local e = p._data[i]; e.func(0); print(e.name .. ':' .. e.stage) end",
        emit.lua
    ));
    assert_eq!(out, "first:7\nsecond:7\ndefaulted:100");
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
    let scene_create_body = crate::common::function_body(&emit.lua, "scene_create");
    assert!(scene_create_body.contains(key));
    assert!(!scene_create_body.contains("__rock_get_type"));
    assert!(emit.lua.contains(&format!("root_type = \"{key}\"")));
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
@widget(key="beta") fn make_beta() -> WidgetDef { return WidgetDef { value: 2 } }
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
        "{}\nlocal p = widget_definition_plan(); local e = empty_definition_plan(); local missing = not pcall(function() return __rock_definition_catalog('missing') end); print(p.len .. ':' .. p._data[1].key .. ':' .. p._data[1].factory().value .. ':' .. p._data[2].key .. ':' .. e.len .. ':' .. tostring(missing))",
        emit.lua
    ));
    assert_eq!(out, "2:alpha:1:beta:0:true");
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
