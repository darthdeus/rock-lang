use crate::common::{emit_module, mangled_name, run_luajit};

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
fn globals_open_with_sized_storage_and_keep_it_across_reload() {
    let lua = emit_module(FIXTURE);
    let open = lua
        .lines()
        .find(|l| l.starts_with("__rock_state_cell = __rock_globals_open("))
        .expect("open line");
    let nums: Vec<usize> = open
        .trim_end_matches(')')
        .rsplit(", ")
        .take(2)
        .map(|n| n.parse().expect("capacity number"))
        .collect();
    assert_eq!(nums[1], 9);
    // The hash part also counts stdlib statics, so pin only its shape.
    assert!(nums[0] >= 16 && nums[0].is_power_of_two());
    let open_at = lua
        .find("\n__rock_state_cell = __rock_globals_open(")
        .expect("open line");
    let init = lua
        .find("\n__rock_state_initialize()\n")
        .expect("initialize call");
    assert!(open_at < init);
    assert!(lua.contains("__rock_table_new(narr, nhash)"));

    let bump = mangled_name(&lua, "bump");
    let program = format!(
        "{lua}
local storage = __rock_globals
print({bump}())
print({bump}())
{lua}
print({bump}())
print(storage == __rock_globals, __rock_globals_fresh)
"
    );
    assert_eq!(run_luajit(&program), "1\n2\n3\ntrue\tfalse");
}

#[test]
fn a_body_reload_keeps_a_live_scene_and_its_arena_row() {
    use crate::common::{body_id, edit, ws_with_stdlib};

    let before = r#"
struct Row { n: int }
struct Root { rows: Arena<Row> }
global SCENE: Scene<Root>? = null
global HANDLE: Entity<Row>? = null

fn build_scene(builder: SceneBuilder) -> Root? {
    return Root { rows: builder.arena() }
}

@nomangle fn seed() {
    let scene: Scene<Root> = scene_create(build_scene).unwrap()
    SCENE = scene
    HANDLE = scene.get().unwrap().rows.insert(Row { n: 17 })
}

@nomangle fn live_arena_row() -> int {
    let rows = SCENE.unwrap().get().unwrap().rows
    return rows.get(HANDLE.unwrap()).unwrap().n + 1
}
"#;
    let after = ws_with_stdlib(&before.replace(".n + 1", ".n + 2"));
    let change = edit(
        &ws_with_stdlib(before),
        &after,
        &[body_id(&after, "live_arena_row")],
    );
    let overlay = change.overlay.expect("compatible arena layout");
    assert_eq!(change.base.global_layout, change.incremental.global_layout);
    let program = format!(
        "{}\nseed()\nprint(live_arena_row())\n{}\nprint(live_arena_row())",
        change.base.lua, overlay.lua,
    );
    assert_eq!(run_luajit(&program), "18\n19");
}
