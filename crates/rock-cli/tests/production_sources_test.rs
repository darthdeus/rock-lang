//! Production game builds must exclude test-only files (anything under a
//! `tests/` directory: sims, @test fns). A compile error in a sim must not
//! break `rock run/stone/frame/perf <game>`. See
//! `docs/sim-layering.md`.

use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("rock-cli crate has a grandparent")
        .to_path_buf()
}

fn has_tests_component(path: &str) -> bool {
    std::path::Path::new(path)
        .components()
        .any(|c| c.as_os_str() == "tests")
}

#[test]
fn production_only_excludes_tests_dir_but_keeps_game_files() {
    let abs_path = workspace_root().join("mag");
    let spec = rock_cli::unified_compile::SourceSpec::Directory(abs_path.clone());
    let full = rock_cli::unified_compile::discover_sources(&spec)
        .expect("discover_sources(mag) succeeds");

    // Premise: the unfiltered discovery DOES pull in tests/sims today. If this
    // ever stops being true the fix below is testing nothing.
    let full_tests: Vec<String> = full
        .files
        .iter()
        .map(|f| f.borrow().file.to_id_path())
        .filter(|p| has_tests_component(p))
        .collect();
    assert!(
        !full_tests.is_empty(),
        "premise broken: mag full discovery has no tests/ files"
    );

    // The production view drops tests/ files below the owning package root...
    let prod = full.production_only(&abs_path);
    let leaked: Vec<String> = prod
        .files
        .iter()
        .map(|f| f.borrow().file.to_id_path())
        .filter(|p| has_tests_component(p))
        .collect();
    assert!(
        leaked.is_empty(),
        "production_only leaked test-only files into the game build: {leaked:?}"
    );

    // ...while keeping the actual game sources (mag has a main.rock).
    let kept_game = prod
        .files
        .iter()
        .any(|f| f.borrow().file.to_id_path().ends_with("main.rock"));
    assert!(
        kept_game,
        "production_only dropped game sources (no main.rock survived)"
    );
    assert!(
        prod.files.len() < full.files.len(),
        "production_only removed nothing (full={}, prod={})",
        full.files.len(),
        prod.files.len()
    );
}

#[test]
fn production_only_uses_real_package_roots_and_rejects_tagged_mismatch() {
    let outer = tempfile::tempdir().expect("create outer tempdir");
    let project = outer.path().join("tests").join("production-project");
    let tests_dir = project.join("tests");
    std::fs::create_dir_all(&tests_dir).expect("create project directories");
    std::fs::write(
        project.join("stick.toml"),
        "[package]\nname = \"production-project\"\nversion = \"0.1.0\"\ntype = \"lib\"\n",
    )
    .expect("write manifest");
    std::fs::write(project.join("main.rock"), "fn main() {}\n").expect("write production source");
    std::fs::write(tests_dir.join("broken.rock"), "fn broken() {}\n").expect("write test source");

    assert!(
        project
            .components()
            .any(|component| component.as_os_str() == "tests"),
        "the project must live below an unrelated tests component"
    );

    let spec = rock_cli::unified_compile::SourceSpec::Directory(project.clone());
    let full = rock_cli::unified_compile::discover_sources(&spec)
        .expect("discover_sources(temp project) succeeds");
    let production = full.production_only(&project);
    let production_paths: Vec<_> = production
        .files
        .iter()
        .map(|file| file.borrow().file.to_id_path())
        .collect();
    assert!(
        production_paths.iter().any(|path| path.ends_with("main.rock")),
        "the production source below a parent tests component must survive: {production_paths:?}"
    );
    assert!(
        production_paths
            .iter()
            .all(|path| !path.ends_with("tests/broken.rock")),
        "the project's own tests directory must be filtered: {production_paths:?}"
    );

    let package_id = full
        .files
        .iter()
        .find_map(|file| file.borrow().package.as_ref().map(|tag| tag.id))
        .expect("discovered package source has a package tag");
    let wrong_root = outer.path().join("tests").join("different-project");
    std::fs::create_dir_all(&wrong_root).expect("create wrong package root");
    let mut malformed = full.clone();
    malformed.package_roots = vec![(package_id, wrong_root)];
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        malformed.production_only(&project)
    }));
    assert!(
        result.is_err(),
        "a tagged source outside its package root must surface the invariant violation"
    );
}
