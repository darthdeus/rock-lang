use std::path::PathBuf;

#[test]
fn mag_inspector_never_repositions_itself_from_the_selection() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let source = std::fs::read_to_string(root.join("mag/editor_windows.rock")).unwrap();

    assert!(
        source.contains("imgui_begin_window(\"Inspector\", 0)"),
        "the Inspector window itself must remain"
    );
    let inspector = source
        .split_once("fn draw_inspector_window()")
        .expect("Inspector draw function")
        .1
        .split_once("fn draw_outliner_window()")
        .expect("Outliner follows Inspector")
        .0;
    assert!(
        !inspector.contains("imgui_set_next_window_pos"),
        "the Inspector still requests a new position while drawing"
    );
    for removed in ["INSPECTOR_SPAWN_KEY", "inspector_spawn_pos"] {
        assert!(
            !source.contains(removed),
            "the Inspector still carries selection-driven positioning through {removed}"
        );
    }
}
