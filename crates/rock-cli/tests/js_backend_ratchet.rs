//! Anti-rot ratchet for the `--backend js` test executor
//! (`docs/multi-backend-testing-plan.md` T1): pins a floor on the number of
//! suite `@test`s currently RUNNABLE (emitted + executed green under node)
//! over a small fixed corpus, so an emitter regression that silently shrinks
//! JS coverage goes red. Emitter growth raises the floor.
//!
//! Deliberately NOT a full-suite run (default nextest profile stays fast);
//! the corpus is `tests/intrinsics/` plus three suite files verified to
//! contain at least one node-runnable test each.

fn node_available() -> bool {
    std::process::Command::new("node")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn corpus() -> Vec<String> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root");
    [
        "tests/intrinsics",
        "tests/generics_advanced.rock",
        "tests/type_holes_essential.rock",
        "tests/type_inference_core.rock",
    ]
    .iter()
    .map(|p| root.join(p).to_string_lossy().into_owned())
    .collect()
}

#[test]
fn js_runnable_coverage_ratchet() {
    if !node_available() {
        eprintln!("SKIP js_runnable_coverage_ratchet: `node` not on PATH");
        return;
    }
    let summary = rock_cli::js_test::js_test_run(&corpus(), false).expect("js test run");

    assert_eq!(summary.unit_failures, 0, "no unit should fail to build/run");
    assert_eq!(summary.tests_failed, 0, "no runnable test should FAIL under node");

    // The ratchet floor: node-executed green tests over this corpus. RAISE this
    // when emitter growth (T2+) makes more tests runnable; a drop below it is a
    // silent-coverage regression. T2 (strings + print + number formatting)
    // raised it from 4 to 55.
    const FLOOR: usize = 55;
    assert!(
        summary.tests_passed >= FLOOR,
        "JS-runnable coverage regressed: {} passed < floor {}. Skip histogram:\n{}",
        summary.tests_passed,
        FLOOR,
        summary
            .skip_reasons
            .iter()
            .map(|(r, c)| format!("  {c:>4}  {r}"))
            .collect::<Vec<_>>()
            .join("\n")
    );

    // The corpus must keep exercising the SKIP path too (an empty histogram
    // here would mean the corpus no longer covers unmapped surface).
    assert!(
        !summary.skip_reasons.is_empty(),
        "corpus produced no skips -- it no longer exercises the SKIP(reason) path"
    );
}
