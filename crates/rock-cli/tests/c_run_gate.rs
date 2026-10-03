use std::process::Command;

fn run(path: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(["run", path.to_str().expect("utf8 path"), "--backend", "c"])
        .output()
        .expect("spawn rock")
}

fn workspace_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn c_optional_array_literals_are_required_native_tests() {
    let output = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "test", "--backend", "c",
            "tests/c_optional_array_literal_store.rock",
            "tests/value_store_aliasing.rock",
            "--require-test", "c_optional_array_literal_local_store",
            "--require-test", "c_optional_array_literal_keeps_null_entries",
            "--require-test", "c_optional_array_literal_field_store",
            "--require-test", "c_optional_array_literal_argument_store",
            "--require-test", "optional_array_element_if_let_store_twice_copies",
            "--require-test", "optional_array_for_loop_if_let_store_twice_copies",
        ])
        .current_dir(workspace_root())
        .output()
        .expect("run required C optional array tests");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "required C optional array tests failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("19/19 @tests passed (0 failed, 0 skipped)"), "{stdout}");
}

#[test]
fn c_tool_imgui_bindings_link_and_run_against_actual_stone_artifact() {
    let root = workspace_root();
    let dir = tempfile::tempdir().expect("tool imgui fixture directory");
    let source_path = dir.path().join("stone/engine/input/tool.rock");
    std::fs::create_dir_all(source_path.parent().expect("tool fixture parent"))
        .expect("create tool fixture parent");
    let production_source = std::fs::read_to_string(root.join("stone/engine/input/tool.rock"))
        .expect("read authoritative tool source");
    // The C fixture parser does not accept Rock's public item modifier.
    let mut source = production_source
        .lines()
        .map(|line| line.strip_prefix("pub ").unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n");
    source.push_str(
        r#"

global UI_TEXT_INPUT_CAPTURE: bool = false

fn stone_diag_set_s(key: string, value: string) {}

fn main() {
    let id = __tool_imgui_key_id("space")
    let down = __tool_imgui_key_down_id(42)
    let pressed = __tool_imgui_key_pressed_id(42)
    let ctrl = __tool_imgui_ctrl_down()
    print("id={id} down={down} pressed={pressed} ctrl={ctrl}")
    __tool_imgui_inject_key_id(42)
    __tool_imgui_release_key_id(42)
    print("tool-imgui-ok")
}
"#,
    );
    std::fs::write(&source_path, source).expect("write tool imgui fixture");
    let output = dir.path().join("tool-imgui-consumer");
    let compile = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "compile",
            source_path.to_str().expect("utf8 tool fixture"),
            "--backend",
            "c",
            "--output",
            output.to_str().expect("utf8 tool output"),
        ])
        .current_dir(&root)
        .env_remove("ROCK_SKIP_ENGINE_BUILD")
        .output()
        .expect("spawn C tool compile");
    assert!(
        compile.status.success(),
        "C tool consumer did not link against Stone:\nstdout:{}\nstderr:{}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr)
    );
    let generated = std::fs::read_to_string(output.with_extension("c"))
        .expect("read generated C tool consumer");
    for symbol in [
        "stone_imgui_is_key_down",
        "stone_imgui_ctrl_down",
        "stone_imgui_key_id",
        "stone_imgui_inject_key",
        "stone_imgui_release_key",
    ] {
        assert!(generated.contains(symbol), "generated consumer omitted {symbol}");
    }
    let run = Command::new(&output)
        .current_dir(&root)
        .output()
        .expect("run linked C tool consumer");
    assert!(
        run.status.success(),
        "linked C tool consumer failed ({:?}):\nstdout:{}\nstderr:{}",
        run.status,
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(run.stdout, b"id=-2 down=false pressed=false ctrl=false\ntool-imgui-ok\n");
}

#[test]
fn c_imgui_combo_bindings_link_against_actual_stone_artifact() {
    let root = workspace_root();
    let dir = tempfile::tempdir().expect("combo imgui fixture directory");
    let source_path = dir.path().join("stone/engine/ui/imgui.rock");
    std::fs::create_dir_all(source_path.parent().expect("combo fixture parent"))
        .expect("create combo fixture parent");
    let source = r#"
@inline inline(lua) fn imgui_begin_combo(label: string, preview: string) -> bool = "return imgui.BeginCombo(label, preview, 0)"
@inline inline(lua) fn imgui_end_combo() = "imgui.EndCombo()"
@inline inline(lua) fn imgui_set_item_default_focus() = "imgui.SetItemDefaultFocus()"

fn main() {
    if imgui_begin_combo("Modes", "Normal") { let _ = 1 ./ 0 }
    imgui_end_combo()
    imgui_set_item_default_focus()
    print("combo-imgui-ok")
}
"#;
    std::fs::write(&source_path, source).expect("write combo imgui fixture");
    let output = dir.path().join("combo-imgui-consumer");
    let compile = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "compile",
            source_path.to_str().expect("utf8 combo fixture"),
            "--backend",
            "c",
            "--output",
            output.to_str().expect("utf8 combo output"),
        ])
        .current_dir(&root)
        .env_remove("ROCK_SKIP_ENGINE_BUILD")
        .output()
        .expect("spawn C combo compile");
    assert!(
        compile.status.success(),
        "C combo consumer did not link against Stone:\nstdout:{}\nstderr:{}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr)
    );
    let generated = std::fs::read_to_string(output.with_extension("c"))
        .expect("read generated C combo consumer");
    for symbol in [
        "stone_imgui_begin_combo",
        "stone_imgui_end_combo",
        "stone_imgui_set_item_default_focus",
    ] {
        assert!(generated.contains(symbol), "generated consumer omitted {symbol}");
    }
    let run = Command::new(&output)
        .current_dir(&root)
        .output()
        .expect("run linked C combo consumer");
    assert!(
        run.status.success(),
        "linked C combo consumer failed:\nstdout:{}\nstderr:{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(run.stdout, b"combo-imgui-ok\n");
}

#[test]
fn c_imgui_popup_bindings_link_against_actual_stone_artifact() {
    let root = workspace_root();
    let dir = tempfile::tempdir().expect("popup imgui fixture directory");
    let source_path = dir.path().join("stone/engine/ui/imgui.rock");
    std::fs::create_dir_all(source_path.parent().expect("popup fixture parent"))
        .expect("create popup fixture parent");
    let source = r#"
inline(lua) fn imgui_open_popup(id: string) = "imgui.OpenPopup_Str(id, 0)"
inline(lua) fn imgui_begin_popup(id: string) -> bool = "return imgui.BeginPopup(id, 0)"
inline(lua) fn imgui_end_popup() = "imgui.EndPopup()"

fn main() {
    imgui_open_popup("QuestActions")
    if imgui_begin_popup("QuestActions") { print("popup-open") }
    imgui_end_popup()
    print("popup-imgui-ok")
}
"#;
    std::fs::write(&source_path, source).expect("write popup imgui fixture");
    let output = dir.path().join("popup-imgui-consumer");
    let compile = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "compile",
            source_path.to_str().expect("utf8 popup fixture"),
            "--backend",
            "c",
            "--output",
            output.to_str().expect("utf8 popup output"),
        ])
        .current_dir(&root)
        .env_remove("ROCK_SKIP_ENGINE_BUILD")
        .output()
        .expect("spawn C popup compile");
    assert!(
        compile.status.success(),
        "C popup consumer did not link against Stone:\nstdout:{}\nstderr:{}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr)
    );
    let generated = std::fs::read_to_string(output.with_extension("c"))
        .expect("read generated C popup consumer");
    for symbol in [
        "stone_imgui_open_popup",
        "stone_imgui_begin_popup",
        "stone_imgui_end_popup",
    ] {
        assert!(generated.contains(symbol), "generated consumer omitted {symbol}");
    }
    let run = Command::new(&output)
        .current_dir(&root)
        .output()
        .expect("run linked C popup consumer");
    assert!(
        run.status.success(),
        "linked C popup consumer failed:\nstdout:{}\nstderr:{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(run.stdout, b"popup-imgui-ok\n");
}

#[test]
fn c_imgui_menu_item_links_against_actual_stone_artifact() {
    let root = workspace_root();
    let dir = tempfile::tempdir().expect("menu item imgui fixture directory");
    let source_path = dir.path().join("stone/engine/ui/imgui.rock");
    std::fs::create_dir_all(source_path.parent().expect("menu item fixture parent"))
        .expect("create menu item fixture parent");
    let source = r#"
inline(lua) fn imgui_menu_item(label: string, enabled: bool = true) -> bool = """
    return imgui.MenuItem_Bool(label, nil, false, enabled)
"""

fn main() {
    if imgui_menu_item("Duplicate") { print("menu-open") }
    if imgui_menu_item("Disabled", false) { print("menu-open") }
    print("menu-item-imgui-ok")
}
"#;
    std::fs::write(&source_path, source).expect("write menu item imgui fixture");
    let output = dir.path().join("menu-item-imgui-consumer");
    let compile = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "compile",
            source_path.to_str().expect("utf8 menu item fixture"),
            "--backend",
            "c",
            "--output",
            output.to_str().expect("utf8 menu item output"),
        ])
        .current_dir(&root)
        .env_remove("ROCK_SKIP_ENGINE_BUILD")
        .output()
        .expect("spawn C menu item compile");
    assert!(
        compile.status.success(),
        "C menu item consumer did not link against Stone:\nstdout:{}\nstderr:{}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr)
    );
    let generated = std::fs::read_to_string(output.with_extension("c"))
        .expect("read generated C menu item consumer");
    assert!(
        generated.contains("stone_imgui_menu_item"),
        "generated consumer omitted stone_imgui_menu_item"
    );
    let run = Command::new(&output)
        .current_dir(&root)
        .output()
        .expect("run linked C menu item consumer");
    assert!(
        run.status.success(),
        "linked C menu item consumer failed:\nstdout:{}\nstderr:{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(run.stdout, b"menu-item-imgui-ok\n");
}

#[test]
fn c_imgui_begin_child_scroll_raw_links_against_actual_stone_artifact() {
    let root = workspace_root();
    let dir = tempfile::tempdir().expect("raw child imgui fixture directory");
    let source_path = dir.path().join("stone/engine/ui/imgui.rock");
    std::fs::create_dir_all(source_path.parent().expect("raw child fixture parent"))
        .expect("create raw child fixture parent");
    let source = r#"
@scalar_abi inline(lua) fn __imgui_begin_child_scroll_raw(id: string, size: Vec2) -> bool = """
    return imgui.BeginChild_Str(id, imgui.ImVec2_Float(size__x, size__y), 0, 0)
"""

fn main() {
    if __imgui_begin_child_scroll_raw("HistoryTree", v(16.0, 24.0)) { print("child-open") }
    print("raw-child-imgui-ok")
}
"#;
    std::fs::write(&source_path, source).expect("write raw child imgui fixture");
    let output = dir.path().join("raw-child-imgui-consumer");
    let compile = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "compile",
            source_path.to_str().expect("utf8 raw child fixture"),
            "--backend",
            "c",
            "--output",
            output.to_str().expect("utf8 raw child output"),
        ])
        .current_dir(&root)
        .env_remove("ROCK_SKIP_ENGINE_BUILD")
        .output()
        .expect("spawn C raw child compile");
    assert!(
        compile.status.success(),
        "C raw child consumer did not link against Stone:\nstdout:{}\nstderr:{}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr)
    );
    let generated = std::fs::read_to_string(output.with_extension("c"))
        .expect("read generated C raw child consumer");
    assert!(
        generated.contains("stone_imgui_begin_child"),
        "generated consumer omitted stone_imgui_begin_child"
    );
    let run = Command::new(&output)
        .current_dir(&root)
        .output()
        .expect("run linked C raw child consumer");
    assert!(
        run.status.success(),
        "linked C raw child consumer failed:\nstdout:{}\nstderr:{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(run.stdout, b"raw-child-imgui-ok\n");
}

#[test]
fn c_imgui_set_window_pos_named_links_against_actual_stone_artifact() {
    let root = workspace_root();
    let dir = tempfile::tempdir().expect("window position imgui fixture directory");
    let source_path = dir.path().join("stone/engine/ui/imgui.rock");
    std::fs::create_dir_all(source_path.parent().expect("window position fixture parent"))
        .expect("create window position fixture parent");
    let source = r#"
@scalar_abi inline(lua) fn imgui_set_window_pos_named(title: string, pos: Vec2) = """
    imgui.C.igSetWindowPos_Str(title, imgui.ImVec2_Float(pos__x, pos__y), 0)
"""

fn main() {
    imgui_set_window_pos_named("Quests##quest_editor", v(33.5, 44.25))
    print("window-pos-imgui-ok")
}
"#;
    std::fs::write(&source_path, source).expect("write window position imgui fixture");
    let output = dir.path().join("window-pos-imgui-consumer");
    let compile = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "compile",
            source_path.to_str().expect("utf8 window position fixture"),
            "--backend",
            "c",
            "--output",
            output.to_str().expect("utf8 window position output"),
        ])
        .current_dir(&root)
        .env_remove("ROCK_SKIP_ENGINE_BUILD")
        .output()
        .expect("spawn C window position compile");
    assert!(
        compile.status.success(),
        "C window position consumer did not link against Stone:\nstdout:{}\nstderr:{}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr)
    );
    let generated = std::fs::read_to_string(output.with_extension("c"))
        .expect("read generated C window position consumer");
    assert!(
        generated.contains("stone_imgui_set_window_pos_named"),
        "generated consumer omitted stone_imgui_set_window_pos_named"
    );
    let run = Command::new(&output)
        .current_dir(&root)
        .output()
        .expect("run linked C window position consumer");
    assert!(
        run.status.success(),
        "linked C window position consumer failed:\nstdout:{}\nstderr:{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(run.stdout, b"window-pos-imgui-ok\n");
}

#[test]
fn c_imgui_begin_window_raw_links_against_actual_stone_artifact() {
    let root = workspace_root();
    let dir = tempfile::tempdir().expect("window imgui fixture directory");
    let source_path = dir.path().join("stone/engine/ui/imgui.rock");
    std::fs::create_dir_all(source_path.parent().expect("window fixture parent"))
        .expect("create window fixture parent");
    let source = r#"
inline(lua) fn __imgui_begin_window_raw(title: string, flags: int) -> bool = """
    return imgui.Begin(title, nil, flags)
"""

fn main() {
    if __imgui_begin_window_raw("Quests##quest_editor", 2048) { print("window-open") }
    print("window-imgui-ok")
}
"#;
    std::fs::write(&source_path, source).expect("write window imgui fixture");
    let output = dir.path().join("window-imgui-consumer");
    let compile = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "compile",
            source_path.to_str().expect("utf8 window fixture"),
            "--backend",
            "c",
            "--output",
            output.to_str().expect("utf8 window output"),
        ])
        .current_dir(&root)
        .env_remove("ROCK_SKIP_ENGINE_BUILD")
        .output()
        .expect("spawn C window compile");
    assert!(
        compile.status.success(),
        "C window consumer did not link against Stone:\nstdout:{}\nstderr:{}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr)
    );
    let generated = std::fs::read_to_string(output.with_extension("c"))
        .expect("read generated C window consumer");
    assert!(
        generated.contains("stone_imgui_begin_window"),
        "generated consumer omitted stone_imgui_begin_window"
    );
    let run = Command::new(&output)
        .current_dir(&root)
        .output()
        .expect("run linked C window consumer");
    assert!(
        run.status.success(),
        "linked C window consumer failed:\nstdout:{}\nstderr:{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(run.stdout, b"window-imgui-ok\n");
}

#[test]
fn c_imgui_item_hover_queries_link_against_actual_stone_artifact() {
    let root = workspace_root();
    let dir = tempfile::tempdir().expect("item hover imgui fixture directory");
    let source_path = dir.path().join("stone/engine/ui/imgui.rock");
    std::fs::create_dir_all(source_path.parent().expect("item hover fixture parent"))
        .expect("create item hover fixture parent");
    let source = r#"
inline(lua) fn imgui_is_item_hovered() -> bool = """
    return imgui.IsItemHovered()
"""
inline(lua) fn imgui_is_item_hovered_allow_active() -> bool = """
    return imgui.IsItemHovered(128)
"""

fn main() {
    if imgui_is_item_hovered() { print("hovered") }
    if imgui_is_item_hovered_allow_active() { print("hovered") }
    print("item-hover-imgui-ok")
}
"#;
    std::fs::write(&source_path, source).expect("write item hover imgui fixture");
    let output = dir.path().join("item-hover-imgui-consumer");
    let compile = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "compile",
            source_path.to_str().expect("utf8 item hover fixture"),
            "--backend",
            "c",
            "--output",
            output.to_str().expect("utf8 item hover output"),
        ])
        .current_dir(&root)
        .env_remove("ROCK_SKIP_ENGINE_BUILD")
        .output()
        .expect("spawn C item hover compile");
    assert!(
        compile.status.success(),
        "C item hover consumer did not link against Stone:\nstdout:{}\nstderr:{}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr)
    );
    let generated = std::fs::read_to_string(output.with_extension("c"))
        .expect("read generated C item hover consumer");
    for flags in ["(int)(0)", "(int)(128)"] {
        assert!(
            generated.contains(&format!("stone_imgui_is_item_hovered({flags})")),
            "generated consumer omitted item hover flags {flags}"
        );
    }
    let run = Command::new(&output)
        .current_dir(&root)
        .output()
        .expect("run linked C item hover consumer");
    assert!(
        run.status.success(),
        "linked C item hover consumer failed:\nstdout:{}\nstderr:{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(run.stdout, b"item-hover-imgui-ok\n");
}

#[test]
fn c_imgui_set_tooltip_links_against_actual_stone_artifact() {
    let root = workspace_root();
    let dir = tempfile::tempdir().expect("tooltip imgui fixture directory");
    let source_path = dir.path().join("stone/engine/ui/imgui.rock");
    std::fs::create_dir_all(source_path.parent().expect("tooltip fixture parent"))
        .expect("create tooltip fixture parent");
    let source = r#"
@inline inline(lua) fn imgui_set_tooltip(text: string) = "imgui.SetTooltip(text)"

fn main() {
    imgui_set_tooltip("tooltip % 100")
    print("tooltip-imgui-ok")
}
"#;
    std::fs::write(&source_path, source).expect("write tooltip imgui fixture");
    let output = dir.path().join("tooltip-imgui-consumer");
    let compile = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "compile",
            source_path.to_str().expect("utf8 tooltip fixture"),
            "--backend",
            "c",
            "--output",
            output.to_str().expect("utf8 tooltip output"),
        ])
        .current_dir(&root)
        .env_remove("ROCK_SKIP_ENGINE_BUILD")
        .output()
        .expect("spawn C tooltip compile");
    assert!(
        compile.status.success(),
        "C tooltip consumer did not link against Stone:\nstdout:{}\nstderr:{}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr)
    );
    let generated = std::fs::read_to_string(output.with_extension("c"))
        .expect("read generated C tooltip consumer");
    assert!(
        generated.contains("stone_imgui_set_tooltip"),
        "generated consumer omitted stone_imgui_set_tooltip"
    );
    let run = Command::new(&output)
        .current_dir(&root)
        .output()
        .expect("run linked C tooltip consumer");
    assert!(
        run.status.success(),
        "linked C tooltip consumer failed:\nstdout:{}\nstderr:{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(run.stdout, b"tooltip-imgui-ok\n");
}

#[test]
fn c_imgui_image_button_links_against_actual_stone_artifact() {
    let root = workspace_root();
    let dir = tempfile::tempdir().expect("image button imgui fixture directory");
    let source_path = dir.path().join("stone/engine/ui/imgui.rock");
    std::fs::create_dir_all(source_path.parent().expect("image button fixture parent"))
        .expect("create image button fixture parent");
    let source = r###"
@scalar_abi inline(lua) fn imgui_image_button_raw(
    id: string,
    image: Image,
    size: Vec2,
    uv1: Vec2,
    uv2: Vec2,
    bg_color: Color,
    tint_color: Color
) -> bool = """
    local cache = __imgui_shared_image_packet()

    cache.s.x, cache.s.y = size__x, size__y
    cache.u1.x, cache.u1.y = uv1__x, uv1__y
    cache.u2.x, cache.u2.y = uv2__x, uv2__y
    cache.bg.x, cache.bg.y = bg_color__r, bg_color__g
    cache.bg.z, cache.bg.w = bg_color__b, bg_color__a
    cache.tint.x, cache.tint.y = tint_color__r, tint_color__g
    cache.tint.z, cache.tint.w = tint_color__b, tint_color__a

    local texture_id
    if _G.__ROCK_RENDERER_BACKEND == 'wgpu' then
        texture_id = image
    else
        texture_id = stone.stone_get_image_texture(image)
    end
    cache.tex._TexData = nil
    cache.tex._TexID = texture_id
    return imgui.C.igImageButton(id, cache.tex, cache.s, cache.u1, cache.u2, cache.bg, cache.tint)
"""

newtype Image(int)
struct Color { r: float, g: float, b: float, a: float }

fn main() {
    let _ = imgui_image_button_raw("quest##17", Image(17), v(40.5, 22.25), v(0.1, 0.2), v(0.8, 0.9), Color { r: 0.3, g: 0.4, b: 0.5, a: 0.6 }, Color { r: 0.7, g: 0.8, b: 0.9, a: 1.0 })
    print("image-button-imgui-ok")
}
"###;
    std::fs::write(&source_path, source).expect("write image button imgui fixture");
    let output = dir.path().join("image-button-imgui-consumer");
    let compile = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "compile",
            source_path.to_str().expect("utf8 image button fixture"),
            "--backend",
            "c",
            "--output",
            output.to_str().expect("utf8 image button output"),
        ])
        .current_dir(&root)
        .env_remove("ROCK_SKIP_ENGINE_BUILD")
        .output()
        .expect("spawn C image button compile");
    assert!(
        compile.status.success(),
        "C image button consumer did not link against Stone:\nstdout:{}\nstderr:{}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr)
    );
    let generated = std::fs::read_to_string(output.with_extension("c"))
        .expect("read generated C image button consumer");
    assert!(
        generated.contains("stone_imgui_image_button"),
        "generated consumer omitted stone_imgui_image_button"
    );
    let run = Command::new(&output)
        .current_dir(&root)
        .output()
        .expect("run linked C image button consumer");
    assert!(
        run.status.success(),
        "linked C image button consumer failed:\nstdout:{}\nstderr:{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(run.stdout, b"image-button-imgui-ok\n");
}

#[test]
fn c_imgui_window_add_rect_filled_links_against_actual_stone_artifact() {
    let root = workspace_root();
    let dir = tempfile::tempdir().expect("window rectangle imgui fixture directory");
    let source_path = dir.path().join("stone/engine/ui/imgui.rock");
    std::fs::create_dir_all(source_path.parent().expect("window rectangle fixture parent"))
        .expect("create window rectangle fixture parent");
    let source = r#"
@scalar_abi inline(lua) fn imgui_window_add_rect_filled(
    p_min: Vec2, p_max: Vec2, col_abgr: int
) = """
    local dl = imgui.GetWindowDrawList()
    if dl == nil then return end
    dl:AddRectFilled(imgui.ImVec2_Float(p_min__x, p_min__y),
                     imgui.ImVec2_Float(p_max__x, p_max__y),
                     col_abgr)
"""

fn main() {
    imgui_window_add_rect_filled(v(12.5, -4.25), v(90.75, 44.5), 16909060)
    print("window-rect-imgui-ok")
}
"#;
    std::fs::write(&source_path, source).expect("write window rectangle imgui fixture");
    let output = dir.path().join("window-rect-imgui-consumer");
    let compile = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "compile",
            source_path.to_str().expect("utf8 window rectangle fixture"),
            "--backend",
            "c",
            "--output",
            output.to_str().expect("utf8 window rectangle output"),
        ])
        .current_dir(&root)
        .env_remove("ROCK_SKIP_ENGINE_BUILD")
        .output()
        .expect("spawn C window rectangle compile");
    assert!(
        compile.status.success(),
        "C window rectangle consumer did not link against Stone:\nstdout:{}\nstderr:{}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr)
    );
    let generated = std::fs::read_to_string(output.with_extension("c"))
        .expect("read generated C window rectangle consumer");
    assert!(
        generated.contains("stone_imgui_window_add_rect_filled"),
        "generated consumer omitted stone_imgui_window_add_rect_filled"
    );
    let run = Command::new(&output)
        .current_dir(&root)
        .output()
        .expect("run linked C window rectangle consumer");
    assert!(
        run.status.success(),
        "linked C window rectangle consumer failed:\nstdout:{}\nstderr:{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(run.stdout, b"window-rect-imgui-ok\n");
}

#[test]
fn c_compile_keeps_artifacts_and_invalidates_stale_defaults() {
    assert!(
        Command::new("clang")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success()),
        "C backend gate requires clang"
    );
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("foo.rock"),
        "fn echo(value: string) -> string { return value }\n\
         fn main() { let message = echo(\"hi\"); print(message) }\n",
    )
    .expect("write program");
    let compile = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(["compile", "foo.rock", "--backend", "c"])
        .current_dir(dir.path())
        .output()
        .expect("spawn rock compile");
    assert!(
        compile.status.success(),
        "C compile failed:\nstdout:{}\nstderr:{}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr)
    );

    let c_path = dir.path().join("tmp/foo.c");
    let binary = dir.path().join("tmp/foo");
    let source = std::fs::read_to_string(&c_path).expect("generated C source");
    assert!(source.contains("rock_print_string"), "{source}");
    assert!(source.contains("rock_string p0"), "{source}");
    assert!(binary.is_file(), "native binary was not written");
    let run = Command::new(&binary).output().expect("run native binary");
    assert!(run.status.success(), "native binary failed: {run:?}");
    assert_eq!(run.stdout, b"hi\n");

    let explicit_compile = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "compile",
            "foo.rock",
            "--backend",
            "c",
            "--output",
            "build/custom",
        ])
        .current_dir(dir.path())
        .output()
        .expect("spawn explicit rock compile");
    assert!(explicit_compile.status.success(), "explicit C compile failed");
    assert!(dir.path().join("build/custom.c").is_file());
    assert!(dir.path().join("build/custom").is_file());

    std::fs::write(
        dir.path().join("foo.rock"),
        "extern(lua) fn unsupported_runtime()\nfn main() { unsupported_runtime() }\n",
    )
    .expect("write unsupported program");
    let failed_compile = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(["compile", "foo.rock", "--backend", "c"])
        .current_dir(dir.path())
        .output()
        .expect("spawn failing rock compile");
    assert!(
        !failed_compile.status.success(),
        "unsupported compile passed"
    );
    assert!(!c_path.exists(), "failed compile left stale C source");
    assert!(!binary.exists(), "failed compile left stale binary");
}

#[test]
fn c_compile_and_run_load_project_dependencies() {
    assert!(
        Command::new("clang")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success()),
        "C backend gate requires clang"
    );
    let root = tempfile::tempdir().expect("tempdir");
    let library = root.path().join("numbers");
    let game = root.path().join("game");
    std::fs::create_dir_all(&library).expect("create library");
    std::fs::create_dir_all(&game).expect("create game");
    std::fs::write(
        library.join("stick.toml"),
        "[package]\nname = \"numbers\"\ntype = \"lib\"\n",
    )
    .expect("write library manifest");
    std::fs::write(
        library.join("numbers.rock"),
        "fn project_answer() -> string { return \"answer=42\" }\n",
    )
    .expect("write library source");
    std::fs::write(
        game.join("stick.toml"),
        "[package]\nname = \"game\"\ntype = \"bin\"\n\n[dependencies]\nnumbers = { path = \"../numbers\" }\n",
    )
    .expect("write game manifest");
    std::fs::write(
        game.join("main.rock"),
        "fn main() { print(project_answer()) }\n",
    )
    .expect("write game source");

    let compile = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "compile",
            game.to_str().expect("utf8 game path"),
            "--backend",
            "c",
            "--output",
            "native-game",
        ])
        .current_dir(root.path())
        .output()
        .expect("spawn rock compile");
    assert!(
        compile.status.success(),
        "C project compile failed:\nstdout:{}\nstderr:{}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr)
    );
    let compiled_run = Command::new(root.path().join("native-game"))
        .output()
        .expect("run compiled project");
    assert!(
        compiled_run.status.success(),
        "compiled project failed: {compiled_run:?}"
    );
    assert_eq!(compiled_run.stdout, b"answer=42\n");

    let project_run = run(&game);
    assert!(
        project_run.status.success(),
        "C project run failed:\nstdout:{}\nstderr:{}",
        String::from_utf8_lossy(&project_run.stdout),
        String::from_utf8_lossy(&project_run.stderr)
    );
    assert_eq!(project_run.stdout, b"answer=42\n");
}

#[test]
fn c_run_executes_main_and_propagates_native_failure() {
    assert!(
        Command::new("clang")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success()),
        "C backend gate requires clang"
    );
    let dir = tempfile::tempdir().expect("tempdir");
    let good = dir.path().join("good.rock");
    std::fs::write(
        &good,
        r#"
        fn sum_to(n: int) -> int {
            let total = 0
            let i = 0
            while i < n {
                total += i
                i += 1
            }
            return total
        }

        fn main() {
            if sum_to(5) != 10 { let _ = 1 ./ 0 }
        }
        "#,
    )
    .expect("write good program");
    let good_run = run(&good);
    assert!(
        good_run.status.success(),
        "valid C program failed:\nstdout:{}\nstderr:{}",
        String::from_utf8_lossy(&good_run.stdout),
        String::from_utf8_lossy(&good_run.stderr)
    );

    let bad = dir.path().join("bad.rock");
    std::fs::write(&bad, "fn main() { let _ = 1 ./ 0 }\n").expect("write bad program");
    let bad_run = run(&bad);
    assert!(
        !bad_run.status.success(),
        "native failure was reported as success"
    );

    let printing = dir.path().join("printing.rock");
    std::fs::write(
        &printing,
        "fn echo(value: string) -> string { let copy = value; return copy }\n\
         fn main() {\n    let message = echo(\"hi\")\n    print(message)\n}\n",
    )
    .expect("write printing program");
    let printing_run = run(&printing);
    assert!(
        printing_run.status.success(),
        "C print program failed:\nstdout:{}\nstderr:{}",
        String::from_utf8_lossy(&printing_run.stdout),
        String::from_utf8_lossy(&printing_run.stderr)
    );
    assert_eq!(printing_run.stdout, b"hi\n");

    let unsupported = dir.path().join("unsupported.rock");
    std::fs::write(
        &unsupported,
        "extern(lua) fn unsupported_runtime()\nfn main() { unsupported_runtime() }\n",
    )
    .expect("write unsupported program");
    let unsupported_run = run(&unsupported);
    let unsupported_output = format!(
        "{}{}",
        String::from_utf8_lossy(&unsupported_run.stdout),
        String::from_utf8_lossy(&unsupported_run.stderr)
    );
    assert!(
        !unsupported_run.status.success(),
        "unsupported runtime boundary was reported as success"
    );
    assert!(
        unsupported_output.contains("C backend"),
        "unsupported boundary lost its C diagnostic:\n{unsupported_output}"
    );
}
