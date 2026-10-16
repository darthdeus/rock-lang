use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn prelude_source() -> String {
    std::fs::read_to_string(repo_root().join("stone/engine/ui/imgui_prelude.rock"))
        .expect("stone imgui prelude must exist")
}

#[test]
fn js_sibling_compiles_the_real_prelude_module() {
    let root = repo_root();
    for backend in ["skull", "js"] {
        let output = crate::command::rock_bin()
            .current_dir(&root)
            .args(["test", "stone/engine/ui/imgui_prelude.rock", "--backend", backend])
            .output()
            .expect("run prelude backend compile");
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        assert!(output.status.success(), "{backend} prelude compile failed:\n{text}");
    }
    let source = prelude_source();
    assert!(source.contains("inline(js) fn __imgui_prelude_init"));
    assert!(source.contains("return true;"));

    let tmp_root = root.join("tmp");
    std::fs::create_dir_all(&tmp_root).expect("create temp root");
    let project = tempfile::Builder::new()
        .prefix("cimgui-js-global-init-")
        .tempdir_in(&tmp_root)
        .expect("JS initializer project");
    let stone = root.join("stone");
    std::fs::write(
        project.path().join("stick.toml"),
        format!(
            "[package]\nname = \"cimgui_js_global_init\"\ntype = \"bin\"\n\n[dependencies]\nstone = {{ path = \"{}\" }}\n",
            stone.display()
        ),
    )
    .expect("write JS initializer manifest");
    std::fs::write(
        project.path().join("main.rock"),
        "global OBSERVED: bool = __IMGUI_PRELUDE_READY\nfn main() { print(OBSERVED) }\n",
    )
    .expect("write JS initializer source");
    let output = crate::command::rock_bin()
        .current_dir(&root)
        .args(["run", project.path().to_str().unwrap(), "--backend", "js"])
        .output()
        .expect("run JS initializer project");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "JS initializer project failed:\n{stdout}\n{stderr}");
    assert_eq!(stdout.trim(), "true", "JS prelude global was not ready for game code");
}
