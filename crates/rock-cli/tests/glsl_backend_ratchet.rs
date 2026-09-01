//! Anti-rot ratchet for the `--backend glsl` value-test executor
//! (`docs/backends.md`): pins a per-profile floor on the
//! number of `@test(returns = ...)` value tests currently RUNNABLE (emitted +
//! executed green on the GPU) over a small fixed corpus, so a GLSL-emitter or
//! readback regression that silently shrinks coverage goes red. Emitter growth
//! (A1 fn-emission work) raises the floor.
//!
//! No GL context (headless, runner exit 2) FAILS unless `ROCK_ALLOW_NO_GL=1`
//! acknowledges it: an early `return` here read as PASS in nextest's summary
//! while enforcing nothing, which is the same swallow the gate exists to catch.
//! A missing `stone-glsl-runner` binary is a hard error from `glsl_test_run` --
//! the gate must not read as green when its executor was never built. The runner is
//! located as a sibling of the test binary (`target/<profile>/stone-glsl-runner`);
//! `just t` and CI both build it before running this.

fn corpus() -> Vec<String> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root");
    [
        "tests/intrinsics/value_reference.rock",
        "tests/js_loops_valuemodel.rock",
        "tests/js_collections.rock",
    ]
    .iter()
    .map(|p| root.join(p).to_string_lossy().into_owned())
    .collect()
}

fn allow_no_gl() -> bool {
    std::env::var("ROCK_ALLOW_NO_GL").as_deref() == Ok("1")
}

#[test]
fn glsl_runnable_coverage_ratchet() {
    let summary = rock_cli::glsl_test::glsl_test_run(&corpus(), false)
        .unwrap_or_else(|e| panic!("glsl gate cannot run: {e:#}"));

    // No GL context is an ENVIRONMENT fact, but a test that returns early here
    // reports PASS while proving nothing -- and nextest reports it in the passed
    // count, indistinguishable from real enforcement. So it fails by default and
    // the escape hatch is explicit: a box that genuinely cannot make a GL context
    // sets ROCK_ALLOW_NO_GL=1 and gets a loud skip line instead.
    if summary.per_profile.iter().all(|p| !p.gl_available) {
        assert!(
            allow_no_gl(),
            "no GL context on any profile, so this ratchet enforced NOTHING. \
             Ensure a display/driver the `stone-glsl-runner` can use, or set \
             ROCK_ALLOW_NO_GL=1 to acknowledge an unenforced run."
        );
        eprintln!(
            "SKIP glsl_runnable_coverage_ratchet: ROCK_ALLOW_NO_GL=1 and no GL context \
             -- GLSL runnable coverage was NOT enforced"
        );
        return;
    }

    assert_eq!(summary.unit_failures, 0, "no unit should fail to build");

    // The value class is fixed by the corpus; this many @tests carry no
    // `returns=` and are permanently outside it (a collapsed count, never a
    // histogram). Pinned so the tier-1 split can't silently drift.
    assert_eq!(
        summary.outside_value_class, 0,
        "the corpus is all value tests; a non-value @test crept in"
    );

    // Per-profile floor: GPU-executed green value tests over this corpus. RAISE
    // when A1 fn-emission growth makes more tests runnable; a drop below it is a
    // silent-coverage regression. value_reference.rock (38, incl. the B2 bvec
    // tier) + js_loops_valuemodel (6) + js_collections (1 emits:
    // `to_string_int_roundtrip`) = 45 per profile.
    const FLOOR: usize = 45;
    for p in &summary.per_profile {
        if !p.gl_available {
            // One profile short is still a coverage hole, so it needs the same
            // acknowledgement as the all-profiles case above.
            assert!(
                allow_no_gl(),
                "profile [{}] had no GL context while another did, so its floor went \
                 unchecked. Set ROCK_ALLOW_NO_GL=1 to acknowledge an unenforced profile.",
                p.profile
            );
            eprintln!("SKIP profile [{}]: no GL context -- floor NOT enforced", p.profile);
            continue;
        }
        assert!(
            p.failed.is_empty(),
            "profile [{}]: {} value test(s) FAILED on the GPU: {:?}",
            p.profile,
            p.failed.len(),
            p.failed
        );
        assert!(
            p.passed >= FLOOR,
            "profile [{}]: GLSL-runnable coverage regressed: {} passed < floor {}. Skip histogram:\n{}",
            p.profile,
            p.passed,
            FLOOR,
            p.skip_reasons
                .iter()
                .map(|(r, c)| format!("  {c:>4}  {r}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        // The corpus must keep exercising the SKIP path too (an empty histogram
        // would mean it no longer covers un-emittable surface).
        assert!(
            !p.skip_reasons.is_empty(),
            "profile [{}]: corpus produced no skips -- it no longer exercises the SKIP(reason) path",
            p.profile
        );
    }
}
