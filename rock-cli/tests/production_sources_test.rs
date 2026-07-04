//! Production game builds must exclude test-only files (anything under a
//! `tests/` directory: sims, @test fns). A compile error in a sim must not
//! break `rock run/stone/frame/perf <game>`. See
//! `docs/sims-out-of-game-build-plan.md`.

use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("rock-cli crate has a parent")
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
    let spec = rock_cli::unified_compile::SourceSpec::Directory(abs_path);
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

    // Fix: production_only drops every tests/ file...
    let prod = full.production_only();
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
