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
fn c_tool_imgui_bindings_link_and_run_against_actual_stone_artifact() {
    let root = workspace_root();
    let dir = tempfile::tempdir().expect("tool imgui fixture directory");
    let source_path = dir.path().join("stone/engine/input/tool.rock");
    std::fs::create_dir_all(source_path.parent().expect("tool fixture parent"))
        .expect("create tool fixture parent");
    let mut source = std::fs::read_to_string(root.join("stone/engine/input/tool.rock"))
        .expect("read authoritative tool source");
    source.push_str(
        r#"

fn stone_diag_set_s(key: string, value: string) {}

fn main() {
    let id = __tool_imgui_key_id("space")
    if id != -2 { let _ = 1 ./ 0 }
    if __tool_imgui_key_down_id(42) { let _ = 1 ./ 0 }
    if __tool_imgui_key_pressed_id(42) { let _ = 1 ./ 0 }
    if __tool_imgui_ctrl_down() { let _ = 1 ./ 0 }
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
        "linked C tool consumer failed:\nstdout:{}\nstderr:{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(run.stdout, b"tool-imgui-ok\n");
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
