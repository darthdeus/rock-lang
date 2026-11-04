use crate::common::ws_with;

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
fn lua_and_js_open_the_same_global_layout() {
    let ws = ws_with(FIXTURE);
    let mid = *ws
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("test module");
    let lua = skull::mir_emit::emit_workspace_mir(&ws, mid).expect("lua emit");
    let js = skull_codegen_js::emit::emit_program(&ws, mid).expect("js emit");
    assert_eq!(lua.global_layout, js.global_layout);
    assert!(
        js.program
            .contains(&format!("__rock_globals_open({:?});", js.global_layout))
    );
}
