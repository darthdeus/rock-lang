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
        "tests/js_loops_valuemodel.rock",
        "tests/js_collections.rock",
        "tests/js_closures.rock",
        "tests/js_match.rock",
        "tests/js_multivalue.rock",
        "tests/js_inline.rock",
        "tests/js_value_structs.rock",
        "tests/js_shared_tail.rock",
        "tests/closure_semantics.rock",
        "tests/closure_capture_semantics.rock",
        "tests/js_coroutines.rock",
        "tests/coroutines.rock",
        "tests/stackless_coroutines/phase1_linear.rock",
        "tests/stackless_coroutines/phase3_loops.rock",
        "tests/js_table_unset.rock",
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
    // raised it from 4 to 55; V1's value-test corpus
    // (tests/intrinsics/value_reference.rock) raised it to 67; T3 (loops) added
    // tests/js_loops_valuemodel.rock to the corpus, raising it to 74; T4
    // (error/assert, Vec, structs, Map, equality) added tests/js_collections.rock
    // and lifted the whole corpus to 91. T5 (closures/lambdas) added
    // tests/js_closures.rock and lifted it to 99. T6 (short-circuit fold +
    // exhaustive-match structuring) added tests/js_match.rock and lifted it to 106.
    // T7 (multi-value ABI + loop-capture) added tests/js_multivalue.rock -> 111.
    // T8a (shared transform pipeline: inline/cleanup/recover + narrowed arity
    // guard + relooper empty-forward/infinite-loop fixes) added tests/js_inline.rock
    // -> 114 (full-suite pass count 2299 -> 2568). T8b (GC-free value structs emit
    // a plain `{__ty, ...}` object; GC-carrying ones reject) added
    // tests/js_value_structs.rock -> 118 (full-suite 2568 -> 2650). T8c
    // (shared-branch-tail structuring: lambda-body loop analysis, tail
    // duplication, labeled loops for multi-level jumps) added
    // tests/js_shared_tail.rock -> 122 (full-suite 2650 -> 2738). S1 (shared
    // closure-conversion MIR pass: env records + hoisted siblings, deleting the
    // T5/T7/T8c native-arrow path) added tests/closure_semantics.rock +
    // tests/closure_capture_semantics.rock -> 134; the loop-counter/enumerate
    // captures the deleted T7 path skipped now emit (full-suite 2743 -> 2756).
    // S4a (coroutines: shared step transform + materialize_frame + trampoline
    // emission + JS ctor/runtime) added tests/js_coroutines.rock,
    // tests/coroutines.rock, and stackless phase1/phase3 -> 157 (full-suite
    // 2756 -> 2876; the `resume`/`kill`/`coroutine fn` buckets collapsed).
    // T8d table_unset (`__table_unset`: Map delete / array hole, dispatched on
    // `instanceof Map`) added tests/js_table_unset.rock -> 165 (full-suite
    // 2878 -> 2941; the table_unset bucket of 142 collapsed).
    const FLOOR: usize = 165;
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
