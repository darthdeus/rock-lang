use std::path::PathBuf;
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

fn run_index(repo: &PathBuf, output: &std::path::Path) -> std::process::Output {
    Command::new(rock_bin())
        .current_dir(repo)
        .args([
            "docs-index",
            "stdlib",
            "stone",
            "--output",
            output.to_str().unwrap(),
        ])
        .output()
        .expect("run declaration index")
}

#[test]
fn declaration_index_is_deterministic_and_stamped() {
    let repo = root();
    let scratch = tempfile::tempdir_in(repo.join("tmp")).expect("declaration index scratch");
    let first = scratch.path().join("first.json");
    let second = scratch.path().join("second.json");

    let first_run = run_index(&repo, &first);
    assert!(
        first_run.status.success(),
        "first declaration index failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&first_run.stdout),
        String::from_utf8_lossy(&first_run.stderr)
    );
    let second_run = run_index(&repo, &second);
    assert!(
        second_run.status.success(),
        "second declaration index failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&second_run.stdout),
        String::from_utf8_lossy(&second_run.stderr)
    );

    assert_eq!(
        std::fs::read(&first).unwrap(),
        std::fs::read(&second).unwrap(),
        "repeated declaration-index builds must be byte-identical"
    );

    let index: serde_json::Value = serde_json::from_slice(&std::fs::read(&first).unwrap())
        .expect("declaration index JSON");
    assert_eq!(index["schema"], 1);
    let revision = String::from_utf8(
        Command::new("git")
            .current_dir(&repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("read repository revision")
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    assert_eq!(index["repository_revision"].as_str(), Some(revision.as_str()));
    assert!(index["source_identity"]
        .as_str()
        .is_some_and(|identity| identity.starts_with("sha256:")));

    let repo_text = repo.to_string_lossy();
    let sources = index["sources"].as_array().expect("source records");
    assert!(!sources.is_empty());
    assert!(sources.iter().all(|source| {
        source["path"].as_str().is_some_and(|path| {
            !path.starts_with('/') && !path.contains(repo_text.as_ref())
        }) && source["sha256"]
            .as_str()
            .is_some_and(|hash| hash.starts_with("sha256:"))
    }));

    let declarations = index["declarations"].as_array().expect("declarations");
    assert!(!declarations.is_empty());
    assert!(declarations.iter().any(|declaration| {
        declaration["kind"] == "function"
            && declaration["name"] == "asset_path"
            && declaration["source"] == "stone/engine/compat/assets.rock"
    }));
    assert!(declarations.iter().any(|declaration| {
        declaration["kind"] == "struct"
            && declaration["name"] == "Ptr"
            && declaration["source"] == "stdlib/unified.rock"
    }));
}

#[test]
fn declaration_index_preserves_parameter_defaults() {
    let repo = root();
    let scratch = tempfile::tempdir_in(repo.join("tmp")).expect("default signature scratch");
    let source = scratch.path().join("defaults.rock");
    let output = scratch.path().join("defaults.json");
    std::fs::write(&source, concat!(
        "// UTF-8 before defaults: \u{e9}\n",
        "fn sample(required: int, amount: float = 1.0, label: string = \"two  spaces, = \\\"quote\\\"\", ",
        "enabled: bool = false, value: int? = null, grouped: int = (2 + 3) * 4, ",
        "position: Vec2 = v(1, 2), values: [int] = [1, 2]) {}\n",
        "deco init marker(stage: int = 100, mode: string = \"presentation\") {}\n",
        "fn multiline(value: int = (\n    2 + // keep this comment\n    3\n)) {}\n",
    )).unwrap();
    let result = Command::new(rock_bin())
        .current_dir(&repo)
        .args(["docs-index", source.to_str().unwrap(), "--output", output.to_str().unwrap()])
        .output().expect("index default arguments");
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let index: serde_json::Value = serde_json::from_slice(&std::fs::read(output).unwrap()).unwrap();
    let declarations = index["declarations"].as_array().unwrap();
    let signature = |name: &str| declarations.iter()
        .find(|declaration| declaration["name"] == name).unwrap()["signature"].as_str().unwrap();
    assert_eq!(signature("sample"), concat!(
        "fn sample(required: int, amount: float = 1.0, label: string = \"two  spaces, = \\\"quote\\\"\", ",
        "enabled: bool = false, value: int? = null, grouped: int = (2 + 3) * 4, ",
        "position: Vec2 = v(1, 2), values: [int] = [1, 2]) -> ()",
    ));
    assert_eq!(signature("marker"), "decorator marker(stage: int = 100, mode: string = \"presentation\")");
    assert_eq!(signature("multiline"), "fn multiline(value: int = (\n    2 + // keep this comment\n    3\n)) -> ()");
}

/// Each source parses in its own package's dialect, as the compiler parses it:
/// a `modules = "v1"` package's `pub` items and a legacy sibling in one index.
#[test]
fn declaration_index_parses_each_package_in_its_dialect() {
    let repo = root();
    let scratch = tempfile::tempdir_in(repo.join("tmp")).expect("dialect scratch");
    let v1 = scratch.path().join("v1pkg");
    let legacy = scratch.path().join("legacypkg");
    for (dir, name, language, src) in [
        (&v1, "v1pkg", "[language]\nmodules = \"v1\"\n", "pub fn exported(x: int) -> int { return x }\n"),
        (&legacy, "legacypkg", "", "fn plain(x: int) -> int { return x }\n"),
    ] {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("stick.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\ntype = \"lib\"\n\n{language}\n[build]\nchannels = [\"dev\"]\ndefault_channel = \"dev\"\n"),
        )
        .unwrap();
        std::fs::write(dir.join("lib.rock"), src).unwrap();
    }
    let output = scratch.path().join("dialects.json");
    let result = Command::new(rock_bin())
        .current_dir(&repo)
        .args(["docs-index", v1.to_str().unwrap(), legacy.to_str().unwrap(), "--output", output.to_str().unwrap()])
        .output()
        .expect("index both dialects");
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let index: serde_json::Value = serde_json::from_slice(&std::fs::read(output).unwrap()).unwrap();
    let mut names: Vec<&str> = index["declarations"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|declaration| declaration["name"].as_str())
        .collect();
    names.sort();
    assert_eq!(names, vec!["exported", "plain"]);
}
