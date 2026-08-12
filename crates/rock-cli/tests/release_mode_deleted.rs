//! Rock has no compile-mode `--release`. The global flag is rejected by clap,
//! and the emitted program carries no `__ROCK_RELEASE` runtime global -- there
//! is nothing for a shipped build to branch on.
//!
//! Cargo's own `--release`, the `fire-release` profile and `[release].assets`
//! in stick.toml are unrelated and stay.

use std::process::Command;

fn rock_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

fn tmp(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rock_release_deleted_{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn the_global_release_flag_is_rejected() {
    let dir = tmp("flag");
    let src = dir.join("main.rock");
    std::fs::write(&src, "fn main() { print(\"ok\") }\n").unwrap();

    // Control: the same invocation without the flag succeeds, so a failure
    // below is the flag and not the fixture.
    let ok = Command::new(rock_bin()).arg("run").arg(&src).output().expect("spawn rock");
    assert!(
        ok.status.success(),
        "control run failed: {}",
        String::from_utf8_lossy(&ok.stderr)
    );

    let out = Command::new(rock_bin())
        .arg("run")
        .arg(&src)
        .arg("--release")
        .output()
        .expect("spawn rock");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "rock accepted --release: {stderr}");
    assert!(
        stderr.contains("unexpected argument") || stderr.contains("--release"),
        "expected an unknown-argument rejection naming --release, got: {stderr}"
    );
}

#[test]
fn emitted_programs_carry_no_release_global() {
    let dir = tmp("emit");
    let src = dir.join("main.rock");
    let out_lua = dir.join("main.lua");
    std::fs::write(
        &src,
        "fn main() {\n    let b = native_buffer_int(4)\n    b[0] = 1\n    print(b[0])\n}\n",
    )
    .unwrap();

    let out = Command::new(rock_bin())
        .arg("compile")
        .arg(&src)
        .arg("--output")
        .arg(&out_lua)
        .output()
        .expect("spawn rock");
    assert!(
        out.status.success(),
        "compile failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let lua = std::fs::read_to_string(&out_lua).expect("emitted lua");
    // Denominator: the emission really happened and holds the program.
    assert!(lua.contains("native_buffer_int"), "emitted lua does not hold the program");
    assert!(
        !lua.contains("__ROCK_RELEASE"),
        "emitted program still initializes __ROCK_RELEASE"
    );
    assert!(
        !lua.contains("is_release_mode"),
        "emitted program still references is_release_mode"
    );
}
