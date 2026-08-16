use crate::common::ws_with;
use skull_mir::state_image::StateImagePlan;

const FIXTURE: &str = r#"
value Boxed { x: int, y: int }
nobox value Flat { x: int, y: int }

global BOXED: Boxed = Boxed { x: 1, y: 2 }
global FLAT: Flat = Flat { x: 3, y: 4 }
global RESULT: int = 0

fn with_static() -> int {
    static initialized = 7
    static lazy = 2
    return initialized + lazy
}

@inline fn generic_static<T>(value: T) -> T {
    static calls = 0
    calls = calls + 1
    return value
}

co fn frame_source() -> Coroutine<int> {
    let live = 11
    yield live
    yield live + 1
}

fn main() {
    let _ = generic_static(5)
    let _frame = frame_source()
    RESULT = with_static() + BOXED.x + FLAT.x
}
"#;

#[test]
fn lua_and_js_emit_the_same_logical_state_schema() {
    let ws = ws_with(FIXTURE);
    let mid = *ws
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("test module");
    let lua = skull::mir_emit::emit_workspace_mir(&ws, mid).expect("lua emit");
    let js = skull_codegen_js::emit::emit_program(&ws, mid).expect("js emit");

    assert_eq!(lua.state_schema, js.state_schema);
    let census = lua.state_schema.census();
    assert!(
        census.examined > 0 && census.is_total(),
        "schema census is not total: {census:?}"
    );
    assert!(census.globals > 0 && census.statics > 0 && census.frames > 0 && census.storage > 0);
}

#[test]
fn lua_and_js_derive_the_same_state_image_plan() {
    let ws = ws_with(FIXTURE);
    let mid = *ws
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("test module");
    let lua = skull::mir_emit::emit_workspace_mir(&ws, mid).expect("lua emit");
    let js = skull_codegen_js::emit::emit_program(&ws, mid).expect("js emit");
    let lua_plan = StateImagePlan::from_schema(&lua.state_schema).expect("Lua plan");
    let js_plan = StateImagePlan::from_schema(&js.state_schema).expect("JS plan");
    assert_eq!(lua_plan, js_plan);
    assert!(lua.lua.contains("__ROCK_STATE_IMAGE_PLAN = {"));
    assert!(js.program.contains("var __ROCK_STATE_IMAGE_PLAN = {"));
    assert_eq!(lua_plan.census().examined, lua_plan.census().assigned);
    assert!(lua_plan.census().examined > 0);
}
