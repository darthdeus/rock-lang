use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn rock_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

fn toml_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"")
}

fn project_fixture(name: &str, main: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
    std::fs::create_dir_all(root().join("tmp")).unwrap();
    let scratch = tempfile::Builder::new().prefix(name).tempdir_in(root().join("tmp")).unwrap();
    let project = scratch.path().join("game");
    let overlay = scratch.path().join("overlay");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(overlay.join("assets/sfx")).unwrap();
    std::fs::write(project.join("main.rock"), main).unwrap();
    std::fs::write(
        project.join("stick.toml"),
        format!(
            "[package]\nname = \"overlay-default-test\"\ntype = \"bin\"\n\
             [dependencies]\nstone = {{ path = \"{}\" }}\n\
             [build.vars]\nGIT_SHA = \"test\"\n\
             [assets]\noverlays = [\"{}\"]\n\
             [release]\nassets = [\"assets\"]\n",
            toml_path(&root().join("stone")),
            toml_path(&overlay),
        ),
    )
    .unwrap();
    (scratch, project, overlay)
}

#[test]
fn stick_asset_overlays_feed_native_preamble() {
    let (_scratch, project, overlay) = project_fixture("overlay-default-", "fn main() {}\n");
    std::fs::write(overlay.join("assets/sfx/configured.txt"), "configured").unwrap();
    let lua = project.join("main.lua");
    let compile = Command::new(rock_bin())
        .current_dir(root())
        .args(["compile", project.to_str().unwrap(), "--output", lua.to_str().unwrap()])
        .output()
        .expect("compile configured-overlay fixture");
    assert!(
        compile.status.success(),
        "compile failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr)
    );
    let emitted = std::fs::read_to_string(&lua).unwrap();
    assert!(
        emitted.contains("__ROCK_ASSET_OVERLAY_DEFAULTS")
            && emitted.contains(&overlay.to_string_lossy().replace('\\', "\\\\")),
        "native preamble omitted [assets].overlays:\n{}",
        emitted.lines().take(8).collect::<Vec<_>>().join("\n")
    );

}

#[test]
fn native_preamble_clears_removed_stick_overlay_defaults() {
    let (_scratch, project, _overlay) = project_fixture("overlay-default-clear-", "fn main() {}\n");
    std::fs::write(
        project.join("stick.toml"),
        format!(
            "[package]\nname = \"overlay-default-clear\"\ntype = \"bin\"\n\
             [dependencies]\nstone = {{ path = \"{}\" }}\n\
             [build.vars]\nGIT_SHA = \"test\"\n\
             [release]\nassets = [\"assets\"]\n",
            toml_path(&root().join("stone")),
        ),
    )
    .unwrap();
    let lua = project.join("main.lua");
    let compile = Command::new(rock_bin())
        .current_dir(root())
        .args(["compile", project.to_str().unwrap(), "--output", lua.to_str().unwrap()])
        .output()
        .expect("compile fixture without configured overlays");
    assert!(compile.status.success(), "compile failed:\n{}", String::from_utf8_lossy(&compile.stderr));
    let emitted = std::fs::read_to_string(lua).unwrap();
    assert!(
        emitted.contains("__ROCK_ASSET_OVERLAY_DEFAULTS = {}"),
        "a reload must actively clear defaults removed from stick.toml"
    );
}
