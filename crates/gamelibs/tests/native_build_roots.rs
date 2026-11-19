#![cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64"),
))]

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

fn cargo_check(project: &Path, target: &Path, cargo_home: &Path, cache: &Path) -> Output {
    Command::new(env!("CARGO"))
        .args(["check", "--offline", "-vv"])
        .current_dir(project)
        .env("CARGO_TARGET_DIR", target)
        .env("CARGO_HOME", cargo_home)
        .env("ROCK_GAMELIBS_DIR", cache)
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
    let cache = relocated_root.join("cache");
    let bundle = cache.join(if cfg!(target_os = "macos") {
        "gamelibs-macos-arm64"
    } else {
        "gamelibs-linux-x86_64"
    });
    let lib = bundle.join("lib");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::create_dir_all(&lib).unwrap();
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
    let output = cargo_check(&project, &target, &cargo_home, &cache);
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "isolated build failed:\n{log}");

    assert!(
        log.contains(&format!("cargo:rustc-link-search=native={}", lib.display())),
        "runner did not use the relocated gamelibs library directory {}:\n{log}",
        lib.display()
    );
    assert!(
        log.contains(&format!(
            "cargo:rerun-if-changed={}",
            project.join("Cargo.toml").display()
        )),
        "runner did not invalidate cached paths when its manifest moves:\n{log}"
    );
    // Removing an immutable installation must invalidate cached build-script output.
    fs::remove_dir_all(&bundle).unwrap();
    let missing = cargo_check(&project, &target, &cargo_home, &cache);
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
        missing_log.contains("set ROCK_GAMELIBS_DIR"),
        "missing-cache remedy absent: {missing_log}"
    );
    assert!(
        missing_log.contains(&bundle.display().to_string()),
        "missing-cache path absent: {missing_log}"
    );
}
