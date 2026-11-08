#![cfg(target_os = "linux")]

use std::fs;
use std::path::Path;
use std::process::Command;

fn build(project: &Path, target: &Path) -> std::process::Output {
    Command::new(env!("CARGO"))
        .args(["build", "--offline"])
        .current_dir(project)
        .env("CARGO_TARGET_DIR", target)
        .output()
        .expect("run isolated Cargo build")
}

#[test]
fn shared_target_relinks_against_the_current_worktree() {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = crate_dir.parent().unwrap().parent().unwrap();
    let scratch = root.join(format!("target/gamelibs-relocation-{}", std::process::id()));
    fs::create_dir_all(&scratch).unwrap();
    let first = scratch.join("first");
    fs::create_dir_all(first.join("src")).unwrap();
    fs::write(
        first.join("Cargo.toml"),
        format!(
            "[package]\nname='gamelibs-relocation-probe'\nversion='0.1.0'\nedition='2021'\n\
         [workspace]\n[build-dependencies]\ngamelibs={{path='{}'}}\n",
            crate_dir.display(),
        ),
    )
    .unwrap();
    fs::write(
        first.join("build.rs"),
        "fn main() { gamelibs::link_search_and_rpath(); }\n",
    )
    .unwrap();
    let source = "#[link(name=\"SDL2\")] extern \"C\" { fn SDL_GetRevision() -> *const std::ffi::c_char; }\n\
                  fn main() { println!(\"{:?}\", unsafe { SDL_GetRevision() }); }\n";
    fs::write(first.join("src/main.rs"), source).unwrap();
    let target = scratch.join("shared-target");
    let initial = build(&first, &target);
    assert!(
        initial.status.success(),
        "initial link: {}",
        String::from_utf8_lossy(&initial.stderr)
    );
    let second = scratch.join("second");
    fs::rename(&first, &second).unwrap();
    // Force a new link while keeping the build script and its inputs unchanged.
    fs::write(second.join("src/main.rs"), format!("{source}\n")).unwrap();
    let relocated = build(&second, &target);
    let stderr = String::from_utf8_lossy(&relocated.stderr).into_owned();
    fs::remove_dir_all(&scratch).unwrap();
    assert!(relocated.status.success(), "relocated link: {stderr}");
}
