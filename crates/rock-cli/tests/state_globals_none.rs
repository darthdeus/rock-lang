//! Package-scoped `globals = "none"` enforcement (state ownership plan R5).
//!
//! The package policy belongs to the package that owns a source file. A
//! dependency therefore keeps its own policy while the root is compiled, and
//! declaration annotations cannot turn a mutable global back on.

use std::path::{Path, PathBuf};
use std::process::Command;

fn rock_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

fn manifest(name: &str, globals: Option<&str>, dependencies: Option<&str>) -> String {
    let language = match globals {
        Some(value) => format!("\n[language]\nmodules = \"v1\"\nglobals = \"{value}\"\n"),
        None => "\n[language]\nmodules = \"v1\"\n".to_string(),
    };
    let dependencies = dependencies.unwrap_or("");
    format!(
        "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\ntype = \"bin\"\n{language}\n[dependencies]\n{dependencies}\n[build]\nchannels = [\"dev\"]\ndefault_channel = \"dev\"\n"
    )
}

fn write_file(root: &Path, path: &str, contents: &str) {
    let path = root.join(path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("fixture parent");
    }
    std::fs::write(path, contents).expect("fixture file");
}

fn compile(root: &Path) -> Result<String, String> {
    let out = root.join("out.lua");
    let result = Command::new(rock_bin())
        .arg("compile")
        .arg(root)
        .arg("--output")
        .arg(&out)
        .output()
        .expect("run rock compile");
    let mut output = String::from_utf8_lossy(&result.stdout).into_owned();
    output.push_str(&String::from_utf8_lossy(&result.stderr));
    if result.status.success() {
        Ok(output)
    } else {
        Err(output)
    }
}

#[test]
fn unknown_globals_value_is_rejected_by_manifest_parse() {
    let root = tempfile::tempdir().expect("tempdir");
    write_file(
        root.path(),
        "stick.toml",
        &manifest("globals-bad", Some("maybe"), None),
    );
    write_file(root.path(), "main.rock", "fn main() {}\n");

    let err = compile(root.path()).expect_err("unknown globals value must reject");
    assert!(
        err.contains("globals") && err.contains("maybe"),
        "manifest diagnostic must name the key and value:\n{err}"
    );
}

#[test]
fn mutable_global_is_rejected_even_with_state_override() {
    let root = tempfile::tempdir().expect("tempdir");
    write_file(
        root.path(),
        "stick.toml",
        &manifest("globals-none-state", Some("none"), None),
    );
    write_file(
        root.path(),
        "main.rock",
        "@state(ownership=session)\nglobal COUNT: int = 0\nfn main() { COUNT = COUNT + 1 }\n",
    );

    let err = compile(root.path()).expect_err("@state must not escape globals = none");
    assert!(
        err.contains("COUNT") && err.contains("globals = \"none\""),
        "diagnostic must name the declaration and owning policy:\n{err}"
    );
}

#[test]
fn the_state_override_fixture_is_valid_without_the_package_boundary() {
    let root = tempfile::tempdir().expect("tempdir");
    write_file(root.path(), "stick.toml", &manifest("globals-state-control", None, None));
    write_file(
        root.path(),
        "main.rock",
        "@state(ownership=session)\nglobal COUNT: int = 0\nfn main() { COUNT = COUNT + 1 }\n",
    );

    compile(root.path()).expect("the negative fixture must be valid before the package rule");
}

#[test]
fn mutable_global_is_rejected_even_with_memory_header_override() {
    let root = tempfile::tempdir().expect("tempdir");
    write_file(
        root.path(),
        "stick.toml",
        &manifest("globals-none-memory", Some("none"), None),
    );
    write_file(
        root.path(),
        "main.rock",
        "@memory(platform)\nglobal COUNT: int = 0\nfn main() { COUNT = COUNT + 1 }\n",
    );

    let err = compile(root.path()).expect_err("@memory must not escape globals = none");
    assert!(
        err.contains("COUNT") && err.contains("globals = \"none\""),
        "diagnostic must name the declaration and owning policy:\n{err}"
    );
}

#[test]
fn const_global_is_accepted_in_a_globals_none_package() {
    let root = tempfile::tempdir().expect("tempdir");
    write_file(
        root.path(),
        "stick.toml",
        &manifest("globals-none-const", Some("none"), None),
    );
    write_file(
        root.path(),
        "main.rock",
        "const LIMIT: int = 7\nfn main() { print(\"{LIMIT}\") }\n",
    );

    compile(root.path()).expect("const globals are the allowed declaration");
}

#[test]
fn static_local_is_rejected_in_a_globals_none_package() {
    let root = tempfile::tempdir().expect("tempdir");
    write_file(
        root.path(),
        "stick.toml",
        &manifest("globals-none-static", Some("none"), None),
    );
    write_file(
        root.path(),
        "main.rock",
        "fn main() { static COUNT: int = 0 COUNT = COUNT + 1 }\n",
    );

    let err = compile(root.path()).expect_err("mutable static storage must obey globals = none");
    assert!(
        err.contains("COUNT") && err.contains("globals = \"none\""),
        "diagnostic must name the declaration and owning policy:\n{err}"
    );
}

#[test]
fn the_static_fixture_is_valid_without_the_package_boundary() {
    let root = tempfile::tempdir().expect("tempdir");
    write_file(root.path(), "stick.toml", &manifest("globals-static-control", None, None));
    write_file(
        root.path(),
        "main.rock",
        "fn main() { static COUNT: int = 0 COUNT = COUNT + 1 }\n",
    );

    compile(root.path()).expect("the negative fixture must be valid before the package rule");
}

#[test]
fn each_package_applies_its_own_globals_policy() {
    let root = tempfile::tempdir().expect("tempdir");
    write_file(
        root.path(),
        "stick.toml",
        &manifest(
            "globals-root",
            Some("none"),
            Some("provider = { path = \"provider\", allow_legacy = true }"),
        ),
    );
    write_file(
        root.path(),
        "main.rock",
        "const ROOT_LIMIT: int = 7\nfn main() { print(\"{ROOT_LIMIT}\") }\n",
    );
    write_file(
        root.path(),
        "provider/stick.toml",
        &manifest("provider", None, None),
    );
    write_file(
        root.path(),
        "provider/lib.rock",
        "global PROVIDER_COUNT: int = 0\nfn provider_tick() { PROVIDER_COUNT = PROVIDER_COUNT + 1 }\n",
    );

    compile(root.path()).expect("a default-policy dependency may still own mutable globals");
}

#[test]
fn a_dependency_globals_none_policy_is_checked_when_root_compiles() {
    let root = tempfile::tempdir().expect("tempdir");
    write_file(
        root.path(),
        "stick.toml",
        &manifest(
            "globals-root-default",
            None,
            Some("provider = { path = \"provider\", allow_legacy = true }"),
        ),
    );
    write_file(root.path(), "main.rock", "fn main() {}\n");
    write_file(
        root.path(),
        "provider/stick.toml",
        &manifest("provider", Some("none"), None),
    );
    write_file(
        root.path(),
        "provider/lib.rock",
        "global PROVIDER_COUNT: int = 0\nfn provider_tick() { PROVIDER_COUNT = PROVIDER_COUNT + 1 }\n",
    );

    let err = compile(root.path()).expect_err("dependency policy must be enforced");
    assert!(
        err.contains("PROVIDER_COUNT") && err.contains("globals = \"none\""),
        "dependency diagnostic must identify its own declaration and policy:\n{err}"
    );
}
