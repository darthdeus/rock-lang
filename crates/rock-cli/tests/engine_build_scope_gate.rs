//! Proves that core engine compilation is scoped to `stone-rs` rather than
//! building the entire workspace.
//!
//! We choose a real-mechanism gate in the style of `engine_cdylib_freshness_gate`:
//! after a staged engine build via `build_libstone`, it asserts that an artifact
//! for a crate `stone-rs` does not depend on (`rock-cli`) is absent in `target/debug/`.
//! This proves real Cargo and filesystem behavior rather than asserting syntax or
//! arguments by construction.
//!
//! `#[ignore]`d for the same reason `engine_cdylib_freshness_gate` is: it removes
//! and rebuilds shared workspace artifacts in `target/debug/` and `tmp/`. Run
//! deliberately:
//!
//!   cargo test -p rock-cli --test rock_cli_heavy -- --ignored --nocapture engine_build_scope

use std::path::{Path, PathBuf};
use std::process::Command;

fn isolated_test(name: &str) -> bool {
    if std::env::var("ROCK_ENGINE_GATE_CHILD").as_deref() == Ok(name) {
        return false;
    }
    static GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let guard = GATE.lock().expect("engine gate lock");
    let result = Command::new(std::env::current_exe().expect("test executable"))
        .args([&format!("engine_build_scope_gate::{name}"), "--exact", "--ignored", "--nocapture"])
        .env("ROCK_ENGINE_GATE_CHILD", name)
        .output().expect("isolated engine gate");
    drop(guard);
    assert!(result.status.success(), "{}{}",
        String::from_utf8_lossy(&result.stdout), String::from_utf8_lossy(&result.stderr));
    true
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

/// A project root whose `tmp/` is private and whose every other top-level
/// entry links to the real workspace. `build_libstone` stages into
/// `<ROCK_DIR>/tmp`, and `tools/test-desktop.sh full` pins the sha1 of the
/// real `<root>/tmp/libstone.*` in an engine build receipt for the whole run,
/// so a test that deletes or restages that file fails every later heavy test.
/// Cargo still builds the real sources into the real `target/`.
///
/// Sets `ROCK_DIR`, which `rock_dir()` caches on first use: call it before any
/// production call, once per test process (nextest runs one test per process).
pub(crate) fn private_engine_root(label: &str) -> PathBuf {
    let real = workspace_root();
    let home = std::env::var_os("HOME").expect("HOME");
    let private = PathBuf::from(home).join("tmp").join(format!(
        "rock-engine-gate-root-{label}-{}", std::process::id()
    ));
    if private.exists() {
        fs_err::remove_dir_all(&private).expect("clear previous private root");
    }
    fs_err::create_dir_all(private.join("tmp")).expect("create private root");
    for entry in fs_err::read_dir(&real).expect("list workspace root") {
        let name = entry.expect("workspace entry").file_name();
        if matches!(name.to_str(), Some("tmp" | ".git")) {
            continue;
        }
        std::os::unix::fs::symlink(real.join(&name), private.join(&name))
            .expect("link workspace entry into private root");
    }
    unsafe { std::env::set_var("ROCK_DIR", &private) };
    private
}

fn cdylib_name(stem: &str) -> String {
    #[cfg(target_os = "windows")]
    {
        format!("{stem}.dll")
    }
    #[cfg(target_os = "macos")]
    {
        format!("lib{stem}.dylib")
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        format!("lib{stem}.so")
    }
}

fn cargo_target(root: &Path) -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR").filter(|path| !path.is_empty())
        .map(PathBuf::from).map(|path| root.join(path))
        .unwrap_or_else(|| root.join("target"))
}

#[test]
#[ignore = "deletes shared workspace artifacts and rebuilds a crate; run deliberately"]
fn core_engine_build_does_not_compile_unrelated_workspace_crates() {
    if isolated_test("core_engine_build_does_not_compile_unrelated_workspace_crates") { return; }
    let root = private_engine_root("scope");

    // The unrelated crate artifact that stone-rs does NOT depend on.
    // When cargo build --lib runs with no -p, rock-cli's lib target is compiled
    // and produces target/debug/librock_cli.rlib. When scoped with -p stone-rs,
    // cargo only builds stone-rs and its dependencies, leaving this artifact absent.
    let target = cargo_target(&root);
    let unrelated_artifact = target.join("debug/librock_cli.rlib");
    if unrelated_artifact.exists() {
        fs_err::remove_file(&unrelated_artifact).expect("remove unrelated artifact before test");
    }
    assert!(
        !unrelated_artifact.exists(),
        "fixture starts without unrelated artifact: {}",
        unrelated_artifact.display()
    );

    // Also clear staged libstone so we verify the staged artifact is actually produced
    let staged_stone = root.join("tmp").join(cdylib_name("stone"));
    if staged_stone.exists() {
        fs_err::remove_file(&staged_stone).expect("remove staged libstone");
    }

    // Ensure no receipt in the environment bypasses the build
    let prev_receipt = std::env::var_os("ROCK_ENGINE_BUILD_RECEIPT");
    unsafe {
        std::env::remove_var("ROCK_ENGINE_BUILD_RECEIPT");
    }

    let result = rock_host::engine_build::build_libstone();

    // Restore env
    if let Some(val) = prev_receipt {
        unsafe {
            std::env::set_var("ROCK_ENGINE_BUILD_RECEIPT", val);
        }
    }

    result.expect("build_libstone should succeed");

    assert!(
        staged_stone.exists(),
        "build_libstone must produce staged libstone at {}",
        staged_stone.display()
    );

    assert!(
        !unrelated_artifact.exists(),
        "build_libstone built unrelated crate artifact {}, proving cargo was not package-scoped",
        unrelated_artifact.display()
    );
    fs_err::remove_dir_all(&root).expect("remove private engine root");
}

#[test]
#[ignore = "builds stone-rs without default features; run deliberately"]
fn c_libstone_stages_under_its_own_name_and_leaves_the_engine_library() {
    if isolated_test("c_libstone_stages_under_its_own_name_and_leaves_the_engine_library") { return; }
    let root = private_engine_root("c-libstone");
    let engine_lib = root.join("tmp").join(cdylib_name("stone"));
    let sentinel = b"engine libstone sentinel: the C build must not touch this";
    fs_err::write(&engine_lib, sentinel).expect("write sentinel engine libstone");
    unsafe { std::env::remove_var("ROCK_ENGINE_BUILD_RECEIPT") };

    rock_host::engine_build::build_libstone_for_c().expect("build C libstone");

    assert_eq!(
        fs_err::read(&engine_lib).expect("read engine libstone"),
        sentinel,
        "build_libstone_for_c overwrote the Lua engine's {}",
        engine_lib.display()
    );
    let c_lib = root.join("tmp").join(cdylib_name("stone_c"));
    assert!(c_lib.is_file(), "C libstone not staged at {}", c_lib.display());
    fs_err::remove_dir_all(&root).expect("remove private engine root");
}

#[test]
#[ignore = "builds and stages real Stone providers; run deliberately"]
fn receipt_bound_c_consumers_preserve_and_validate_the_engine_library() {
    if isolated_test("receipt_bound_c_consumers_preserve_and_validate_the_engine_library") { return; }
    let root = private_engine_root("receipt-c-consumers");
    let fixture = tempfile::tempdir_in(root.join("tmp")).expect("C fixture directory");
    let receipt = fixture.path().join("receipt.json");
    let prepared = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(["engine-build-receipt", "--output"])
        .arg(&receipt)
        .current_dir(&root)
        .env_remove("ROCK_ENGINE_BUILD_RECEIPT")
        .output().expect("prepare engine receipt");
    assert!(prepared.status.success(), "{}{}",
        String::from_utf8_lossy(&prepared.stdout), String::from_utf8_lossy(&prepared.stderr));

    let source = fixture.path().join("stone/engine/ui/imgui.rock");
    fs_err::create_dir_all(source.parent().unwrap()).expect("ImGui fixture parent");
    fs_err::write(&source, r#"
@inline inline(lua) fn imgui_set_tooltip(text: string) = "imgui.SetTooltip(text)"
fn main() {
    imgui_set_tooltip("receipt % 100")
    print("receipt-imgui-ok")
}
"#).expect("ImGui fixture");
    let binary = fixture.path().join("imgui-consumer");
    let project = fixture.path().join("project");
    fs_err::create_dir_all(project.join("tests")).expect("asset fixture parent");
    let stone = workspace_root().join("stone");
    fs_err::write(project.join("stick.toml"), format!(
        "[package]\nname = \"receipt-assets\"\ntype = \"lib\"\n\
         [language]\nmodules = \"v1\"\n[dependencies]\nstone = {{ path = \"{}\" }}\n\
         [build.vars]\nGIT_SHA = \"test\"\n", stone.display()
    )).expect("asset fixture manifest");
    fs_err::write(project.join("tests/assets.rock"), format!(
        "use stone::project_asset_path;\n@test fn engine_asset_root() {{\n\
         assert_eq(project_asset_path(\"@engine/compat/assets.rock\"), \"{}\")\n}}\n",
        stone.join("engine/compat/assets.rock").display()
    )).expect("asset fixture source");

    let engine = root.join("tmp").join(cdylib_name("stone"));
    let bytes = fs_err::read(&engine).expect("staged engine bytes");
    let mtime = fs_err::metadata(&engine).unwrap().modified().unwrap();
    let commands = [
        vec!["compile".into(), source.into_os_string(), "--backend".into(), "c".into(),
            "--output".into(), binary.clone().into_os_string()],
        vec!["test".into(), project.join("tests").into_os_string(), "--backend".into(), "skull".into()],
        vec!["test".into(), project.join("tests").into_os_string(), "--backend".into(), "c".into()],
    ];
    for args in &commands {
        let result = Command::new(env!("CARGO_BIN_EXE_rock"))
            .args(args).current_dir(&root)
            .env("ROCK_ENGINE_BUILD_RECEIPT", &receipt)
            .env("ROCK_TEST_VERBOSE", "1")
            .env_remove("ROCK_SKIP_ENGINE_BUILD")
            .env_remove("STONE_SOURCE_DIR").env_remove("__ROCK_STONE_DIR")
            .env_remove("GAME_SOURCE_DIR")
            .output().expect("receipt-bound consumer");
        assert!(result.status.success(), "{args:?}: {}{}",
            String::from_utf8_lossy(&result.stdout), String::from_utf8_lossy(&result.stderr));
        if args[0] == "test" {
            assert!(String::from_utf8_lossy(&result.stdout).contains("1/1 @tests passed"));
        }
        assert!(fs_err::read(&engine).unwrap() == bytes, "{args:?} changed engine bytes");
        assert_eq!(fs_err::metadata(&engine).unwrap().modified().unwrap(), mtime,
            "{args:?} restaged engine libstone");
    }
    let run = Command::new(&binary).output().expect("run C ImGui consumer");
    assert!(run.status.success(), "{}", String::from_utf8_lossy(&run.stderr));
    assert_eq!(run.stdout, b"receipt-imgui-ok\n");
    assert!(root.join("tmp").join(cdylib_name("stone_c")).is_file());

    // A receipt is also an integrity boundary for consumers that link stone_c.
    fs_err::write(&engine, b"replaced staged engine").expect("invalidate engine receipt");
    for args in &commands {
        let result = Command::new(env!("CARGO_BIN_EXE_rock"))
            .args(args).current_dir(&root)
            .env("ROCK_ENGINE_BUILD_RECEIPT", &receipt)
            .env("ROCK_TEST_VERBOSE", "1")
            .env_remove("ROCK_SKIP_ENGINE_BUILD")
            .output().expect("consumer with invalid receipt");
        let text = format!("{}{}", String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr));
        assert!(!result.status.success() && text.contains("its staged library changed"),
            "{args:?} accepted a replaced receipt-bound engine library: {text}");
        assert_eq!(fs_err::read(&engine).unwrap(), b"replaced staged engine");
    }
    fs_err::remove_dir_all(&root).expect("remove private engine root");
}

#[test]
#[ignore = "builds and stages the real engine library; run deliberately"]
fn receipt_bound_raw_staging_preserves_the_engine_library() {
    if isolated_test("receipt_bound_raw_staging_preserves_the_engine_library") { return; }
    let root = private_engine_root("receipt-raw-stage");
    let receipt = root.join("tmp/receipt.json");
    unsafe { std::env::remove_var("ROCK_ENGINE_BUILD_RECEIPT") };
    rock_host::engine_build::prepare_engine_build_receipt(&[], &receipt)
        .expect("prepare engine receipt");
    let engine = root.join("tmp").join(cdylib_name("stone"));
    let bytes = fs_err::read(&engine).expect("staged engine bytes");
    let mtime = fs_err::metadata(&engine).unwrap().modified().unwrap();
    unsafe { std::env::set_var("ROCK_ENGINE_BUILD_RECEIPT", &receipt) };
    rock_host::engine_build::stage_cdylib("stone_rs", "stone").expect("validate staged engine");
    assert!(fs_err::read(&engine).unwrap() == bytes, "stage_cdylib changed engine bytes");
    assert_eq!(fs_err::metadata(&engine).unwrap().modified().unwrap(), mtime,
        "stage_cdylib restaged receipt-bound engine libstone");

    // A later C build's dep-info must not redefine the already owned engine.
    let target = cargo_target(&root);
    let dep_info = target.join("debug/libstone_rs.d");
    let saved_dep_info = fs_err::read(&dep_info).expect("Cargo dep-info");
    let foreign_input = root.join("tmp/foreign-provider-input");
    fs_err::write(&foreign_input, b"foreign provider input").expect("foreign input");
    fs_err::write(&dep_info, format!("provider: {}\n", foreign_input.display()))
        .expect("replace dep-info with foreign provider inputs");
    let reissued = root.join("tmp/reissued.json");
    let result = rock_host::engine_build::prepare_engine_build_receipt(&[], &reissued);
    fs_err::write(&dep_info, saved_dep_info).expect("restore exact Cargo dep-info");
    result.expect("reissue bound receipt");
    let original: serde_json::Value = serde_json::from_slice(&fs_err::read(&receipt).unwrap()).unwrap();
    let current: serde_json::Value = serde_json::from_slice(&fs_err::read(&reissued).unwrap()).unwrap();
    assert_eq!(current["libs"], original["libs"],
        "receipt reissue replaced owned input provenance with foreign Cargo dep-info");
    assert!(fs_err::read(&engine).unwrap() == bytes);
    assert_eq!(fs_err::metadata(&engine).unwrap().modified().unwrap(), mtime);

    fs_err::write(&engine, b"replaced staged engine").expect("invalidate receipt");
    let error = rock_host::engine_build::stage_cdylib("stone_rs", "stone")
        .expect_err("raw staging must reject a changed receipt-bound library");
    assert!(error.to_string().contains("its staged library changed"), "{error:#}");
    assert_eq!(fs_err::read(&engine).unwrap(), b"replaced staged engine");
    fs_err::remove_dir_all(&root).expect("remove private engine root");
}
