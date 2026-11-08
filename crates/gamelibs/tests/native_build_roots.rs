#![cfg(target_os = "linux")]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

struct Scratch(std::path::PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0) {
            if error.kind() != std::io::ErrorKind::NotFound {
                eprintln!(
                    "failed to remove test scratch {}: {error}",
                    self.0.display()
                );
            }
        }
    }
}

fn isolated_cargo_home(scratch: &Path) -> std::path::PathBuf {
    let source = std::env::var_os("CARGO_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".cargo"))
        })
        .expect("Cargo home");
    let cargo_home = scratch.join("cargo-home");
    fs::create_dir_all(&cargo_home).unwrap();
    for entry in ["registry", "git"] {
        let from = source.join(entry);
        if from.exists() {
            std::os::unix::fs::symlink(from, cargo_home.join(entry)).unwrap();
        }
    }
    let config = source.join("config.toml");
    if config.exists() {
        fs::copy(config, cargo_home.join("config.toml")).unwrap();
    }
    cargo_home
}

fn cargo_check(project: &Path, target: &Path, cargo_home: &Path) -> Output {
    Command::new(env!("CARGO"))
        .args(["check", "--offline", "-vv"])
        .current_dir(project)
        .env("CARGO_TARGET_DIR", target)
        .env("CARGO_HOME", cargo_home)
        .env(
            "ROCK_GAMELIBS_DIR",
            project.ancestors().nth(3).unwrap().join("cache"),
        )
        .output()
        .expect("run isolated Cargo check")
}

#[test]
fn native_build_script_finds_relocated_gamelibs_root() {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = crate_dir.parent().unwrap().parent().unwrap();
    let scratch = Scratch(root.join(format!("target/native-build-roots-{}", std::process::id())));
    let relocated_root = scratch.0.join("checkout");
    let project = relocated_root.join("nested/build/native-probe");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::create_dir_all(relocated_root.join("cache/gamelibs-linux-x86_64/lib")).unwrap();
    fs::write(
        project.join("Cargo.toml"),
        format!(
            "[package]\nname='relocated-native-probe'\nversion='0.1.0'\nedition='2021'\n\
[workspace]\n[build-dependencies]\ngamelibs={{path='{}'}}\n",
            crate_dir.display(),
        ),
    )
    .unwrap();
    fs::write(
        project.join("build.rs"),
        "fn main() { gamelibs::link_search_and_rpath(); }",
    )
    .unwrap();
    fs::write(project.join("src/main.rs"), "fn main() {}\n").unwrap();

    let target = scratch.0.join("target");
    let cargo_home = isolated_cargo_home(&scratch.0);
    let output = cargo_check(&project, &target, &cargo_home);
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "isolated build failed:\n{log}");

    let expected_lib = relocated_root.join("cache/gamelibs-linux-x86_64/lib");
    assert!(
        log.contains(&expected_lib.display().to_string()),
        "runner did not use the relocated gamelibs library directory {}:\n{log}",
        expected_lib.display()
    );
    let build_output = fs::read_dir(target.join("debug/build"))
        .unwrap()
        .map(|entry| entry.expect("Cargo build directory entry"))
        .find(|entry| {
            // Cargo gives the executable and run-output directories the same prefix.
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("relocated-native-probe-")
                && entry.path().join("output").is_file()
        })
        .map(|entry| fs::read_to_string(entry.path().join("output")).unwrap())
        .expect("Cargo build-script output");
    assert!(
        build_output.contains(&format!(
            "cargo:rerun-if-changed={}",
            project.join("Cargo.toml").display()
        )),
        "runner did not invalidate cached paths when its manifest moves:\n{build_output}"
    );
    // Removing an immutable installation must invalidate cached build-script output.
    fs::remove_dir_all(relocated_root.join("cache/gamelibs-linux-x86_64")).unwrap();
    let missing = cargo_check(&project, &target, &cargo_home);
    let missing_log = format!(
        "{}{}",
        String::from_utf8_lossy(&missing.stdout),
        String::from_utf8_lossy(&missing.stderr)
    );
    assert!(
        !missing.status.success(),
        "cached build ignored the removed installation"
    );
    assert!(
        missing_log.contains("just sync-gamelibs"),
        "missing-cache remedy absent: {missing_log}"
    );
    assert!(
        missing_log.contains(
            &relocated_root
                .join("cache/gamelibs-linux-x86_64")
                .display()
                .to_string()
        ),
        "missing-cache path absent: {missing_log}"
    );
}
