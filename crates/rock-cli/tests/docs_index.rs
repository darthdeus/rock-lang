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
