use std::time::Duration;

#[test]
fn allocation_report_writer_is_explicit_and_writes_the_compiler_report() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("allocation-report.txt");
    let report = skull::allocation_report::AllocationReport::default();

    super::write_allocation_report(Some(path.as_os_str()), &report).expect("write report");

    let text = std::fs::read_to_string(path).expect("read report");
    assert!(text.contains("emitted allocation-capable Lua report: 0 bodies, 0 direct sites"));
    assert!(text.contains("emitted site manifest: 0 sites"));
}

fn emit_overlay_checked(
    ws: &bone_semantic::inference::Workspace,
    cache: &mut skull::mir_emit::SkullEmitCache,
    reinferred: &[(bone_semantic::prelude::ModuleId, bone_semantic::hir::BodyOwner)],
) -> skull::mir_emit::MirEmit {
    skull::mir_emit::emit_overlay(ws, cache, reinferred).expect("emit_overlay")
}

#[cfg(test)]
enum ExecOutcome {
    /// Clean run; holds full stdout (module load prints + test prints).
    Output(String),
    /// Runtime error, lua load failure, spawn failure, or timeout -- carrying
    /// the diagnostic. A test that pins a SPECIFIC failure has to be able to
    /// tell it from an unrelated load error or a timeout; discarding the message
    /// made every `Errored` arm accept any failure as proof.
    Errored(String),
}

#[cfg(test)]
impl ExecOutcome {
    /// The failure diagnostic, or a panic naming `ctx` if the run succeeded.
    fn expect_errored(self, ctx: &str) -> String {
        match self {
            ExecOutcome::Errored(d) => d,
            ExecOutcome::Output(o) => {
                panic!("{ctx}: expected a failure, got clean output:\n{}", normalize_stdout(&o))
            }
        }
    }
}

#[cfg(test)]
fn exec_lua(program: &str) -> ExecOutcome {
    let timeout = Some(Duration::from_secs(15));
    match rock_host::lua_runner::run_lua_code_with_timeout(program, timeout) {
        Ok(out) if !out.success => ExecOutcome::Errored(out.combined()),
        Ok(out) => ExecOutcome::Output(out.combined()),
        Err(e) => ExecOutcome::Errored(format!("lua runner failed: {e}")),
    }
}

/// Replace `0x<hex>` sequences with `0xADDR`: table/cdata identity prints
/// differ per-process and would false-positive every diff.
#[cfg(test)]
fn normalize_stdout(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'0' && i + 2 < bytes.len() && bytes[i + 1] == b'x'
            && bytes[i + 2].is_ascii_hexdigit()
        {
            out.push_str("0xADDR");
            i += 2;
            while i < bytes.len() && bytes[i].is_ascii_hexdigit() {
                i += 1;
            }
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

/// First line where `lhs` and `rhs` differ, for divergence reporting.
#[cfg(test)]
fn first_diff_line(lhs: &str, rhs: &str) -> String {
    for (i, (a, b)) in lhs.lines().zip(rhs.lines()).enumerate() {
        if a != b {
            return format!("line {}: {:?} != {:?}", i + 1, a, b);
        }
    }
    format!(
        "line count: {} != {}",
        lhs.lines().count(),
        rhs.lines().count()
    )
}

/// `true` when the outputs contain the same lines in a different order.
/// Map iteration rides Lua `pairs()`, whose order is unspecified and can
/// legitimately differ between a frozen baseline+overlay and a fresh build.
#[cfg(test)]
fn is_order_only(a: &str, b: &str) -> bool {
    let mut la: Vec<&str> = a.lines().collect();
    let mut lb: Vec<&str> = b.lines().collect();
    la.sort_unstable();
    lb.sort_unstable();
    la == lb
}

#[cfg(test)]
mod incremental_watch_tests {
    //! End-to-end check that a real `bone_semantic::Session`'s `reinferred` list drives
    //! `emit_incremental` to the same Lua a fresh full build produces — the watch
    //! loop's correctness, minus the file-watching. Mirrors the in-skull oracle
    //! (`skull/tests/incremental_emit.rs`) but through the actual Session edit
    //! path used by `skull_watch`, so it also covers "is `reinferred` enough".

    use crate::bone_backend::bone_session;

    fn files(src: &str) -> Vec<(String, String)> {
        vec![("main.rock".to_string(), src.to_string())]
    }

    #[test]
    fn session_body_edit_incremental_matches_fresh() {
        let v1 = "fn helper() -> int { 3 }\nfn use_it() -> int { helper() }";
        let v2 = "fn helper() -> int { 5 }\nfn use_it() -> int { helper() }";

        // Warm session at v1, full emit -> cache.
        let mut session = bone_session(files(v1)).expect("session v1");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (_full_v1, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v1");

        // Edit helper's body -> a body-level re-inference.
        let result = session.update_file("main.rock", v2).expect("update");
        assert!(
            matches!(result.tier, bone_incremental::session::UpdateTier::BodyOnly),
            "editing a fn body should be a BodyOnly update, got {:?}",
            result.tier
        );
        let incremental =
            skull::mir_emit::emit_incremental(session.workspace(), &mut cache, &result.reinferred)
                .expect("emit_incremental");

        // Fresh session at v2, full emit.
        let fresh_session = bone_session(files(v2)).expect("session v2");
        let fresh = skull::mir_emit::emit_full(fresh_session.workspace(), mid).expect("emit_full v2").0;

        assert_eq!(
            incremental.lua, fresh.lua,
            "Session-driven incremental emit != fresh full emit after a body edit"
        );
    }

    #[test]
    fn cross_file_inline_edit_matches_fresh() {
        // a.rock's tiny `helper` is inlined into b.rock's `caller` (a CROSS-MODULE
        // inline dep — `deps` holds the callee's FuncRef across modules). Editing
        // a.rock re-infers only a's body, but skull must still re-emit b's caller.
        let mut session = bone_session(vec![
            ("a.rock".into(), "@inline\nfn helper() -> int { 3 }".into()),
            ("b.rock".into(), "fn caller() -> int { helper() }".into()),
        ])
        .expect("session");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (_init, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full");

        let result = session.update_file("a.rock", "@inline\nfn helper() -> int { 7 }").expect("update");
        let incremental =
            skull::mir_emit::emit_incremental(session.workspace(), &mut cache, &result.reinferred).expect("inc");
        let fresh = skull::mir_emit::emit_full(session.workspace(), mid).expect("fresh").0;
        assert_eq!(incremental.lua, fresh.lua, "cross-file inline edit: incremental != fresh");
    }

    /// Byte-identity oracle for the taken_by_ref -> ScalarCtx -> affected-set
    /// interaction. `foo` has a value-struct param: while a body takes it BY
    /// REFERENCE (`let f = foo`) it keeps the single-cdata ABI; with no by-ref it
    /// scalarizes. A body edit that flips that membership must re-emit foo's chunk
    /// AND its direct callers (their call-site shape changes), even though foo's
    /// own body is unchanged. Each case asserts incremental == fresh.
    fn abi_flip_case(v1: &str, v2: &str) {
        let mut session = bone_session(files(v1)).expect("session v1");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (_full, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v1");
        let result = session.update_file("main.rock", v2).expect("update");
        assert!(
            matches!(result.tier, bone_incremental::session::UpdateTier::BodyOnly),
            "edit should be BodyOnly, got {:?}",
            result.tier
        );
        let incremental =
            skull::mir_emit::emit_incremental(session.workspace(), &mut cache, &result.reinferred).expect("inc");
        let fresh = skull::mir_emit::emit_full(session.workspace(), mid).expect("fresh").0;
        assert_eq!(incremental.lua, fresh.lua, "by-ref ABI flip: incremental != fresh");
    }

    const ABI_DECL: &str = "value V2 { x: float, y: float }\n\
                            fn foo(v: V2) -> float { v.x }\n\
                            fn other() -> float { foo(V2 { x: 2.0, y: 3.0 }) }\n";

    #[test]
    fn taken_by_ref_removal_flips_abi() {
        // use_ref drops its by-ref to foo -> foo scalarizes (cdata -> splat).
        abi_flip_case(
            &format!("{ABI_DECL}fn use_ref() -> float {{ let f = foo; f(V2 {{ x: 1.0, y: 2.0 }}) }}"),
            &format!("{ABI_DECL}fn use_ref() -> float {{ other() }}"),
        );
    }

    #[test]
    fn taken_by_ref_addition_flips_abi() {
        // use_ref STARTS taking foo by ref -> foo must flip scalar -> cdata. This
        // is the runtime-dangerous direction: a stale scalarized foo def would get
        // a single cdata from the new CallValue site.
        abi_flip_case(
            &format!("{ABI_DECL}fn use_ref() -> float {{ other() }}"),
            &format!("{ABI_DECL}fn use_ref() -> float {{ let f = foo; f(V2 {{ x: 1.0, y: 2.0 }}) }}"),
        );
    }

    #[test]
    fn taken_by_ref_refcount_keeps_abi_when_one_referencer_remains() {
        // TWO bodies take foo by ref; removing ONE must NOT flip foo (still taken
        // by ref by the other) -- the refcount, not a naive set, is load-bearing.
        // A naive set-removal would wrongly scalarize foo here; incremental would
        // then diverge from fresh (which keeps foo as cdata).
        let v1 = format!(
            "{ABI_DECL}fn ref_a() -> float {{ let f = foo; f(V2 {{ x: 1.0, y: 2.0 }}) }}\n\
             fn ref_b() -> float {{ let g = foo; g(V2 {{ x: 3.0, y: 4.0 }}) }}"
        );
        let v2 = format!(
            "{ABI_DECL}fn ref_a() -> float {{ other() }}\n\
             fn ref_b() -> float {{ let g = foo; g(V2 {{ x: 3.0, y: 4.0 }}) }}"
        );
        abi_flip_case(&v1, &v2);
    }

    #[test]
    fn fast_path_multiline_then_following_edit_matches_fresh() {
        // Incremental-parse fast path: edit `a`'s body so it GROWS by lines
        // (line_delta != 0, shifting every following item), then edit `c` (which
        // sits after `a`). The second edit must locate `c` through the metadata
        // shifted by the first. Each step's emit must equal a fresh full build.
        let v0 = "fn a() -> int { 1 }\nfn b() -> int { 2 }\nfn c() -> int { 3 }";
        let v1 = "fn a() -> int {\n    let x = 1\n    x + 10\n}\nfn b() -> int { 2 }\nfn c() -> int { 3 }";
        let v2 = "fn a() -> int {\n    let x = 1\n    x + 10\n}\nfn b() -> int { 2 }\nfn c() -> int { 99 }";

        let mut session = bone_session(files(v0)).expect("v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (_f0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit v0");

        let r1 = session.update_file("main.rock", v1).expect("update a");
        let inc1 = skull::mir_emit::emit_incremental(session.workspace(), &mut cache, &r1.reinferred).expect("inc1");
        let fresh1 = skull::mir_emit::emit_full(bone_session(files(v1)).unwrap().workspace(), mid).expect("fresh1").0;
        assert_eq!(inc1.lua, fresh1.lua, "after multi-line growth edit to `a`");

        let r2 = session.update_file("main.rock", v2).expect("update c");
        let inc2 = skull::mir_emit::emit_incremental(session.workspace(), &mut cache, &r2.reinferred).expect("inc2");
        let fresh2 = skull::mir_emit::emit_full(bone_session(files(v2)).unwrap().workspace(), mid).expect("fresh2").0;
        assert_eq!(inc2.lua, fresh2.lua, "after editing `c` following a multi-line edit to `a`");
    }

    // --- fuzz oracle: incremental == fresh over random valid body edits -------
    //
    // The plan is emphatic that hand-picked tests miss codegen-dep undercoverage
    // ("only the incremental==fresh fuzz catches it" — rock's deletion bug). This
    // builds a function call/inline graph, then mutates random bodies (always
    // type-valid by construction: every fn is `-> int`, every callee is a
    // lower-indexed fn so the graph is a DAG, no recursion), driving the REAL
    // `Session.update_file` -> `emit_incremental` path. After each edit it asserts
    // the incremental assembly is byte-identical to `emit_full` of the SAME
    // (edited) workspace — the fresh build is the ground truth. A divergence means
    // a chunk that should have re-emitted (an inline-dependent caller) didn't.

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            self.0 >> 33
        }
        fn pick(&mut self, n: usize) -> usize {
            (self.next() as usize) % n.max(1)
        }
    }

    #[derive(Clone)]
    enum Body {
        Const(i64),
        Call(usize), // calls f{j}, j < i (DAG, inlinable, no recursion)
    }

    fn render(bodies: &[Body]) -> String {
        use std::fmt::Write as _;
        let mut s = String::new();
        // `@inline` on every fn so the call graph actually inlines under the
        // default annotation-gated policy -- this fuzz's whole point is to
        // exercise inline-dependency re-emit (a leaf edit must re-emit its
        // inliners) + InlineCache invalidation across edits.
        for (i, b) in bodies.iter().enumerate() {
            match b {
                Body::Const(k) => {
                    let _ = writeln!(s, "@inline\nfn f{i}() -> int {{ {k} }}");
                }
                Body::Call(j) => {
                    let _ = writeln!(s, "@inline\nfn f{i}() -> int {{ f{j}() }}");
                }
            }
        }
        s
    }

    /// Mutate one body to a new valid form; returns the index changed.
    fn mutate(bodies: &mut [Body], rng: &mut Rng) -> usize {
        let i = rng.pick(bodies.len());
        bodies[i] = if i == 0 || rng.pick(2) == 0 {
            Body::Const(rng.pick(1000) as i64)
        } else {
            Body::Call(rng.pick(i)) // some lower-indexed fn -> inline edge
        };
        i
    }

    fn run_fuzz(seed: u64, n_fns: usize, steps: usize) {
        let mut rng = Rng(seed.max(1));
        // Start: a mix of consts and call edges so inlining is exercised from step 0.
        let mut bodies: Vec<Body> = (0..n_fns)
            .map(|i| {
                if i > 0 && rng.pick(2) == 0 {
                    Body::Call(rng.pick(i))
                } else {
                    Body::Const(rng.pick(1000) as i64)
                }
            })
            .collect();

        let mut session = bone_session(files(&render(&bodies))).expect("fuzz session init");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (_init, mut cache) =
            skull::mir_emit::emit_full(session.workspace(), mid).expect("fuzz emit_full init");

        for step in 0..steps {
            let changed = mutate(&mut bodies, &mut rng);
            let text = render(&bodies);
            let result = session
                .update_file("main.rock", &text)
                .unwrap_or_else(|e| panic!("seed {seed} step {step}: update_file failed: {}", e.message));

            // By construction every program type-checks; a diagnostic is a bug.
            let diags = session.diagnostics();
            assert!(
                diags.is_empty(),
                "seed {seed} step {step} (edited f{changed}): unexpected diagnostics: {:?}\n--- program ---\n{text}",
                diags.iter().map(|d| d.message.clone()).collect::<Vec<_>>()
            );

            let incremental =
                skull::mir_emit::emit_incremental(session.workspace(), &mut cache, &result.reinferred)
                    .unwrap_or_else(|e| panic!("seed {seed} step {step}: emit_incremental failed: {}", e.message));
            let fresh = skull::mir_emit::emit_full(session.workspace(), mid)
                .unwrap_or_else(|e| panic!("seed {seed} step {step}: emit_full failed: {}", e.message))
                .0;

            assert_eq!(
                incremental.lua, fresh.lua,
                "seed {seed} step {step}: incremental != fresh after editing f{changed} \
                 (reinferred={} bodies)\n--- program ---\n{text}",
                result.reinferred.len()
            );
        }
    }

    #[test]
    fn fuzz_incremental_matches_fresh() {
        // A handful of deterministic seeds; each drives many valid body edits on a
        // call/inline graph and checks incremental==fresh every step. Override with
        // SKULL_FUZZ_SEEDS / SKULL_FUZZ_STEPS for a longer soak.
        let steps: usize = std::env::var("SKULL_FUZZ_STEPS").ok().and_then(|s| s.parse().ok()).unwrap_or(120);
        let seeds: u64 = std::env::var("SKULL_FUZZ_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(8);
        for seed in 1..=seeds {
            run_fuzz(seed.wrapping_mul(2654435761), 16, steps);
        }
    }
}

#[cfg(test)]
mod overlay_runtime_tests {
    //! Runtime-equivalence oracle for the TWO-FILE SPLIT (`main.lua` +
    //! `incremental.lua`). The byte-identical oracles (`incremental_watch_tests`,
    //! `skull/tests/incremental_emit.rs`) cover the full-reassembly
    //! `emit_incremental` path; they say nothing about the overlay, which is NOT
    //! byte-equal to a full build by design (Lua later-definition-wins). The only
    //! correctness gate for the split is BEHAVIORAL: a frozen `main.lua` (the
    //! stale baseline) + an accumulating overlay, EXECUTED under luajit, must
    //! print exactly what a fresh full build of the final source prints.
    //!
    //! This exercises three things no other test does at runtime:
    //!   1. later-definition-wins -- the overlay's fresh `foo` overrides the
    //!      stale `foo` baked into the frozen baseline;
    //!   2. accumulation -- edit A then B, the overlay must still carry A' (else A
    //!      silently reverts to the baseline's stale copy);
    //!   3. inline-dependent caller re-emit -- editing an inlined callee must
    //!      re-emit its callers into the overlay (the callers' chunks contain the
    //!      callee's body), or the caller runs the stale inlined value.
    //!
    //! Strategy mirrors `SkullStone`: drive edits through a real `bone_semantic::Session`'s
    //! `update_file` -> `reinferred`, `emit_overlay` from the SAME accumulating
    //! cache, and compose `frozen_baseline ++ overlay` against a fresh `emit_full`.

    use crate::bone_backend::bone_session;
    use super::{emit_overlay_checked, exec_lua, normalize_stdout, ExecOutcome};

    /// Standalone `@test`/synthetic programs have no engine. Stub the full-reload
    /// registry reset only when a fixture emits one; body overlays refresh the
    /// CodeRefs already stored in registries.
    const SYS_STUB: &str = "function clear_all_systems() end\n";

    /// The stub is omitted when a fixture defines its own reset function.
    fn stub_for(full_lua: &str) -> &'static str {
        if full_lua.contains("function clear_all_systems") { "" } else { SYS_STUB }
    }

    /// `main` is unmangled (no-mangle exception), so a bare `main()` entry runs
    /// it on both sides without computing a mangled name. Append AFTER the
    /// overlay so the overlay's fresh `main` wins over the stale baseline's.
    fn run_parts(parts: &[&str]) -> ExecOutcome {
        exec_lua(&parts.join("\n"))
    }

    /// Assert the frozen-baseline+overlay execution matches the fresh full build
    /// byte-for-byte on stdout (addresses normalized). `ctx` labels failures.
    fn assert_runtime_eq(stale_main: &str, overlay: &str, fresh_full: &str, ctx: &str) {
        let composed = run_parts(&[stale_main, stub_for(stale_main), overlay, "main()"]);
        let fresh = run_parts(&[fresh_full, stub_for(fresh_full), "main()"]);
        match (composed, fresh) {
            (ExecOutcome::Output(c), ExecOutcome::Output(f)) => {
                let (nc, nf) = (normalize_stdout(&c), normalize_stdout(&f));
                assert_eq!(
                    nc, nf,
                    "{ctx}: frozen-baseline + overlay diverged from fresh full build at runtime\n\
                     --- composed ---\n{nc}\n--- fresh ---\n{nf}"
                );
            }
            (ExecOutcome::Errored(_), ExecOutcome::Errored(_)) => {
                panic!("{ctx}: BOTH sides errored -- harness bug (bad source or missing stub)");
            }
            (ExecOutcome::Output(_), ExecOutcome::Errored(_)) => {
                panic!("{ctx}: composed (baseline+overlay) runs but fresh full build errors")
            }
            (ExecOutcome::Errored(_), ExecOutcome::Output(_)) => {
                panic!("{ctx}: fresh full build runs but composed (baseline+overlay) errors")
            }
        }
    }

    fn files(src: &str) -> Vec<(String, String)> {
        vec![("main.rock".to_string(), src.to_string())]
    }

    /// The skull watcher's robustness contract: a BROKEN edit between two valid
    /// states must (a) surface diagnostics so the watcher skips the hot reload
    /// and keeps the last-good program running, and (b) leave the session able to
    /// recover -- the next valid edit emits a program identical to a fresh build.
    /// The parse is total, so a broken edit no longer fails `update_file`; the
    /// watcher gates on `diagnostics()` instead and drops its incremental cache so
    /// recovery is a correct full rebuild (not a diff across the broken state).
    #[test]
    fn watcher_skips_reload_on_broken_edit_and_recovers() {
        let v1 = "fn compute() -> int { 1 }\nfn main() { print(\"{compute()}\") }\n";
        let v2 = "fn compute() -> int { 42 }\nfn main() { print(\"{compute()}\") }\n";
        let broken = "fn compute() -> int { 1\nfn main() { print(\"{compute()}\") }\n"; // missing `}`

        let mut session = bone_session(files(v1)).expect("session v1");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        assert!(session.diagnostics().is_empty(), "v1 is clean");

        // Broken edit: total parse -> Ok, but a diagnostic appears. The watcher's
        // gate (`!errs.is_empty()`) fires here, so it serves last good + no reload.
        session.update_file("main.rock", broken).expect("broken update is total (Ok)");
        assert!(
            !session.diagnostics().is_empty(),
            "a broken edit must surface a diagnostic so the watcher skips the reload",
        );

        // Recovery to a NEW valid program: clean, and a full rebuild matches a
        // fresh session built from v2 (no corruption from the broken intermediate).
        session.update_file("main.rock", v2).expect("recover to v2");
        assert!(session.diagnostics().is_empty(), "v2 is clean again");
        let recovered = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit recovered").0;

        let fresh_session = bone_session(files(v2)).expect("fresh v2 session");
        let fresh_mid = skull::user_module_id(fresh_session.workspace()).unwrap();
        let fresh = skull::mir_emit::emit_full(fresh_session.workspace(), fresh_mid).expect("emit fresh").0;

        assert_eq!(
            recovered.lua, fresh.lua,
            "recovered build after a broken intermediate must equal a fresh build",
        );
    }

    /// Regression for the "stale main.lua / `IVec2_ct nil`" desync class
    /// (TODO.md): a body edit's overlay references a value-struct ctor (e.g.
    /// `P_ct`) emitted GLOBAL into main.lua by `value_struct_defs`. The frozen
    /// baseline (v0) must already carry EVERY defined value struct's ctor -- even
    /// `P`, which v0's bodies never construct -- so when the edited body in the
    /// overlay first constructs `P`, its `P_ct` resolves against the baseline
    /// rather than crashing on nil. Pins the two invariants that close the bug:
    /// `value_struct_defs` emits ALL value structs (not use-gated), and the frozen
    /// baseline is a real full build. If either regresses, the composed run errors
    /// (nil ctor) while fresh succeeds, and `assert_runtime_eq` fails loudly.
    #[test]
    fn overlay_referencing_baseline_value_ctor_runtime_eq() {
        // `P` is DEFINED in both versions but only CONSTRUCTED in v1's `compute`.
        // v1 uses `P` as a WHOLE value (`==`), not pure field reads: SROA
        // (`scalarize_struct_cells`) scalarizes a field-read-only struct away
        // (no ctor), which would defeat this test's point -- it must reference the
        // baseline's `P_ct`, so it must keep the cdata (`==` forces that).
        let decl = "value P { x: int, y: int }\n";
        let v0 = format!("{decl}fn compute() -> int {{ 1 }}\nfn main() {{ print(\"{{compute()}}\") }}\n");
        let v1 = format!(
            "{decl}fn compute() -> int {{ let p = P {{ x: 5, y: 6 }}\n let q = P {{ x: 2, y: 9 }}\n if p == q {{ 0 }} else {{ p.x + p.y }} }}\nfn main() {{ print(\"{{compute()}}\") }}\n"
        );

        let mut session = bone_session(files(&v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");
        let stale_main = full0.lua;
        // Sanity: the frozen baseline defines P's ctor even though nothing builds P.
        assert!(stale_main.contains("P_ct"), "baseline must emit every value-struct ctor, even unused ones");

        let result = session.update_file("main.rock", &v1).expect("update v1");
        assert!(
            matches!(result.tier, bone_incremental::session::UpdateTier::BodyOnly),
            "constructing an already-defined value struct is a body edit, got {:?}",
            result.tier
        );
        let overlay = emit_overlay_checked(session.workspace(), &mut cache, &result.reinferred);
        assert!(overlay.lua.contains("P_ct"), "overlay's edited body must reference P_ct");
        let fresh = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v1").0;

        assert_runtime_eq(&stale_main, &overlay.lua, &fresh.lua, "overlay refs baseline value ctor (P 5+6=11)");
    }

    /// One concrete edit, fully spelled out, so the mechanism is legible before
    /// the fuzz generalizes it. v0: f0=1, f1 inlines f0, main prints both -> "1\n1".
    /// Edit f0 to 2. The overlay must re-emit f0 AND the inlined caller f1 AND
    /// main (which inlines them) -> "2\n2". If any of those is missing from the
    /// overlay, the stale baseline's 1 leaks through.
    #[test]
    fn concrete_inlined_edit_runtime_eq() {
        let v0 = "@inline\nfn f0() -> int { 1 }\n\
                  fn f1() -> int { f0() }\n\
                  fn main() { print(\"{f0()}\") print(\"{f1()}\") }\n";
        let v1 = "@inline\nfn f0() -> int { 2 }\n\
                  fn f1() -> int { f0() }\n\
                  fn main() { print(\"{f0()}\") print(\"{f1()}\") }\n";

        let mut session = bone_session(files(v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");
        let stale_main = full0.lua;

        let result = session.update_file("main.rock", v1).expect("update v1");
        assert!(
            matches!(result.tier, bone_incremental::session::UpdateTier::BodyOnly),
            "literal edit should be BodyOnly, got {:?}",
            result.tier
        );
        let overlay = emit_overlay_checked(session.workspace(), &mut cache, &result.reinferred);
        let fresh = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v1").0;

        // Sanity: the stale baseline alone prints 1,1 -- so a leak is observable.
        let baseline_only = run_parts(&[&stale_main, SYS_STUB, "main()"]);
        if let ExecOutcome::Output(o) = baseline_only {
            assert_eq!(normalize_stdout(&o).trim(), "1\n1", "baseline should print the STALE values");
        } else {
            panic!("stale baseline failed to run");
        }

        assert_runtime_eq(&stale_main, &overlay.lua, &fresh.lua, "concrete f0 1->2");
    }

    /// A `const`'s value edit hot-reloads through the overlay. v0: FOO=5, main
    /// prints it -> "5". Edit FOO to 10: the overlay must re-emit FOO's init
    /// UNGUARDED so the reload reassigns it -> "10". If the overlay omits the const
    /// init, the stale baseline's 5 leaks through and diverges from the fresh build.
    #[test]
    fn concrete_const_value_edit_runtime_eq() {
        let v0 = "const FOO: int = 5\nfn main() { print(\"{FOO}\") }\n";
        let v1 = "const FOO: int = 10\nfn main() { print(\"{FOO}\") }\n";

        let mut session = bone_session(files(v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");
        let stale_main = full0.lua;

        let result = session.update_file("main.rock", v1).expect("update v1");
        assert!(
            matches!(result.tier, bone_incremental::session::UpdateTier::BodyOnly),
            "const value edit should be BodyOnly, got {:?}",
            result.tier
        );
        let overlay = emit_overlay_checked(session.workspace(), &mut cache, &result.reinferred);
        let fresh = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v1").0;

        // Sanity: the stale baseline alone prints the OLD const, so a leak is observable.
        let baseline_only = run_parts(&[&stale_main, SYS_STUB, "main()"]);
        if let ExecOutcome::Output(o) = baseline_only {
            assert_eq!(normalize_stdout(&o).trim(), "5", "baseline prints the STALE const");
        } else {
            panic!("stale baseline failed to run");
        }

        assert_runtime_eq(&stale_main, &overlay.lua, &fresh.lua, "const FOO 5->10");
    }

    /// A `static` local's INITIALIZER edit hot-reloads through the overlay -- the
    /// static analog of `concrete_const_value_edit_runtime_eq` and the direct
    /// regression for `docs/static-hot-reload-overlay-bug.md`. Unlike a const, a
    /// static is lazy (inits on first reach) and name-keyed (its cell persists
    /// across reloads BY DESIGN), so the overlay must emit an UNGUARDED reset (nil
    /// the cell + clear its done-flag) for a static whose init key changed, or the
    /// redefined fn re-reads the stale warmed cell and the edit is silently ignored.
    ///
    /// Models the LIVE game: the baseline runs once (warming the static to 5), THEN
    /// the overlay loads and it runs again. Without the reset the second call still
    /// prints 5; with it, 10. (`assert_runtime_eq`'s compose order never warms the
    /// static before the overlay, so it can't see this -- hence the bespoke run.)
    #[test]
    fn concrete_static_init_edit_runtime_eq() {
        let v0 = "fn probe() -> int { static x = 5\n x }\nfn main() { print(\"{probe()}\") }\n";
        let v1 = "fn probe() -> int { static x = 10\n x }\nfn main() { print(\"{probe()}\") }\n";

        let mut session = bone_session(files(v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");
        let stale_main = full0.lua;

        let result = session.update_file("main.rock", v1).expect("update v1");
        assert!(
            matches!(result.tier, bone_incremental::session::UpdateTier::BodyOnly),
            "static initializer edit should be BodyOnly, got {:?}",
            result.tier
        );
        let overlay = emit_overlay_checked(session.workspace(), &mut cache, &result.reinferred);

        // Warm the static in the frozen baseline (first main() -> inits x=5, sets
        // the done-flag), THEN load the overlay + call again. The bug is observable
        // precisely because the cell is already warm: without the overlay's reset
        // the redefined `probe` skips re-init and re-reads the stale 5.
        let composed = run_parts(&[&stale_main, SYS_STUB, "main()", &overlay.lua, "main()"]);
        match composed {
            ExecOutcome::Output(o) => assert_eq!(
                normalize_stdout(&o).trim(),
                "5\n10",
                "warm baseline prints 5; after the overlay reload the static re-inits to 10",
            ),
            ExecOutcome::Errored(_) => panic!("composed baseline+overlay run errored"),
        }
    }

    /// A `static` inside a NESTED `fn` must hot-reload too. Nested fns are hoisted
    /// to sibling chunks, so their statics have to be folded into the enclosing
    /// chunk's `statics` (both the full-reload preamble and the overlay read
    /// `chunk.statics`); if they aren't, the cell is invisible to the reset logic
    /// and an initializer edit is silently ignored on every reload.
    #[test]
    fn nested_fn_static_init_edit_runtime_eq() {
        let v0 = "fn probe() -> int { fn inner() -> int { static x = 5\n x }\n inner() }\nfn main() { print(\"{probe()}\") }\n";
        let v1 = "fn probe() -> int { fn inner() -> int { static x = 10\n x }\n inner() }\nfn main() { print(\"{probe()}\") }\n";

        let mut session = bone_session(files(v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");
        let stale_main = full0.lua;

        let result = session.update_file("main.rock", v1).expect("update v1");
        assert!(
            matches!(result.tier, bone_incremental::session::UpdateTier::BodyOnly),
            "nested static init edit should be BodyOnly, got {:?}",
            result.tier
        );
        let overlay = emit_overlay_checked(session.workspace(), &mut cache, &result.reinferred);

        let composed = run_parts(&[&stale_main, SYS_STUB, "main()", &overlay.lua, "main()"]);
        match composed {
            ExecOutcome::Output(o) => assert_eq!(
                normalize_stdout(&o).trim(),
                "5\n10",
                "nested-fn static must re-init on initializer edit through the overlay",
            ),
            ExecOutcome::Errored(_) => panic!("composed baseline+overlay run errored"),
        }
    }

    /// Accumulation across TWO overlays (the analog of the const "edit A then B"
    /// oracle, point 2 in this module's doc). A reset line fires only on the reload
    /// that dofiles the overlay carrying it, so it must appear ONLY in the overlay
    /// of the reload where the init key changed -- NOT in later overlays, or an
    /// unrelated follow-up edit would re-reset and wipe accumulated static state.
    /// Requires advancing the per-cell baseline as overlays apply; a baseline only
    /// rebuilt on full builds would re-emit the reset here and lose the accumulation.
    #[test]
    fn static_init_edit_then_unrelated_edit_preserves_reinit_overlay() {
        // `probe` accumulates into a static counter each call; DO_RESET (unused
        // here) keeps the fn shape stable. v1 edits the initializer (0 -> 100); v2
        // is an UNRELATED edit (dead `let`), initializer still 100.
        let v0 = "fn probe() -> int { static n = 0\n n = n + 1\n n }\nfn main() { print(\"{probe()}\") }\n";
        let v1 = "fn probe() -> int { static n = 100\n n = n + 1\n n }\nfn main() { print(\"{probe()}\") }\n";
        let v2 = "fn probe() -> int { static n = 100\n let unused = 0\n n = n + 1\n n }\nfn main() { print(\"{probe()}\") }\n";

        let mut session = bone_session(files(v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");
        let stale_main = full0.lua;

        let r1 = session.update_file("main.rock", v1).expect("update v1");
        let overlay1 = emit_overlay_checked(session.workspace(), &mut cache, &r1.reinferred);
        let r2 = session.update_file("main.rock", v2).expect("update v2");
        let overlay2 = emit_overlay_checked(session.workspace(), &mut cache, &r2.reinferred);

        // Models the live game: main.lua(v0) loaded once; each overlay dofile'd in
        // turn; probe reached (accumulating) between reloads. n: inits 0 -> 1,2 ->
        // reload1 re-inits to 100 -> 101,102 -> reload2 UNRELATED must PRESERVE ->
        // 103,104. A re-reset at reload2 would drop back to 101.
        let composed = run_parts(&[
            &stale_main, SYS_STUB,
            "main()", "main()",            // 1, 2   (init 0)
            &overlay1.lua,
            "main()", "main()",            // 101, 102  (re-init 100)
            &overlay2.lua,
            "main()", "main()",            // 103, 104  (preserve)
        ]);
        match composed {
            ExecOutcome::Output(o) => assert_eq!(
                normalize_stdout(&o).trim(),
                "1\n2\n101\n102\n103\n104",
                "reload1 re-inits to 100; reload2 (unrelated) must preserve, not re-reset",
            ),
            ExecOutcome::Errored(_) => panic!("composed baseline+overlays run errored"),
        }
    }

    /// The §A narrowing on the overlay path: an UNRELATED edit in a fn that owns a
    /// static must NOT reset the static -- the overlay reconsiders the fn's cells
    /// (it's in `changed_since_full`) but the key is unchanged, so the warm value
    /// is preserved. Mirror of `static_hot_reload.rs::static_preserved_when_unrelated_body_edited`
    /// but through the overlay. Without the key check (reset every changed fn's
    /// statics) this would wrongly re-init the static and diverge.
    #[test]
    fn static_preserved_under_unrelated_overlay_edit() {
        // `DO_SET` distinguishes PRESERVE (n still 99) from RESET (n re-inits 42).
        let v0 = "global DO_SET: bool = false\n\
                  fn probe() -> int { static n = 42\n if DO_SET { n = 99 }\n n }\n\
                  fn main() { print(\"{probe()}\") }\n";
        // Unrelated body edit (dead `let`), initializer `42` untouched.
        let v1 = "global DO_SET: bool = false\n\
                  fn probe() -> int { static n = 42\n let unused = 0\n if DO_SET { n = 99 }\n n }\n\
                  fn main() { print(\"{probe()}\") }\n";

        let mut session = bone_session(files(v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");
        let stale_main = full0.lua;

        let result = session.update_file("main.rock", v1).expect("update v1");
        assert!(
            matches!(result.tier, bone_incremental::session::UpdateTier::BodyOnly),
            "unrelated body edit should be BodyOnly, got {:?}",
            result.tier
        );
        let overlay = emit_overlay_checked(session.workspace(), &mut cache, &result.reinferred);

        // Warm the static to 99 (DO_SET=true, one call), load the overlay, then call
        // again with DO_SET=false: an unrelated edit must PRESERVE n -> still 99.
        let composed = run_parts(&[&stale_main, SYS_STUB, "DO_SET = true", "main()", &overlay.lua, "DO_SET = false", "main()"]);
        match composed {
            ExecOutcome::Output(o) => assert_eq!(
                normalize_stdout(&o).trim(),
                "99\n99",
                "unrelated edit must not reset the static: warmed 99 survives the reload",
            ),
            ExecOutcome::Errored(_) => panic!("composed baseline+overlay run errored"),
        }
    }

    /// const-on-const: editing an UPSTREAM const must update a DOWNSTREAM const
    /// that reads it, in dependency order -- even with reversed source order (B
    /// declared before A). v0: B = A + 1, A = 5 -> B = 6. Edit A to 10: the overlay
    /// must re-emit BOTH consts with A's init BEFORE B's, so B becomes 11. If the
    /// overlay re-emits only the changed const (A) or emits in source order, B
    /// stays stale (6) and diverges from the fresh build (11).
    #[test]
    fn const_reading_const_overlay_runtime_eq() {
        let v0 = "const B: int = A + 1\nconst A: int = 5\nfn main() { print(\"{B}\") }\n";
        let v1 = "const B: int = A + 1\nconst A: int = 10\nfn main() { print(\"{B}\") }\n";

        let mut session = bone_session(files(v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");
        let stale_main = full0.lua;

        let result = session.update_file("main.rock", v1).expect("update v1");
        assert!(
            matches!(result.tier, bone_incremental::session::UpdateTier::BodyOnly),
            "const value edit should be BodyOnly, got {:?}",
            result.tier
        );
        let overlay = emit_overlay_checked(session.workspace(), &mut cache, &result.reinferred);
        let fresh = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v1").0;

        // Sanity: the stale baseline prints the OLD downstream value (B = old A + 1).
        let baseline_only = run_parts(&[&stale_main, SYS_STUB, "main()"]);
        if let ExecOutcome::Output(o) = baseline_only {
            assert_eq!(normalize_stdout(&o).trim(), "6", "baseline prints stale B = old A + 1");
        } else {
            panic!("stale baseline failed to run");
        }

        assert_runtime_eq(&stale_main, &overlay.lua, &fresh.lua, "const B reads A, A 5->10 => B 11");
    }

    /// Narrowing: editing one const re-emits ONLY the affected consts -- the
    /// changed one plus its transitive dependents (consts that read it) -- not
    /// every const. `A` is edited; `DEP = A + 1` must follow; the independent
    /// `UNRELATED` must NOT appear in the overlay at all.
    #[test]
    fn const_overlay_reemits_only_affected() {
        let v0 = "const A: int = 5\nconst DEP: int = A + 1\nconst UNRELATED: int = 99\nfn main() { print(\"{A} {DEP} {UNRELATED}\") }\n";
        let v1 = "const A: int = 10\nconst DEP: int = A + 1\nconst UNRELATED: int = 99\nfn main() { print(\"{A} {DEP} {UNRELATED}\") }\n";

        let mut session = bone_session(files(v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");

        let result = session.update_file("main.rock", v1).expect("update v1");
        let overlay = emit_overlay_checked(session.workspace(), &mut cache, &result.reinferred);

        // A changed -> its ginit fn + line re-emitted. DEP reads A -> its line
        // re-emitted (ginit unchanged, lives in main.lua). UNRELATED is independent
        // -> it must NOT appear in the overlay at all.
        assert!(overlay.lua.contains("__skull_ginit_A"), "changed const A must be re-emitted:\n{}", overlay.lua);
        assert!(overlay.lua.contains("DEP = "), "dependent const DEP (reads A) must be re-emitted:\n{}", overlay.lua);
        assert!(!overlay.lua.contains("UNRELATED"), "independent const UNRELATED must NOT be re-emitted:\n{}", overlay.lua);

        // ...and the result is still runtime-correct.
        let fresh = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v1").0;
        assert_runtime_eq(&full0.lua, &overlay.lua, &fresh.lua, "narrowed const overlay: A 5->10, DEP follows, UNRELATED untouched");
    }

    /// Positive control: the oracle must not be vacuously green. With the overlay
    /// OMITTED, the frozen baseline serves stale values and MUST diverge from a
    /// fresh build -- proving `assert_runtime_eq`'s comparison has teeth (an
    /// overlay that silently dropped a function would look exactly like this).
    #[test]
    fn harness_detects_a_missing_overlay() {
        let v0 = "fn f0() -> int { 1 }\nfn main() { print(\"{f0()}\") }\n";
        let v1 = "fn f0() -> int { 9 }\nfn main() { print(\"{f0()}\") }\n";

        let mut session = bone_session(files(v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let stale_main = skull::mir_emit::emit_full(session.workspace(), mid).expect("full v0").0.lua;
        session.update_file("main.rock", v1).expect("update v1");
        let fresh = skull::mir_emit::emit_full(session.workspace(), mid).expect("full v1").0.lua;

        // Empty overlay (just the stub) == "the overlay dropped everything".
        let stale = run_parts(&[&stale_main, SYS_STUB, "main()"]);
        let fresh_run = run_parts(&[&fresh, SYS_STUB, "main()"]);
        match (stale, fresh_run) {
            (ExecOutcome::Output(s), ExecOutcome::Output(f)) => assert_ne!(
                normalize_stdout(&s),
                normalize_stdout(&f),
                "missing overlay should leave a STALE/fresh divergence the oracle can detect"
            ),
            _ => panic!("both versions should run cleanly"),
        }
    }

    // --- runtime accumulation fuzz: frozen v0 baseline + cumulative overlay -----
    //
    // The byte-identical fuzz (`incremental_watch_tests::fuzz_incremental_matches_fresh`)
    // proves `emit_incremental` (full reassembly) == fresh. This one RUNS the
    // OVERLAY: a single frozen `main.lua` (v0) plus an overlay that accumulates
    // EVERY change from v0 through vk -- never a full rebuild in between, the
    // worst case for the accumulation rule. After each edit, `frozen ++ overlay`
    // executed under luajit must print exactly what a fresh `emit_full(vk)` prints.
    // Editing an inlined callee forces its callers (incl. `main`, which inlines
    // them) into the overlay; a dropped accumulation or a missed caller re-emit
    // surfaces as a wrong line of output.

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            self.0 >> 33
        }
        fn pick(&mut self, n: usize) -> usize {
            (self.next() as usize) % n.max(1)
        }
    }

    #[derive(Clone)]
    enum Body {
        Const(i64),
        Call(usize), // f{j}, j < i: a DAG inline edge (no recursion)
    }

    /// Render the f-graph plus a stable `main` that prints every fN on its own
    /// line. `main` is never mutated; it inlines the leaf fns, so it lands in the
    /// overlay whenever any callee it reaches changes.
    fn render(bodies: &[Body]) -> String {
        use std::fmt::Write as _;
        let mut s = String::new();
        // `@inline` so the graph actually inlines under the default policy -- the
        // overlay accumulation fuzz must exercise inlined-callee re-emit at runtime.
        for (i, b) in bodies.iter().enumerate() {
            match b {
                Body::Const(k) => { let _ = writeln!(s, "@inline\nfn f{i}() -> int {{ {k} }}"); }
                Body::Call(j) => { let _ = writeln!(s, "@inline\nfn f{i}() -> int {{ f{j}() }}"); }
            }
        }
        let _ = write!(s, "fn main() {{");
        for i in 0..bodies.len() {
            let _ = write!(s, " print(\"{{f{i}()}}\")");
        }
        let _ = writeln!(s, " }}");
        s
    }

    fn mutate(bodies: &mut [Body], rng: &mut Rng) -> usize {
        let i = rng.pick(bodies.len());
        bodies[i] = if i == 0 || rng.pick(2) == 0 {
            Body::Const(rng.pick(1000) as i64)
        } else {
            Body::Call(rng.pick(i))
        };
        i
    }

    fn run_overlay_fuzz(seed: u64, n_fns: usize, steps: usize) {
        let mut rng = Rng(seed.max(1));
        let mut bodies: Vec<Body> = (0..n_fns)
            .map(|i| {
                if i > 0 && rng.pick(2) == 0 { Body::Call(rng.pick(i)) } else { Body::Const(rng.pick(1000) as i64) }
            })
            .collect();

        let v0 = render(&bodies);
        let mut session = bone_session(files(&v0)).unwrap_or_else(|e| panic!("seed {seed}: session init: {}", e.message));
        let mid = skull::user_module_id(session.workspace()).unwrap();
        // The FROZEN baseline: emitted once at v0, never rebuilt. The overlay must
        // carry the cumulative v0->vk diff on top of it.
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid)
            .unwrap_or_else(|e| panic!("seed {seed}: emit_full v0: {}", e.message));
        let stale_main = full0.lua;

        for step in 0..steps {
            let changed = mutate(&mut bodies, &mut rng);
            let text = render(&bodies);
            let result = session
                .update_file("main.rock", &text)
                .unwrap_or_else(|e| panic!("seed {seed} step {step}: update_file: {}", e.message));

            let diags = session.diagnostics();
            assert!(
                diags.is_empty(),
                "seed {seed} step {step} (edited f{changed}): unexpected diagnostics: {:?}\n{text}",
                diags.iter().map(|d| d.message.clone()).collect::<Vec<_>>()
            );

            let overlay = emit_overlay_checked(session.workspace(), &mut cache, &result.reinferred);
            let fresh = skull::mir_emit::emit_full(session.workspace(), mid)
                .unwrap_or_else(|e| panic!("seed {seed} step {step}: emit_full vk: {}", e.message))
                .0;

            assert_runtime_eq(
                &stale_main,
                &overlay.lua,
                &fresh.lua,
                &format!("seed {seed} step {step} (edited f{changed}, cumulative since v0)"),
            );
        }
    }

    #[test]
    fn runtime_accumulation_fuzz() {
        // Each luajit spawn costs ~10-30ms, so this runs fewer/shorter than the
        // byte fuzz. Override with SKULL_RT_FUZZ_SEEDS / SKULL_RT_FUZZ_STEPS for a
        // longer soak.
        let steps: usize = std::env::var("SKULL_RT_FUZZ_STEPS").ok().and_then(|s| s.parse().ok()).unwrap_or(20);
        let seeds: u64 = std::env::var("SKULL_RT_FUZZ_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(4);
        for seed in 1..=seeds {
            run_overlay_fuzz(seed.wrapping_mul(2654435761), 8, steps);
        }
    }

    // --- real-corpus composition: overlay every fn, RUN @tests vs full ---------
    //
    // The synthetic fuzz covers later-wins + accumulation on an int-only graph.
    // Real corpus files exercise code SHAPES the synthetic graph can't reach
    // (struct/value-struct ctors, generics, closures, coroutines, maps, vecs).
    // Here v0 == vN: emit_full -> baseline, then route EVERY user function through
    // the overlay (the maximal body edit) and assert each `@test` prints the same
    // composed (baseline ++ overlay) as it does from the baseline alone. Identity
    // bodies mean any divergence is a COMPOSITION bug -- redefining a real-shaped
    // function after main.lua, or stale slot publication, breaking it.

    use bone_semantic::hir::BodyOwner;

    /// Resolve a workspace-root-relative path. A unit test's CWD is the crate
    /// dir (`crates/rock-cli/`), but the corpus lives at the workspace root, so
    /// anchor to `CARGO_MANIFEST_DIR`'s grandparent.
    fn ws_path(rel: &str) -> String {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(rel)
            .to_string_lossy()
            .into_owned()
    }

    #[derive(Default)]
    struct FileResult {
        compared: usize,
        divergences: Vec<(String, String, String)>,
    }

    /// Compare overlay-composed vs full-build execution for one file. A file that
    /// fails to parse/typecheck/emit, or whose tests error on BOTH sides,
    /// contributes 0 comparisons rather than a divergence -- this oracle compares
    /// skull-vs-skull (overlay vs full), so a file skull can't compile is out of
    /// scope.
    fn overlay_check_file(file: &str) -> FileResult {
        let mut r = FileResult::default();
        let Ok(src) = fs_err::read_to_string(file) else { return r };
        let Ok(user) = skull_parser::parse_module(&src, ustr::Ustr::from(file)) else { return r };
        let user_file = user.file;
        let Ok(ws) = crate::bone_backend::check_with_stdlib([user]) else { return r };
        let mid = crate::bone_backend::entry_module_id(&ws, user_file);
        let Ok((full, mut cache)) = skull::mir_emit::emit_full(&ws, mid) else { return r };
        let Ok(tests) = skull::module_tests(&ws, mid) else { return r };
        if tests.is_empty() {
            return r;
        }

        // The maximal body edit: re-route every user function through the overlay.
        let reinferred: Vec<(bone_semantic::prelude::ModuleId, BodyOwner)> = ws
            .module(mid)
            .functions
            .iter()
            .map(|(fid, _)| (mid, BodyOwner::Function(fid)))
            .collect();
        let overlay = emit_overlay_checked(&ws, &mut cache, &reinferred);

        let stub = stub_for(&full.lua);
        for t in &tests {
            let call = format!("{}()", t.name);
            let composed = run_parts(&[&full.lua, stub, &overlay.lua, &call]);
            let fresh = run_parts(&[&full.lua, stub, &call]);
            match (composed, fresh) {
                (ExecOutcome::Output(c), ExecOutcome::Output(f)) => {
                    r.compared += 1;
                    let (nc, nf) = (normalize_stdout(&c), normalize_stdout(&f));
                    // Map `pairs()` order legitimately varies; same line-set in a
                    // different order is not a divergence.
                    if nc != nf && !super::is_order_only(&nc, &nf) {
                        r.divergences.push((file.to_string(), t.name.clone(), super::first_diff_line(&nf, &nc)));
                    }
                }
                // Both error: out of scope (skull may not run this test standalone).
                (ExecOutcome::Errored(_), ExecOutcome::Errored(_)) => {}
                (ExecOutcome::Output(_), ExecOutcome::Errored(_)) => {
                    r.compared += 1;
                    r.divergences.push((file.to_string(), t.name.clone(), "composed runs, full errors".into()));
                }
                (ExecOutcome::Errored(_), ExecOutcome::Output(_)) => {
                    r.compared += 1;
                    r.divergences.push((file.to_string(), t.name.clone(), "overlay BREAKS a test the full build runs".into()));
                }
            }
        }
        r
    }

    struct SweepTotals {
        files_with_tests: usize,
        compared: usize,
        divergences: Vec<(String, String, String)>,
    }

    /// Run `overlay_check_file` over a list, catching per-file bone panics (some
    /// inputs panic in inference). Returns totals.
    fn sweep(files: &[String]) -> SweepTotals {
        use rayon::prelude::*;
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let per: Vec<FileResult> = files
            .par_iter()
            .map(|f| {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| overlay_check_file(f)))
                    .unwrap_or_default()
            })
            .collect();
        std::panic::set_hook(prev);
        SweepTotals {
            compared: per.iter().map(|r| r.compared).sum(),
            files_with_tests: per.iter().filter(|r| r.compared > 0).count(),
            divergences: per.into_iter().flat_map(|r| r.divergences).collect(),
        }
    }

    /// Fast always-on subset: a curated spread of real code shapes. Asserts no
    /// divergence AND that a meaningful number of tests actually ran (guards
    /// against the suite silently skipping everything if emit/parse breaks).
    #[test]
    fn overlay_composition_curated_real_files() {
        let curated = [
            "tests/adversarial_generics.rock",
            "tests/adversarial_map.rock",
            "tests/adversarial_strings.rock",
            "tests/adversarial_overloads.rock",
            "tests/adversarial_newtypes.rock",
            "tests/adversarial_iterators.rock",
            "tests/adversarial_functions_lambdas.rock",
            "tests/adversarial_coroutines.rock",
            "tests/adversarial_nullable_refinement.rock",
        ];
        let present: Vec<String> = curated.iter().map(|s| ws_path(s)).filter(|f| std::path::Path::new(f).exists()).collect();
        assert!(present.len() >= 8, "curated corpus files missing: only {} present", present.len());
        let t = sweep(&present);
        assert!(
            t.divergences.is_empty(),
            "overlay composition diverged from full build on real files:\n{}",
            t.divergences.iter().map(|(f, ts, d)| format!("  {f}::{ts}: {d}")).collect::<Vec<_>>().join("\n")
        );
        assert!(
            t.files_with_tests >= 6 && t.compared >= 15,
            "too few real comparisons (files_with_tests={}, tests={}) -- emit/parse likely broke",
            t.files_with_tests, t.compared
        );
    }

    // --- sim tier: a registry retains and observes a CodeRef -------------------
    //
    // The corpus tier can't reach this: a `@update`/`@draw` decorator registers a
    // function VALUE into a registry (`UpdateSystem { func, name, stage }`). The
    // stored value is a CodeRef, so an overlay must refresh it without a
    // name-based registry repair.

    /// A minimal game-shaped module: one `@update` system whose body writes
    /// `tick_val` to an observable global, plus the engine's deco/registry/clear
    /// machinery and a frame runner. `run_frame()` drives the registry exactly as
    /// the engine's `run_update_systems` does (iterate, call `system.func()`).
    fn sim_fixture(tick_val: i32) -> String {
        format!(
            "struct UpdateSystem {{ func: fn() -> (), name: string, stage: int }}\n\
             global UPDATE_SYSTEMS: [UpdateSystem] = []\n\
             deco init update(stage: int = 100) {{\n\
                UPDATE_SYSTEMS.push(UpdateSystem {{ func: transmute(target.func), name: transmute(target.name), stage: stage }})\n\
             }}\n\
             global TICK_OUT: int = 0\n\
             @update\n\
             fn tick() {{ TICK_OUT = {tick_val} }}\n\
             @nomangle\n\
             fn run_frame() {{ for s in UPDATE_SYSTEMS {{ s.func() }} }}\n"
        )
    }

    /// Run the registry-driving frame and report the observable global.
    const SIM_DRIVER: &str = "run_frame() print(TICK_OUT)";

    /// An existing lifecycle entry must invoke the edited body through its
    /// retained CodeRef. The overlay contains no decorator rebind call.
    #[test]
    fn sim_overlay_refreshes_existing_code_ref_registry_entry() {
        let v0 = sim_fixture(1);
        let v1 = sim_fixture(2);

        let mut session = bone_session(files(&v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");

        // Baseline: the registry runs the v0 body.
        match run_parts(&[&full0.lua, SIM_DRIVER]) {
            ExecOutcome::Output(o) => assert_eq!(normalize_stdout(&o).trim(), "1", "baseline frame should run the v0 system body"),
            ExecOutcome::Errored(_) => panic!("baseline fixture failed to run"),
        }

        let result = session.update_file("main.rock", &v1).expect("update v1");
        // The decorated body edit must re-infer the body (else the overlay can't
        // re-emit it). Guards the bone `lower.rs` footprint fix.
        assert_eq!(
            result.reinferred.len(), 1,
            "a decorated-fn body edit must re-infer exactly the edited body"
        );
        let overlay = emit_overlay_checked(session.workspace(), &mut cache, &result.reinferred);
        // The overlay carries the redefined body and slot publication only.
        assert!(
            overlay.lua.contains("function tick_ret_unit()")
                && !overlay.lua.contains("__rock_rebind_system"),
            "overlay must redefine the body without a name-based registry repair:\n{}",
            overlay.lua
        );
        let fresh = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v1").0;

        let composed = run_parts(&[&full0.lua, &overlay.lua, SIM_DRIVER]);
        let fresh_run = run_parts(&[&fresh.lua, SIM_DRIVER]);
        match (composed, fresh_run) {
            (ExecOutcome::Output(c), ExecOutcome::Output(f)) => {
                assert_eq!(normalize_stdout(&f).trim(), "2", "fresh full build should run the v1 body");
                assert_eq!(
                    normalize_stdout(&c).trim(),
                    "2",
                    "the retained registry CodeRef must run the EDITED decorated body"
                );
            }
            _ => panic!("composed or fresh run errored"),
        }
    }

    /// Slot publication belongs to the OUTER successful overlay boundary. A
    /// later top-level initializer failure must leave an already-stored value
    /// on v0, even though the overlay has staged the edited callback body.
    #[test]
    fn failed_overlay_after_late_top_level_work_keeps_retained_code_ref_old() {
        let v0 = format!(
            "const LATE: int = 0\nfn explode() -> int {{ error(\"late overlay failure\") return 0 }}\n{}",
            sim_fixture(1).replace("TICK_OUT = 1", "TICK_OUT = 1 + LATE")
        );
        let v1 = format!(
            "const LATE: int = explode()\nfn explode() -> int {{ error(\"late overlay failure\") return 0 }}\n{}",
            sim_fixture(2).replace("TICK_OUT = 2", "TICK_OUT = 2 + LATE")
        );
        let mut session = bone_session(files(&v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");

        let result = session.update_file("main.rock", &v1).expect("update v1");
        let overlay = emit_overlay_checked(session.workspace(), &mut cache, &result.reinferred);
        let late_line = overlay
            .lua
            .lines()
            .position(|line| line.trim_start().starts_with("LATE ="))
            .expect("v1 overlay must execute the changed top-level initializer");
        assert!(
            overlay.lua.contains("__rock_code_slot_stage")
                && overlay.lua.contains("__rock_code_slot_publish"),
            "the failing overlay must stage a compatible callback body:\n{}",
            overlay.lua
        );

        let mut failing = String::new();
        for (index, line) in overlay.lua.lines().enumerate() {
            if index == late_line {
                failing.push_str("error('late overlay failure')\n");
            }
            failing.push_str(line);
            failing.push('\n');
        }
        let script = format!(
            "{}\n{}\nlocal ok, err = xpcall(function()\n{}end, debug.traceback)\nif not ok then __rock_code_slot_abort() end\nprint(ok)\nprint(_G.__rock_code_slot_pending == nil)\nrun_frame()\nprint(TICK_OUT)",
            full0.lua, SYS_STUB, failing
        );

        match exec_lua(&script) {
            ExecOutcome::Output(output) => assert_eq!(
                normalize_stdout(&output).trim(),
                "false\ntrue\n1",
                "a failed overlay must report failure and keep the retained v0 CodeRef"
            ),
            ExecOutcome::Errored(diagnostic) => {
                panic!("the loader failure must be caught so the old CodeRef can run: {diagnostic}")
            }
        }
    }

    #[test]
    fn wire_hook_failure_is_post_publication_and_not_reported_as_rollback() {
        let v0 = r#"
@lifecycle(phase="wire", owner="session")
deco init wire {}
const DEP: int = 1
global RUNS: int = 0
@wire(onchange=[DEP]) fn gated_wire() { RUNS = RUNS + 1 }
fn main() {}
"#;
        let v1 = v0.replace(
            "RUNS = RUNS + 1",
            "error(\"wire hook failed after publication\")",
        );
        let mut session = bone_session(files(v0)).expect("wire session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid)
            .expect("wire full v0");
        let result = session.update_file("main.rock", &v1).expect("wire body edit");
        let overlay = emit_overlay_checked(session.workspace(), &mut cache, &result.reinferred);
        let driver = r#"
local function run_wire()
  local phase = _G.__ROCK_LIFECYCLE_PLAN.wire
  for i = 1, phase.len do
    local entry = phase._data[i]
    if __rock_lifecycle_take_wire(entry.key) then entry.func() end
  end
end
run_wire()
assert(__rock_state_get("global:main.rock::RUNS::value") == 1, "initial wire did not run exactly once: " .. tostring(__rock_state_get("global:main.rock::RUNS::value")))
local old_plan = _G.__ROCK_LIFECYCLE_PLAN
REPLACE_OVERLAY
local candidate_plan = _G.__ROCK_LIFECYCLE_PLAN
assert(candidate_plan ~= old_plan, "candidate LifecyclePlan was not published")
local ok, err = pcall(run_wire)
assert(not ok and tostring(err):find("wire hook failed after publication", 1, true), "wire failure was not observed: " .. tostring(err))
assert(_G.__rock_code_slot_pending == nil, "the hook failure must not leave a staging transaction")
assert(_G.__ROCK_LIFECYCLE_PLAN == candidate_plan, "hook failure must not claim publication rollback")
assert(__rock_state_get("global:main.rock::RUNS::value") == 1, "wire failure changed the retained effect")
print("WIRE_HOOK_FAILURE_POST_PUBLICATION")
"#
        .replace("REPLACE_OVERLAY", &overlay.lua);
        let script = format!("{}\n{}\n{}", full0.lua, SYS_STUB, driver);
        match exec_lua(&script) {
            ExecOutcome::Output(output) => {
                assert!(normalize_stdout(&output).contains("WIRE_HOOK_FAILURE_POST_PUBLICATION"))
            }
            ExecOutcome::Errored(error) => panic!("wire hook failure driver failed: {error}"),
        }
    }

    /// A `co fn` BODY edit, delivered through a REAL `Session.update_file`, must
    /// re-emit the coroutine into the overlay so a fresh invocation runs the new
    /// body -- exactly like an ordinary fn. The corpus sweep can't catch this: it
    /// fabricates `reinferred` from every function directly, bypassing the Session
    /// (where the fast-path body reparse slices the item from `fn`, dropping `co`).
    /// This drives the genuine edit pipeline end to end.
    #[test]
    fn coroutine_body_edit_overlay_runs_new_body() {
        let v0 = "co fn counter() -> Coroutine<int> {\n  yield 1\n  yield 2\n}\n\
                  fn main() {\n  let c = counter()\n  let a = c.resume() ?? -1\n  let b = c.resume() ?? -1\n  print(\"{a} {b}\")\n}\n";
        let v1 = "co fn counter() -> Coroutine<int> {\n  yield 10\n  yield 20\n}\n\
                  fn main() {\n  let c = counter()\n  let a = c.resume() ?? -1\n  let b = c.resume() ?? -1\n  print(\"{a} {b}\")\n}\n";

        let mut session = bone_session(files(v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");
        match run_parts(&[&full0.lua, stub_for(&full0.lua), "main()"]) {
            ExecOutcome::Output(o) => assert_eq!(normalize_stdout(&o).trim(), "1 2", "baseline co fn body"),
            ExecOutcome::Errored(_) => panic!("baseline coroutine fixture failed to run"),
        }

        let result = session.update_file("main.rock", v1).expect("update v1");
        // The co fn body edit must re-infer exactly the edited body.
        assert_eq!(result.reinferred.len(), 1, "a co fn body edit re-infers exactly one body");

        let overlay = emit_overlay_checked(session.workspace(), &mut cache, &result.reinferred);
        // The overlay must carry the re-emitted coroutine (its top-level step fn).
        assert!(
            overlay.lua.contains("counter_ret_Coroutine_int_unit_unit__step("),
            "overlay must re-emit the coroutine's step fn:\n{}",
            overlay.lua
        );

        let fresh = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v1").0;
        let composed = run_parts(&[&full0.lua, stub_for(&full0.lua), &overlay.lua, "main()"]);
        let fresh_run = run_parts(&[&fresh.lua, stub_for(&fresh.lua), "main()"]);
        match (composed, fresh_run) {
            (ExecOutcome::Output(c), ExecOutcome::Output(f)) => {
                assert_eq!(normalize_stdout(&f).trim(), "10 20", "fresh full build runs the v1 co fn body");
                assert_eq!(
                    normalize_stdout(&c).trim(),
                    "10 20",
                    "overlay must run the EDITED coroutine body, not the stale one"
                );
            }
            _ => panic!("composed or fresh coroutine run errored"),
        }
    }

    /// A FLAT-ENUM-yielding `co fn` body edit, through the overlay, must re-emit the
    /// frame-slot yield channel and CodeRefs for the ctor's `__advance`/`__resume`
    /// fields, then a freshly-created coroutine must run the new body,
    /// threading the yield through the slots -- no `{__variant}` box on the concrete
    /// consumer path. Guards the frame-slot channel (`docs/coroutine-allocation-plan.md`
    /// Part A) against a hot-reload regression; the scalar-yield test above does not
    /// exercise the channel (int yields keep the multi-return path).
    #[test]
    fn flat_enum_coroutine_body_edit_overlay_threads_and_runs_new_body() {
        // The `Act(() -> ())` closure payload makes Ev NON-cdata-eligible, so it is a
        // genuinely FLAT (boxed) enum that rides the frame-slot channel -- an
        // all-scalar-payload enum would scalarize as a `_ct` cdata instead (mirrors
        // `tests/enum_thread_yield.rock`).
        let prog = |a: i32, b: i32| {
            format!(
                "enum Ev {{ Tick(int), Pair(int, int), Done, Act(() -> ()) }}\n\
                 co fn gen() -> Coroutine<Ev> {{\n  yield Ev::Tick({a})\n  yield Ev::Pair({a}, {b})\n  yield Ev::Done\n}}\n\
                 fn score(e: Ev) -> int {{\n  return match e {{\n    Ev::Tick(x) => x\n    Ev::Pair(p, q) => p + q\n    Ev::Done => 100\n    Ev::Act(_) => -1\n  }}\n}}\n\
                 fn main() {{\n  let c = gen()\n  let e0 = c.resume()\n  let e1 = c.resume()\n  let s0 = if e0 != null {{ score(e0) }} else {{ -1 }}\n  let s1 = if e1 != null {{ score(e1) }} else {{ -1 }}\n  print(\"{{s0}} {{s1}}\")\n}}\n"
            )
        };
        let (v0, v1) = (prog(1, 2), prog(10, 20));

        let mut session = bone_session(files(&v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");
        match run_parts(&[&full0.lua, stub_for(&full0.lua), "main()"]) {
            ExecOutcome::Output(o) => assert_eq!(normalize_stdout(&o).trim(), "1 3", "baseline flat-enum co fn (Tick(1)=1, Pair(1,2)=3)"),
            ExecOutcome::Errored(_) => panic!("baseline flat-enum coroutine fixture failed to run"),
        }

        let result = session.update_file("main.rock", &v1).expect("update v1");
        let overlay = emit_overlay_checked(session.workspace(), &mut cache, &result.reinferred);
        // The overlay must re-emit the whole frame-slot channel.
        assert!(overlay.lua.contains("gen_ret_Coroutine_Ev_unit_unit__step("), "overlay must re-emit the step fn:\n{}", overlay.lua);
        assert!(overlay.lua.contains(".__y0 = "), "overlay step must write yield leaves to frame slots:\n{}", overlay.lua);
        assert!(
            overlay.lua.contains("function gen_ret_Coroutine_Ev_unit_unit__advance(")
                && overlay.lua.contains("__rock_code_slot_ref(\"generated:main.rock::gen<>::advance\")")
                && overlay.lua.contains("__rock_code_slot_ref(\"generated:main.rock::gen<>::resume\")"),
            "overlay must re-emit the per-co-fn __advance/__resume fns + CodeRefs in the ctor:\n{}",
            overlay.lua
        );

        let fresh = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v1").0;
        let composed = run_parts(&[&full0.lua, stub_for(&full0.lua), &overlay.lua, "main()"]);
        let fresh_run = run_parts(&[&fresh.lua, stub_for(&fresh.lua), "main()"]);
        match (composed, fresh_run) {
            (ExecOutcome::Output(c), ExecOutcome::Output(f)) => {
                assert_eq!(normalize_stdout(&f).trim(), "10 30", "fresh v1: Tick(10)=10, Pair(10,20)=30");
                assert_eq!(
                    normalize_stdout(&c).trim(),
                    "10 30",
                    "overlay must run the EDITED flat-enum body via the re-emitted frame-slot channel"
                );
            }
            _ => panic!("composed or fresh flat-enum coroutine run errored"),
        }
    }

    /// The load-bearing hot-reload case: a flat-enum coroutine created + first-resumed
    /// under v0, then resumed again ACROSS a body-edit overlay on the SAME live object
    /// (the engine's per-frame poll pattern). The live object keeps its v0
    /// `__advance`/`__resume` closures, but `__advance` calls `gen_ret_Coroutine_Ev_unit_unit__step` by GLOBAL
    /// name, so a mid-flight resume runs the NEW step -- writing the new yield leaves
    /// into the SAME reused `__y*` slots on the object/frame (the enum shape is
    /// unchanged), which the (baseline, unchanged) `pump` reads directly. If
    /// `__advance` had captured the step by value, resume #2 would run the stale v0
    /// body (Pair(1,2)=3, not Pair(10,20)=30).
    #[test]
    fn live_flat_enum_coroutine_survives_overlay_and_threads_new_body() {
        // `Act(() -> ())` forces Ev to be a FLAT (boxed) enum on the frame-slot
        // channel (an all-scalar-payload enum would be a `_ct` cdata instead).
        let prog = |a: i32, b: i32| {
            format!(
                "enum Ev {{ Tick(int), Pair(int, int), Done, Act(() -> ()) }}\n\
                 global GEN: Coroutine<Ev>? = null\n\
                 co fn gen() -> Coroutine<Ev> {{\n  yield Ev::Tick({a})\n  yield Ev::Pair({a}, {b})\n  yield Ev::Done\n}}\n\
                 fn score(e: Ev) -> int {{\n  return match e {{\n    Ev::Tick(x) => x\n    Ev::Pair(p, q) => p + q\n    Ev::Done => 100\n    Ev::Act(_) => -1\n  }}\n}}\n\
                 @nomangle\n\
                 fn start() {{ GEN = gen() }}\n\
                 @nomangle\n\
                 fn pump() {{\n  let c = GEN\n  if c != null {{\n    let e = c.resume()\n    if e != null {{ print(\"{{score(e)}}\") }} else {{ print(\"-1\") }}\n  }}\n}}\n"
            )
        };
        let (v0, v1) = (prog(1, 2), prog(10, 20));

        let mut session = bone_session(files(&v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");

        let result = session.update_file("main.rock", &v1).expect("update v1");
        let overlay = emit_overlay_checked(session.workspace(), &mut cache, &result.reinferred);

        // start() + pump() under v0 (Tick(1)=1), then overlay, then pump() x3 on the
        // SAME live coroutine under v1: Pair(10,20)=30, Done=100, then done -> -1.
        let composed = run_parts(&[
            &full0.lua,
            stub_for(&full0.lua),
            "start()",
            "pump()",
            &overlay.lua,
            "pump()",
            "pump()",
            "pump()",
        ]);
        match composed {
            ExecOutcome::Output(o) => assert_eq!(
                normalize_stdout(&o).trim(),
                "1\n30\n100\n-1",
                "live coroutine: v0 first yield (Tick=1), then v1 body across the reload (Pair=30, Done=100, done)"
            ),
            ExecOutcome::Errored(_) => panic!("live flat-enum coroutine reload fixture failed to run"),
        }
    }

    /// Layout drift, cell shift: inserting a frame-resident local changes the
    /// StateSchema frame layout, so an overlay must request a safe restart
    /// before any new CodeRef can reach a live process. The inserted `z` is a GENUINE frame cell
    /// (read after the first yield, so it must survive suspension); a dead local
    /// -- `let z = 999` never read -- is now a ghost dropped from the frame and
    /// does NOT shift the layout (the P2-finding fix; that path has its own
    /// structural test in skull/tests/frame_slot_naming.rs).
    /// docs/coroutine-allocation-plan.md "Layout drift across reloads".
    #[test]
    fn live_coroutine_cell_layout_shift_requests_safe_restart() {
        // `Act(() -> ())` forces Ev flat (frame-slot channel); `mk()` keeps the
        // locals as real calls so copy-prop/DCE can't erase the frame cells.
        // v1 inserts `z` AND reads it after yield 1 (`a = a + z`), so `z` is a
        // real frame cell that shifts `b`'s slot -- a genuine layout change.
        let v0 = "enum Ev { Tick(int), Pair(int, int), Done, Act(() -> ()) }\n\
                  global GEN: Coroutine<Ev>? = null\n\
                  fn mk(n: int) -> int { return n }\n\
                  co fn gen() -> Coroutine<Ev> {\n  let a = mk(7)\n  let b = mk(35)\n  yield Ev::Tick(a)\n  yield Ev::Pair(a, b)\n  yield Ev::Done\n}\n\
                  fn score(e: Ev) -> int {\n  return match e {\n    Ev::Tick(x) => x\n    Ev::Pair(p, q) => p + q\n    Ev::Done => 100\n    Ev::Act(_) => -1\n  }\n}\n\
                  @nomangle\n\
                  fn start() { GEN = gen() }\n\
                  @nomangle\n\
                  fn pump() {\n  let c = GEN\n  if c != null {\n    let e = c.resume()\n    if e != null { print(\"{score(e)}\") } else { print(\"-1\") }\n  }\n}\n";
        let v1 = "enum Ev { Tick(int), Pair(int, int), Done, Act(() -> ()) }\n\
                  global GEN: Coroutine<Ev>? = null\n\
                  fn mk(n: int) -> int { return n }\n\
                  co fn gen() -> Coroutine<Ev> {\n  let a = mk(7)\n  let z = mk(999)\n  let b = mk(35)\n  yield Ev::Tick(a)\n  a = a + z\n  yield Ev::Pair(a, b)\n  yield Ev::Done\n}\n\
                  fn score(e: Ev) -> int {\n  return match e {\n    Ev::Tick(x) => x\n    Ev::Pair(p, q) => p + q\n    Ev::Done => 100\n    Ev::Act(_) => -1\n  }\n}\n\
                  @nomangle\n\
                  fn start() { GEN = gen() }\n\
                  @nomangle\n\
                  fn pump() {\n  let c = GEN\n  if c != null {\n    let e = c.resume()\n    if e != null { print(\"{score(e)}\") } else { print(\"-1\") }\n  }\n}\n";

        let mut session = bone_session(files(v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (_full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");

        let result = session.update_file("main.rock", v1).expect("update v1");
        assert!(
            matches!(result.tier, bone_incremental::session::UpdateTier::BodyOnly),
            "inserting a let is a body edit -- must be BodyOnly (the premise), got {:?}",
            result.tier
        );
        let err = match skull::mir_emit::emit_overlay(session.workspace(), &mut cache, &result.reinferred) {
            Ok(_) => panic!("a changed coroutine frame schema must not overlay"),
            Err(err) => err,
        };
        assert!(
            err.message.contains("safe restart"),
            "a cell-shifting frame edit must request the safe restart path, got: {}",
            err.message
        );
    }

    /// Layout drift, state renumber: yields are numbered by order, so a BodyOnly
    /// overlay that ADDS a yield point renumbers the state machine and a live
    /// coroutine's `__state` would dispatch to the wrong resume point (re-run or
    /// skip code) -- the stamp's per-state variant tags catch it and kill the
    /// coroutine cleanly. Cells are deliberately untouched (const payloads only)
    /// so this pins the STATE component of the stamp specifically.
    /// docs/coroutine-allocation-plan.md "Layout drift across reloads".
    #[test]
    fn live_coroutine_yield_renumber_overlay_stamp_kills() {
        let prog = |extra: &str| {
            format!(
                "enum Ev {{ Tick(int), Pair(int, int), Done, Act(() -> ()) }}\n\
                 global GEN: Coroutine<Ev>? = null\n\
                 co fn gen() -> Coroutine<Ev> {{\n{extra}  yield Ev::Tick(1)\n  print(\"fire\")\n  yield Ev::Tick(2)\n  print(\"cleanup\")\n}}\n\
                 fn score(e: Ev) -> int {{\n  return match e {{\n    Ev::Tick(x) => x\n    Ev::Pair(p, q) => p + q\n    Ev::Done => 100\n    Ev::Act(_) => -1\n  }}\n}}\n\
                 @nomangle\n\
                 fn start() {{ GEN = gen() }}\n\
                 @nomangle\n\
                 fn pump() {{\n  let c = GEN\n  if c != null {{\n    let e = c.resume()\n    if e != null {{ print(\"{{score(e)}}\") }} else {{ print(\"-1\") }}\n  }}\n}}\n"
            )
        };
        let (v0, v1) = (prog(""), prog("  yield Ev::Done\n"));

        let mut session = bone_session(files(&v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");

        let result = session.update_file("main.rock", &v1).expect("update v1");
        assert!(
            matches!(result.tier, bone_incremental::session::UpdateTier::BodyOnly),
            "adding a yield is a body edit -- must be BodyOnly (the premise), got {:?}",
            result.tier
        );
        let overlay = emit_overlay_checked(session.workspace(), &mut cache, &result.reinferred);

        // v0: pump() -> Tick(1)=1, suspended at state 1 (before print("fire")).
        // Overlay renumbers: the new Ev::Done yield becomes state 1, so the live
        // frame's __state=1 would resume at the WRONG yield point (pre-stamp it
        // replayed the already-delivered Tick(1), then ran fire/cleanup a pump
        // late). The per-state variant tags in the layout stamp catch the
        // renumbering: resume #2 stamp-kills the coroutine -> -1, no replay.
        let composed = run_parts(&[&full0.lua, stub_for(&full0.lua), "start()", "pump()", &overlay.lua, "pump()", "pump()", "pump()"]);
        match composed {
            ExecOutcome::Output(o) => assert_eq!(
                normalize_stdout(&o).trim(),
                "1\n-1\n-1\n-1",
                "a yield-renumbering overlay must stamp-kill the live coroutine, not teleport its resume point"
            ),
            ExecOutcome::Errored(_) => panic!("yield-renumber + live resume errored (expected the clean kill output)"),
        }
    }

    /// `await` under hot reload: a live outer+inner delegation chain must pick up
    /// a BodyOnly edit to the INNER co fn's body. The chain is advance-by-field
    /// (outer holds the inner object) + step-by-global-name (inner's stale advance
    /// resolves `inner__step` at call time), and both mechanisms must compose
    /// across the delegation boundary. Guards the Part B kill/advance/delegate
    /// refactor (docs/coroutine-allocation-plan.md).
    #[test]
    fn live_yield_star_delegation_survives_inner_body_overlay() {
        let prog = |a: i32, b: i32| {
            format!(
                "enum Ev {{ Tick(int), Act(() -> ()) }}\n\
                 global GEN: Coroutine<Ev>? = null\n\
                 co fn inner() -> Coroutine<Ev> {{\n  yield Ev::Tick({a})\n  yield Ev::Tick({b})\n}}\n\
                 co fn outer() -> Coroutine<Ev> {{\n  await inner()\n}}\n\
                 fn score(e: Ev) -> int {{\n  return match e {{\n    Ev::Tick(x) => x\n    Ev::Act(_) => -1\n  }}\n}}\n\
                 @nomangle\n\
                 fn start() {{ GEN = outer() }}\n\
                 @nomangle\n\
                 fn pump() {{\n  let c = GEN\n  if c != null {{\n    let e = c.resume()\n    if e != null {{ print(\"{{score(e)}}\") }} else {{ print(\"-1\") }}\n  }}\n}}\n"
            )
        };
        let (v0, v1) = (prog(1, 2), prog(10, 20));

        let mut session = bone_session(files(&v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");

        let result = session.update_file("main.rock", &v1).expect("update v1");
        assert!(
            matches!(result.tier, bone_incremental::session::UpdateTier::BodyOnly),
            "a constant tweak in the inner co fn must be BodyOnly, got {:?}",
            result.tier
        );
        let overlay = emit_overlay_checked(session.workspace(), &mut cache, &result.reinferred);

        // pump #1 under v0: outer primes inner, re-yields inner's Tick(1). Overlay
        // (layout-stable: constants only). pump #2 resumes the live chain: the
        // inner's stale advance runs the NEW inner__step -> Tick(20). pump #3:
        // inner done -> outer's delegate exits, outer completes -> -1.
        let composed = run_parts(&[&full0.lua, stub_for(&full0.lua), "start()", "pump()", &overlay.lua, "pump()", "pump()"]);
        match composed {
            ExecOutcome::Output(o) => assert_eq!(
                normalize_stdout(&o).trim(),
                "1\n20\n-1",
                "a live await chain must thread a BodyOnly inner edit (v0 Tick(1), then v1 Tick(20), then done)"
            ),
            ExecOutcome::Errored(_) => panic!("live await delegation reload fixture failed to run"),
        }
    }

    /// A stamp-kill (or ANY error) of an outer coroutine suspended mid-`await`
    /// must chain into the delegated inner: explicit `kill()` recurses through
    /// `__inner` (firing the inner's on_killed), and the error path must match --
    /// otherwise the inner is orphaned and its cleanup hook silently never runs
    /// (found live: mag mob soft-locked in attack state after a reload).
    #[test]
    fn stamp_killed_outer_chains_kill_into_delegated_inner() {
        let prog = |extra: &str| {
            format!(
                "enum Ev {{ Tick(int), Act(() -> ()) }}\n\
                 global GEN: Coroutine<Ev>? = null\n\
                 global INNER_KILLED: int = 0\n\
                 fn mk(n: int) -> int {{ return n }}\n\
                 fn note_inner_killed() {{ INNER_KILLED = INNER_KILLED + 1 }}\n\
                 co fn inner() -> Coroutine<Ev> {{\n  on_killed(() => note_inner_killed())\n  yield Ev::Tick(1)\n  yield Ev::Tick(2)\n}}\n\
                 co fn outer() -> Coroutine<Ev> {{\n{extra}  await inner()\n}}\n\
                 fn score(e: Ev) -> int {{\n  return match e {{\n    Ev::Tick(x) => x\n    Ev::Act(_) => -1\n  }}\n}}\n\
                 @nomangle\n\
                 fn start() {{ GEN = outer() }}\n\
                 @nomangle\n\
                 fn pump() {{\n  let c = GEN\n  if c != null {{\n    let e = c.resume()\n    if e != null {{ print(\"{{score(e)}}\") }} else {{ print(\"-1\") }}\n  }}\n}}\n\
                 @nomangle\n\
                 fn report() {{ print(\"{{INNER_KILLED}}\") }}\n"
            )
        };
        let (v0, v1) = (prog(""), prog("  let z = mk(9)\n"));

        let mut session = bone_session(files(&v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");
        let result = session.update_file("main.rock", &v1).expect("update v1");
        assert!(
            matches!(result.tier, bone_incremental::session::UpdateTier::BodyOnly),
            "outer's added local must be BodyOnly, got {:?}",
            result.tier
        );
        let overlay = emit_overlay_checked(session.workspace(), &mut cache, &result.reinferred);

        // pump #1: outer primes inner -> Tick(1). Overlay shifts OUTER's layout.
        // pump #2: outer stamp-killed -> -1, and the inner's on_killed must fire.
        let composed = run_parts(&[&full0.lua, stub_for(&full0.lua), "start()", "pump()", &overlay.lua, "pump()", "report()"]);
        match composed {
            ExecOutcome::Output(o) => assert_eq!(
                normalize_stdout(&o).trim(),
                "1\n-1\n1",
                "a stamp-killed outer must chain-kill its delegated inner (inner's on_killed fires exactly once)"
            ),
            ExecOutcome::Errored(_) => panic!("stamp-kill inner-chain fixture failed to run"),
        }
    }

    /// A layout-changing coroutine edit must be refused before publication;
    /// preserving the old Session is what keeps the creation-time defer and its
    /// live frame coherent. The defer behavior itself remains covered by the
    /// ordinary coroutine lifecycle tests.
    #[test]
    fn coroutine_frame_layout_change_refuses_before_publication() {
        // v1 inserts `z` and reads it after the first yield (`a = a + z`), so `z`
        // is a genuine frame cell shifting `b`'s slot -- a real layout change.
        // (A dead `let z = 999` is now a ghost that does NOT shift the frame.)
        let prog = |extra: &str, cross: &str| {
            format!(
                "enum Ev {{ Tick(int), Act(() -> ()) }}\n\
                 global GEN: Coroutine<Ev>? = null\n\
                 global BUSY: int = 0\n\
                 fn mk(n: int) -> int {{ return n }}\n\
                 co fn gen() -> Coroutine<Ev> {{\n  BUSY = 1\n  defer {{ BUSY = 0 }}\n  let a = mk(7)\n{extra}  let b = mk(35)\n  yield Ev::Tick(a)\n{cross}  yield Ev::Tick(b)\n}}\n\
                 fn score(e: Ev) -> int {{\n  return match e {{\n    Ev::Tick(x) => x\n    Ev::Act(_) => -1\n  }}\n}}\n\
                 @nomangle\n\
                 fn start() {{ GEN = gen() }}\n\
                 @nomangle\n\
                 fn pump() {{\n  let c = GEN\n  if c != null {{\n    let e = c.resume()\n    if e != null {{ print(\"{{score(e)}}\") }} else {{ print(\"-1\") }}\n  }}\n}}\n\
                 @nomangle\n\
                 fn report() {{ print(\"busy:{{BUSY}}\") }}\n"
            )
        };
        let (v0, v1) = (prog("", ""), prog("  let z = mk(999)\n", "  a = a + z\n"));

        let mut session = bone_session(files(&v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (_full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");
        let result = session.update_file("main.rock", &v1).expect("update v1");
        assert!(matches!(result.tier, bone_incremental::session::UpdateTier::BodyOnly));
        let err = match skull::mir_emit::emit_overlay(session.workspace(), &mut cache, &result.reinferred) {
            Ok(_) => panic!("a changed coroutine frame schema must not overlay"),
            Err(err) => err,
        };
        assert!(
            err.message.contains("safe restart"),
            "a frame-layout edit must request the safe restart path, got: {}",
            err.message
        );
    }

    /// A coroutine hook (`on_killed`/`on_resume`/`on_hot_reload`) argument must
    /// be a lambda literal: the hook is peeled to the constructor, which can
    /// only re-render a lambda with the frame context. Any other fn-typed
    /// expression used to SILENTLY install no hook (docs/unfuck-backlog.md) --
    /// it must be a compile error instead.
    #[test]
    fn non_lambda_coroutine_hook_arg_rejected() {
        let src = "enum Ev { Tick(int), Act(() -> ()) }\n\
                   fn cleanup() { print(\"x\") }\n\
                   co fn gen() -> Coroutine<Ev> {\n  on_killed(cleanup)\n  yield Ev::Tick(1)\n}\n\
                   fn main() { let c = gen() }\n";
        let session = bone_session(files(src)).expect("session");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        match skull::mir_emit::emit_full(session.workspace(), mid) {
            Err(e) => assert!(
                format!("{e:?}").contains("lambda"),
                "rejection must say the hook needs a lambda, got: {e:?}"
            ),
            Ok((full, _)) => {
                // Per-fn skips are also an acceptable surface -- but the co fn
                // must NOT emit with a silently-dropped hook.
                assert!(
                    !full.lua.contains("function gen_ret_Coroutine_Ev_unit_unit__step("),
                    "a co fn with a non-lambda hook arg must not emit:\n{}",
                    full.lua
                );
            }
        }
    }

    /// `yield` inside a `defer` body is a compile error (a defer body cannot
    /// suspend -- it may run during terminal cleanup with no resume protocol).
    #[test]
    fn yield_inside_defer_body_rejected() {
        let src = "enum Ev { Tick(int), Act(() -> ()) }\n\
                   co fn gen() -> Coroutine<Ev> {\n  defer { yield Ev::Tick(1) }\n  yield Ev::Tick(2)\n}\n\
                   fn main() { let c = gen() }\n";
        let session = bone_session(files(src)).expect("session");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let err = skull::mir_emit::emit_full(session.workspace(), mid);
        match err {
            Err(e) => assert!(
                format!("{e:?}").contains("defer"),
                "rejection must mention defer, got: {e:?}"
            ),
            Ok((full, _)) => {
                // Emission collects per-fn skips rather than failing the whole
                // unit -- accept either surface, but the co fn must NOT emit.
                assert!(
                    !full.lua.contains("function gen_ret_Coroutine_Ev_unit_unit__step("),
                    "a co fn with `yield` inside a defer body must not emit:\n{}",
                    full.lua
                );
            }
        }
    }

    /// Layout drift via the Interface tier (which does NOT protect live
    /// coroutines): a full rebuild is a re-`dofile`, not a restart -- mutable
    /// globals carry the `_G.HOT_RELOAD` preservation guard, so a live coroutine
    /// held in a global SURVIVES the full reload, and its stale `__advance`
    /// resolves the step by global name into the NEW program. A realistic
    /// Interface edit (widen a variant + use the new payload, which also inserts
    /// a local) would then drive the old frame with a step compiled for a
    /// different layout -- the stamp kills the survivor cleanly instead.
    /// docs/coroutine-allocation-plan.md "Layout drift across reloads".
    #[test]
    fn live_coroutine_survives_full_reload_and_shape_edit_stamp_kills() {
        let v0 = "enum Ev { Tick(int), Pair(int, int), Done, Act(() -> ()) }\n\
                  global GEN: Coroutine<Ev>? = null\n\
                  fn mk(n: int) -> int { return n }\n\
                  co fn gen() -> Coroutine<Ev> {\n  let a = mk(7)\n  let b = mk(35)\n  yield Ev::Tick(a)\n  yield Ev::Pair(a, b)\n  yield Ev::Done\n}\n\
                  fn score(e: Ev) -> int {\n  return match e {\n    Ev::Tick(x) => x\n    Ev::Pair(p, q) => p + q\n    Ev::Done => 100\n    Ev::Act(_) => -1\n  }\n}\n\
                  @nomangle\n\
                  fn start() { GEN = gen() }\n\
                  @nomangle\n\
                  fn pump() {\n  let c = GEN\n  if c != null {\n    let e = c.resume()\n    if e != null { print(\"{score(e)}\") } else { print(\"-1\") }\n  }\n}\n";
        // Widen Tick BEYOND the current max payload width (Pair's 2 -> Tick's 3),
        // so the frame-slot yield channel genuinely grows -- the shape edit shifts
        // the layout stamp (a within-width widen would be absorbed by the union
        // channel and, post-P2-fix, is correctly layout-stable). The enum edit is
        // Interface tier, delivered via a FULL rebuild.
        let v1 = "enum Ev { Tick(int, int, int), Pair(int, int), Done, Act(() -> ()) }\n\
                  global GEN: Coroutine<Ev>? = null\n\
                  fn mk(n: int) -> int { return n }\n\
                  co fn gen() -> Coroutine<Ev> {\n  let a = mk(7)\n  let b = mk(35)\n  yield Ev::Tick(a, b, a)\n  yield Ev::Pair(a, b)\n  yield Ev::Done\n}\n\
                  fn score(e: Ev) -> int {\n  return match e {\n    Ev::Tick(x, y, z) => x + y + z\n    Ev::Pair(p, q) => p + q\n    Ev::Done => 100\n    Ev::Act(_) => -1\n  }\n}\n\
                  @nomangle\n\
                  fn start() { GEN = gen() }\n\
                  @nomangle\n\
                  fn pump() {\n  let c = GEN\n  if c != null {\n    let e = c.resume()\n    if e != null { print(\"{score(e)}\") } else { print(\"-1\") }\n  }\n}\n";

        let mut session = bone_session(files(v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, _cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");

        let result = session.update_file("main.rock", v1).expect("update v1");
        assert!(
            matches!(
                result.tier,
                bone_incremental::session::UpdateTier::Interface
                    | bone_incremental::session::UpdateTier::FullUser
            ),
            "an enum shape edit must be a full rebuild (the premise), got {:?}",
            result.tier
        );
        let full1 = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v1").0;

        // Premise (non-inverted, stays green): the live coroutine SURVIVES the full
        // re-dofile -- the mutable-global preservation guard keeps GEN.
        let survival = run_parts(&[
            &full0.lua,
            stub_for(&full0.lua),
            "start()",
            "pump()",
            "_G.HOT_RELOAD = true",
            &full1.lua,
            "print(_G.GEN ~= nil)",
        ]);
        match survival {
            ExecOutcome::Output(o) => assert_eq!(
                normalize_stdout(&o).trim(),
                "7\ntrue",
                "a live coroutine in a mutable global must survive a full re-dofile (HOT_RELOAD guard)"
            ),
            ExecOutcome::Errored(_) => panic!("full-reload survival fixture failed to run"),
        }

        // Resuming the survivor: the v1 step's layout stamp (yield channel
        // widened by the Tick payload growing past the union width) rejects the
        // v0 frame -- the coroutine dies cleanly through the pcall path and pump
        // prints -1. Pre-stamp this CRASHED the consumer on the shifted frame
        // read, through the very tier the hot-reload contract once claimed was safe.
        let composed = run_parts(&[
            &full0.lua,
            stub_for(&full0.lua),
            "start()",
            "pump()",
            "_G.HOT_RELOAD = true",
            &full1.lua,
            "print(_G.GEN ~= nil)",
            "pump()",
        ]);
        match composed {
            ExecOutcome::Output(o) => assert_eq!(
                normalize_stdout(&o).trim(),
                "7\ntrue\n-1",
                "a shape-edit full reload must stamp-kill the surviving live coroutine, not corrupt it"
            ),
            ExecOutcome::Errored(_) => panic!("full-reload survivor resume crashed -- the layout stamp regressed"),
        }
    }

    /// PREMISE pin for the schema-drift restart policy
    /// (docs/hot-reload-schema-drift-plan.md): a struct SHAPE edit is an
    /// Interface-tier full rebuild, but the full reload is a re-`dofile` that
    /// preserves any mutable global whose declared-type STRING is unchanged --
    /// and `"Mob?"` names the shape without describing it. A live instance
    /// therefore SURVIVES the reload with its old field set while every reader
    /// recompiles to the new one: the recompiled reader nil-reads the added
    /// field and dies on the first arithmetic touch (or silently propagates
    /// nil through dynamic sinks). Non-inverted -- this documents the emit
    /// layer's real behavior; the restart policy prevents the scenario one
    /// layer up (the stone watcher), which this harness deliberately bypasses.
    #[test]
    fn struct_field_add_full_reload_nil_poisons_live_instances() {
        let v0 = "struct Mob { hp: int }\n\
                  global M: Mob? = null\n\
                  @nomangle\n\
                  fn spawn() { M = Mob { hp: 10 } }\n\
                  @nomangle\n\
                  fn describe() {\n  let m = M\n  if m != null { print(\"{m.hp}\") }\n}\n\
                  fn main() { }\n";
        let v1 = "struct Mob { hp: int, armor: int }\n\
                  global M: Mob? = null\n\
                  @nomangle\n\
                  fn spawn() { M = Mob { hp: 10, armor: 5 } }\n\
                  @nomangle\n\
                  fn describe() {\n  let m = M\n  if m != null { print(\"{m.hp + m.armor}\") }\n}\n\
                  fn main() { }\n";

        let mut session = bone_session(files(v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let full0 = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0").0;

        let result = session.update_file("main.rock", v1).expect("update v1");
        assert!(
            matches!(
                result.tier,
                bone_incremental::session::UpdateTier::Interface
                    | bone_incremental::session::UpdateTier::FullUser
            ),
            "a struct shape edit must be a full rebuild (the premise), got {:?}",
            result.tier
        );
        let full1 = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v1").0;

        // Survival premise: the live v0 instance rides through the full
        // re-dofile untouched -- `"Mob?"` didn't change, so the guard skips
        // the re-init and the old-shape table persists.
        let survival = run_parts(&[
            &full0.lua,
            stub_for(&full0.lua),
            "spawn()",
            "describe()",
            "_G.HOT_RELOAD = true",
            &full1.lua,
            "print(_G.M ~= nil and _G.M.armor == nil)",
        ]);
        match survival {
            ExecOutcome::Output(o) => assert_eq!(
                normalize_stdout(&o).trim(),
                "10\ntrue",
                "a live struct instance must survive the full re-dofile WITHOUT the added field"
            ),
            ExecOutcome::Errored(_) => panic!("struct-shape survival fixture failed to run"),
        }

        // Corruption: the v1 reader assumes `armor: int` on an instance that
        // never had it -- `m.hp + m.armor` is arithmetic on nil.
        let composed = run_parts(&[
            &full0.lua,
            stub_for(&full0.lua),
            "spawn()",
            "describe()",
            "_G.HOT_RELOAD = true",
            &full1.lua,
            "describe()",
        ]);
        // Assert the SPECIFIC failure: any-error-is-success would also pass on a
        // timeout or an unrelated load error, which is how a pin like this rots
        // into proving nothing.
        let diag = composed
            .expect_errored("expected the recompiled reader to die on the nil-valued added field");
        assert!(
            diag.contains("arithmetic on") && diag.contains("nil"),
            "expected nil-arithmetic on the field the live instance never had, got:\n{diag}"
        );
    }

    /// PREMISE pin for the schema-drift restart policy, value-struct class
    /// (docs/hot-reload-schema-drift-plan.md): LuaJIT `ffi.cdef` typedef
    /// redefinition is a SILENT NO-OP -- the v1 chunk's cdef "succeeds" but
    /// `ffi.typeof` keeps the ORIGINAL layout for the life of the process.
    /// Value-struct construction is POSITIONAL in declared-field order
    /// (`P_ct(a, b)`), so after a field REORDER the v1 ctor call feeds v1-order
    /// args into the frozen v0 ctype: every field silently gets the wrong
    /// value. Not even a fresh post-reload construction is safe -- this is the
    /// class where no in-process response exists, the core restart argument.
    #[test]
    fn value_struct_reorder_full_reload_is_caught_by_the_layout_check() {
        let v0 = "value P { x: int, y: int }\n\
                  global CUR: P = P { x: 0, y: 0 }\n\
                  @nomangle\n\
                  fn store_p() { CUR = P { x: 1, y: 2 } }\n\
                  @nomangle\n\
                  fn show() { print(\"{CUR.x},{CUR.y}\") }\n\
                  fn main() { }\n";
        let v1 = "value P { y: int, x: int }\n\
                  global CUR: P = P { x: 0, y: 0 }\n\
                  @nomangle\n\
                  fn store_p() { CUR = P { x: 1, y: 2 } }\n\
                  @nomangle\n\
                  fn show() { print(\"{CUR.x},{CUR.y}\") }\n\
                  fn main() { }\n";

        let mut session = bone_session(files(v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let full0 = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0").0;

        let result = session.update_file("main.rock", v1).expect("update v1");
        assert!(
            matches!(
                result.tier,
                bone_incremental::session::UpdateTier::Interface
                    | bone_incremental::session::UpdateTier::FullUser
            ),
            "a value-struct layout edit must be a full rebuild (the premise), got {:?}",
            result.tier
        );
        let full1 = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v1").0;

        // v0 alone: sanity -- set/show agree on the layout.
        let fresh = run_parts(&[&full0.lua, stub_for(&full0.lua), "store_p()", "show()"]);
        match fresh {
            ExecOutcome::Output(o) => assert_eq!(normalize_stdout(&o).trim(), "1,2"),
            ExecOutcome::Errored(_) => panic!("value-struct v0 fixture failed to run"),
        }

        // Composed: the v1 chunk re-cdefs P (silent no-op), so LuaJIT keeps the
        // v0 layout. Left alone this misassigns silently -- the v1 positional
        // ctor (y first) feeds the frozen v0 ctype (x first) and `show()` prints
        // the SWAP, `2,1`, with no error anywhere.
        //
        // It no longer gets that far. The emitted leaf-layout check
        // (`value_leaf_layout_checks`) compares skull's computed offsets against
        // `ffi.offsetof` at chunk load, sees v1's `y@0` against the frozen
        // `y@4`, and aborts. The restart policy is still the PRIMARY response --
        // `struct_shape_hash` hashes fields in order, so the real driver drifts
        // and respawns cold long before a chunk like this loads (this fixture
        // composes the two chunks by hand, bypassing that gate). The check is
        // the loud backstop for anything that ever slips past it, which is what
        // makes this class detectable rather than silent.
        let composed = run_parts(&[
            &full0.lua,
            stub_for(&full0.lua),
            "store_p()",
            "show()",
            "_G.HOT_RELOAD = true",
            &full1.lua,
            "store_p()",
            "show()",
        ]);
        // Assert the SPECIFIC diagnostic: any-error-is-success would also pass on
        // an unrelated load failure or a timeout, which is how a test like this
        // rots into proving nothing.
        let diag = composed.expect_errored(
            "a reordered value struct reloaded into a frozen ctype must be caught by \
             the leaf-layout check, not silently misassigned",
        );
        assert!(
            diag.contains("skull layout drift") && diag.contains("P.y"),
            "expected the leaf-layout drift diagnostic naming the moved field, got:\n{diag}"
        );
    }

    /// A flat enum's DEFINITION change (here: a payload widening that adds a frame
    /// slot) must classify as an Interface edit -> full `main.lua` rebuild (+ overlay
    /// wipe), NOT a BodyOnly overlay. This is what keeps the frame-slot channel safe
    /// across a shape change: a live coroutine can only survive a BodyOnly reload, and
    /// its (v0) resume box-rebuild + the consumer's `__y*` reads assume the v0 slot
    /// count. Were a shape change a mere overlay, the new step would write a different
    /// number of slots against stale readers. The Interface tier forces a full rebuild
    /// (fresh program, no live objects carried over), so that mismatch is impossible.
    #[test]
    fn flat_enum_shape_edit_forces_full_rebuild_not_overlay() {
        let v0 = "enum Ev { Tick(int), Act(() -> ()) }\n\
                  co fn gen() -> Coroutine<Ev> { yield Ev::Tick(1) }\n\
                  fn main() { let c = gen() }\n";
        // Widen Tick to two payloads: Ev's flat shape grows a positional slot.
        let v1 = "enum Ev { Tick(int, int), Act(() -> ()) }\n\
                  co fn gen() -> Coroutine<Ev> { yield Ev::Tick(1, 2) }\n\
                  fn main() { let c = gen() }\n";

        let mut session = bone_session(files(v0)).expect("session v0");
        let result = session.update_file("main.rock", v1).expect("update v1");
        assert!(
            matches!(
                result.tier,
                bone_incremental::session::UpdateTier::Interface
                    | bone_incremental::session::UpdateTier::FullUser
            ),
            "an enum shape change must force a full rebuild (not a BodyOnly overlay), got {:?}",
            result.tier
        );
    }

    /// `@on_reload` callbacks (e.g. the engine's `fire_live_coroutine_hot_reloads`,
    /// which drives every live coroutine's `on_hot_reload` hook) must fire on EVERY
    /// incremental reload, not just the first. `__rock_fire_hot_reload_callbacks()`
    /// snapshots-and-CLEARS `__rock_on_hot_reload`; on a FULL reload the
    /// `@on_reload` deco-init re-runs during `dofile(main.lua)` and repopulates it,
    /// but a body-edit OVERLAY only re-runs the CHANGED fns' chunks -- it never
    /// re-runs the (unchanged) `@on_reload` deco-init, so after the first fire the
    /// list is permanently empty. Symptom: editing a coroutine's `on_hot_reload {}`
    /// body prints once, then never again (mag `mob_attack_co`).
    ///
    /// Model: a `@on_reload` fn bumping an observable counter + an UNRELATED edited
    /// fn, two successive overlays each followed by a real
    /// `__rock_fire_hot_reload_callbacks()` (== two incremental reloads). The
    /// counter must reach 2.
    #[test]
    fn on_reload_callback_refires_across_successive_overlays() {
        let prog = |n: i32| {
            format!(
                "global FIRE_COUNT: int = 0\n\
                 @nomangle\n\
                 @on_reload\n\
                 fn bump_fire_count() {{ FIRE_COUNT = FIRE_COUNT + 1 }}\n\
                 fn unrelated() -> int {{ return {n} }}\n\
                 @nomangle\n\
                 fn report() {{ print(\"{{FIRE_COUNT}}\") }}\n"
            )
        };
        let (v0, v1, v2) = (prog(1), prog(2), prog(3));

        let mut session = bone_session(files(&v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");

        let r1 = session.update_file("main.rock", &v1).expect("update v1");
        let overlay1 = emit_overlay_checked(session.workspace(), &mut cache, &r1.reinferred);
        let r2 = session.update_file("main.rock", &v2).expect("update v2");
        let overlay2 = emit_overlay_checked(session.workspace(), &mut cache, &r2.reinferred);

        // full0 registers bump_fire_count via @on_reload. Each overlay is a body
        // edit of `unrelated` (bump_fire_count is NEVER in the changed set), then a
        // real fire. Both fires must run the callback -> FIRE_COUNT == 2.
        let composed = run_parts(&[
            &full0.lua,
            stub_for(&full0.lua),
            &overlay1.lua,
            "__rock_fire_hot_reload_callbacks()",
            &overlay2.lua,
            "__rock_fire_hot_reload_callbacks()",
            "report()",
        ]);
        match composed {
            ExecOutcome::Output(o) => assert_eq!(
                normalize_stdout(&o).trim(),
                "2",
                "the @on_reload callback must fire on BOTH incremental reloads"
            ),
            ExecOutcome::Errored(_) => panic!("on_reload overlay fixture failed to run"),
        }
    }

    /// Editing the BODY of an `@on_reload` fn, delivered as an overlay, must run
    /// the NEW body on the next fire through the retained CodeRef.
    #[test]
    fn on_reload_body_edit_runs_new_body_via_code_ref() {
        let prog = |v: i32| {
            format!(
                "global OUT: int = 0\n\
                 @on_reload\n\
                 fn refresh() {{ OUT = {v} }}\n\
                 @nomangle\n\
                 fn report() {{ print(\"{{OUT}}\") }}\n"
            )
        };
        let (v0, v1) = (prog(1), prog(2));

        let mut session = bone_session(files(&v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");

        let r1 = session.update_file("main.rock", &v1).expect("update v1");
        let overlay = emit_overlay_checked(session.workspace(), &mut cache, &r1.reinferred);

        // full0 registers the CodeRef. The overlay stages the edited body, and
        // the existing callback entry must invoke it.
        let composed = run_parts(&[
            &full0.lua,
            stub_for(&full0.lua),
            &overlay.lua,
            "__rock_fire_hot_reload_callbacks()",
            "report()",
        ]);
        match composed {
            ExecOutcome::Output(o) => assert_eq!(
                normalize_stdout(&o).trim(),
                "2",
                "fire must run the EDITED @on_reload body through its CodeRef"
            ),
            ExecOutcome::Errored(_) => panic!("on_reload body-edit fixture failed to run"),
        }
    }

    /// `@on_reload` stores a function value, so ordinary name mangling does not
    /// affect callback freshness.
    #[test]
    fn on_reload_without_nomangle_emits_ok() {
        let prog = "@on_reload\nfn refresh() {}\nfn main() {}\n";
        let session = bone_session(files(prog)).expect("session");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        assert!(
            skull::mir_emit::emit_full(session.workspace(), mid).is_ok(),
            "@on_reload without @nomangle must emit cleanly"
        );
    }

    /// Whole-corpus sweep, gated (heavy: thousands of luajit spawns). Run with
    /// `SKULL_OVERLAY_CORPUS=1 cargo nextest run -p rock-cli overlay_composition_full_corpus`.
    #[test]
    fn overlay_composition_full_corpus() {
        if std::env::var("SKULL_OVERLAY_CORPUS").is_err() {
            return; // opt-in; the curated subset is the always-on gate
        }
        let root = std::env::var("SKULL_OVERLAY_ROOT").unwrap_or_else(|_| ws_path("tests"));
        let files = super::super::collect_rock_files(&root).expect("collect");
        let files: Vec<String> = files.into_iter().filter(|f| !f.contains("/benches/")).collect();
        let t = sweep(&files);
        eprintln!(
            "overlay corpus sweep: {} files, {} with tests, {} tests compared, {} divergences",
            files.len(), t.files_with_tests, t.compared, t.divergences.len()
        );
        assert!(
            t.divergences.is_empty(),
            "overlay composition diverged on {} test(s):\n{}",
            t.divergences.len(),
            t.divergences.iter().map(|(f, ts, d)| format!("  {f}::{ts}: {d}")).collect::<Vec<_>>().join("\n")
        );
    }

    // -- frame-lambda hoist (docs/coroutine-frame-lambda-plan.md P2) ----------
    // These pin the reload/layout contract of P1's co-fn lambda hoist: a
    // capturing co-fn lambda is an env record whose `__fn` names a hoisted
    // top-level body emitted IN THE SAME CHUNK as the step (const-hoist
    // precedent), so a BodyOnly overlay carries it. A test that would also pass
    // against pre-P1 emission (inline `function(...)` closures, no
    // `gen_ret_Coroutine_Ev_unit_unit__step__lambda<k>` global) is not evidence -- each asserts the
    // hoisted body is present in the overlay.

    /// The frame layout stamp for a coroutine ctor `function <ctor>()`: the
    /// `__layout = <N>` literal the ctor writes. Two builds whose only diff is a
    /// lambda-local must produce the SAME stamp (P1: lambda locals left the
    /// layout canon); a co-fn-level local shifts it.
    fn layout_stamp(lua: &str, ctor: &str) -> i64 {
        // The emitted ctor is `function <ctor>_ret_<coroutine-type>()`; recover the
        // one WITHOUT a `__` synthetic suffix (the ctor itself, not its
        // `__step`/`__advance`/`__resume`).
        let prefix = format!("function {ctor}_ret_");
        let mut scan = 0usize;
        let start = loop {
            let at = lua[scan..]
                .find(&prefix)
                .map(|i| scan + i)
                .unwrap_or_else(|| panic!("no ctor `{ctor}` in emitted lua"));
            let rest = &lua[at + "function ".len()..];
            let paren = rest.find('(').expect("ctor header has `(`");
            if !rest[..paren].contains("__") {
                break at;
            }
            scan = at + prefix.len();
        };
        let tail = &lua[start..];
        let key = "__layout = ";
        let lpos = tail.find(key).expect("ctor has no __layout stamp");
        let after = &tail[lpos + key.len()..];
        let end = after.find(|c: char| !c.is_ascii_digit() && c != '-').unwrap_or(after.len());
        after[..end].parse().expect("parse __layout stamp")
    }

    /// A capturing co-fn lambda whose BODY is edited (operator `+` -> `*`): the
    /// overlay must carry the hoisted `gen_ret_Coroutine_Ev_unit_unit__step__lambda0` body, and a
    /// coroutine created AFTER the reload runs the new lambda behavior. Equals a
    /// fresh full build of v1. The `overlay.lua.contains("gen_ret_Coroutine_Ev_unit_unit__step__lambda0")`
    /// assertion is what discriminates P1 from pre-P1 (which had no hoisted
    /// global -- the lambda was an inline closure inside the step).
    #[test]
    fn cofn_capturing_lambda_body_edit_overlay_created_after_reload() {
        let prog = |op: char| {
            format!(
                "enum Ev {{ Tick(int), Act(() -> ()) }}\n\
                 global GEN: Coroutine<Ev>? = null\n\
                 co fn gen() -> Coroutine<Ev> {{\n  let base = 3\n  let f = (x: int) => x {op} base\n  yield Ev::Tick(f(4))\n}}\n\
                 fn score(e: Ev) -> int {{\n  return match e {{\n    Ev::Tick(x) => x\n    Ev::Act(_) => -1\n  }}\n}}\n\
                 @nomangle\n\
                 fn start() {{ GEN = gen() }}\n\
                 @nomangle\n\
                 fn pump() {{\n  let c = GEN\n  if c != null {{\n    let e = c.resume()\n    if e != null {{ print(\"{{score(e)}}\") }} else {{ print(\"-1\") }}\n  }}\n}}\n"
            )
        };
        let (v0, v1) = (prog('+'), prog('*'));

        let mut session = bone_session(files(&v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");

        let result = session.update_file("main.rock", &v1).expect("update v1");
        assert!(
            matches!(result.tier, bone_incremental::session::UpdateTier::BodyOnly),
            "editing a lambda operator is a body edit, got {:?}",
            result.tier
        );
        let overlay = emit_overlay_checked(session.workspace(), &mut cache, &result.reinferred);
        assert!(
            overlay.lua.contains("gen_ret_Coroutine_Ev_unit_unit__step__lambda0"),
            "the overlay must carry the hoisted lambda body (const-hoist precedent); \
             absence means pre-P1 inline-closure emission"
        );
        // Created AFTER the overlay -> the v1 lambda (4*3=12), then done -> -1.
        // "12\n-1" IS a fresh v1 full build's output, so this doubles as the
        // fresh-equivalence check (the program has no `main`, so the generic
        // `assert_runtime_eq` driver doesn't apply -- start/pump is the driver).
        let composed = run_parts(&[&full0.lua, stub_for(&full0.lua), &overlay.lua, "start()", "pump()", "pump()"]);
        match composed {
            ExecOutcome::Output(o) => assert_eq!(
                normalize_stdout(&o).trim(),
                "12\n-1",
                "a coroutine created after the reload runs the new hoisted lambda body (4*3=12)"
            ),
            ExecOutcome::Errored(_) => panic!("post-reload coroutine with hoisted lambda failed to run"),
        }
    }

    /// A LIVE coroutine created BEFORE the reload survives a layout-stable body
    /// edit, and its stored capturing record resolves the edited body through
    /// the same logical CodeRef as a record created afterwards.
    #[test]
    fn cofn_lambda_record_snapshot_refreshes_across_reload() {
        let prog = |op: char| {
            format!(
                "enum Ev {{ Tick(int), Act(() -> ()) }}\n\
                 global GEN: Coroutine<Ev>? = null\n\
                 co fn gen() -> Coroutine<Ev> {{\n  let base = 5\n  let f = (x: int) => x {op} base\n  yield Ev::Tick(f(3))\n  yield Ev::Tick(f(3))\n  let g = (x: int) => x {op} base\n  yield Ev::Tick(g(3))\n}}\n\
                 fn score(e: Ev) -> int {{\n  return match e {{\n    Ev::Tick(x) => x\n    Ev::Act(_) => -1\n  }}\n}}\n\
                 @nomangle\n\
                 fn start() {{ GEN = gen() }}\n\
                 @nomangle\n\
                 fn pump() {{\n  let c = GEN\n  if c != null {{\n    let e = c.resume()\n    if e != null {{ print(\"{{score(e)}}\") }} else {{ print(\"-1\") }}\n  }}\n}}\n"
            )
        };
        let (v0, v1) = (prog('+'), prog('*'));

        let mut session = bone_session(files(&v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");
        // Layout-stable premise: an operator edit doesn't touch the frame canon.
        assert_eq!(
            layout_stamp(&full0.lua, "gen"),
            {
                let mut s = bone_session(files(&v1)).expect("session v1 probe");
                let m = skull::user_module_id(s.workspace()).unwrap();
                let f = skull::mir_emit::emit_full(s.workspace(), m).expect("emit v1 probe").0;
                let _ = &mut s;
                layout_stamp(&f.lua, "gen")
            },
            "an operator-only edit must be layout-stable (no frame-cell change)"
        );

        let result = session.update_file("main.rock", &v1).expect("update v1");
        let overlay = emit_overlay_checked(session.workspace(), &mut cache, &result.reinferred);
        assert!(overlay.lua.contains("gen_ret_Coroutine_Ev_unit_unit__step__lambda1"), "overlay must carry the hoisted g body");

        // start() + pump() under v0: f(3)=8. Overlay (op *). The same stored
        // record now resolves lambda0's edited body, then g uses lambda1.
        let composed = run_parts(&[&full0.lua, stub_for(&full0.lua), "start()", "pump()", &overlay.lua, "pump()", "pump()", "pump()"]);
        let fresh_session = bone_session(files(&v1)).expect("fresh session v1");
        let fresh_mid = skull::user_module_id(fresh_session.workspace()).unwrap();
        let fresh = skull::mir_emit::emit_full(fresh_session.workspace(), fresh_mid)
            .expect("emit fresh v1")
            .0;
        let fresh_run = run_parts(&[
            &fresh.lua,
            stub_for(&fresh.lua),
            "start()",
            "pump()",
            "pump()",
            "pump()",
            "pump()",
        ]);
        match (composed, fresh_run) {
            (ExecOutcome::Output(composed), ExecOutcome::Output(fresh)) => {
                let composed = normalize_stdout(&composed);
                let fresh = normalize_stdout(&fresh);
                assert_eq!(fresh.trim(), "15\n15\n15\n-1", "fresh v1 build output");
                let mut post_reload = composed.lines();
                assert_eq!(post_reload.next(), Some("8"), "the pre-reload record ran v0");
                let mut fresh_after_first = fresh.lines();
                assert_eq!(fresh_after_first.next(), Some("15"), "fresh v1 first yield");
                assert_eq!(
                    post_reload.collect::<Vec<_>>().join("\n"),
                    fresh_after_first.collect::<Vec<_>>().join("\n"),
                    "the retained records after reload must match a fresh v1 build"
                );
            }
            (ExecOutcome::Output(_), ExecOutcome::Errored(_)) => {
                panic!("fresh v1 CodeRef build failed to run")
            }
            (ExecOutcome::Errored(_), ExecOutcome::Output(_)) => {
                panic!("live coroutine CodeRef record failed to run")
            }
            (ExecOutcome::Errored(_), ExecOutcome::Errored(_)) => {
                panic!("both live and fresh CodeRef records failed to run")
            }
        }
    }

    #[test]
    fn named_and_capture_free_values_refresh_through_reusable_slots() {
        let program = |named_delta: i32, free_value: i32| {
            format!(
                "global NAMED: (fn(int) -> int)? = null\n\
                 global FREE: (fn() -> int)? = null\n\
                 @nomangle fn named(x: int) -> int {{ return x + {named_delta} }}\n\
                 @nomangle fn make_free() -> fn() -> int {{ return () => {free_value} }}\n\
                 @nomangle fn start() {{\n\
                   NAMED = named\n\
                   FREE = make_free()\n\
                 }}\n\
                 @nomangle fn invoke() {{\n\
                   let a = NAMED\n\
                   let b = FREE\n\
                   if a != null {{ if b != null {{ print(\"{{a(3)}} {{b()}}\") }} }}\n\
                 }}\n"
            )
        };
        let v0 = program(1, 10);
        let v1 = program(2, 20);
        let mut session = bone_session(files(&v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");
        let baseline = run_parts(&[&full0.lua, "start()", "invoke()"]);
        match baseline {
            ExecOutcome::Output(output) => assert_eq!(normalize_stdout(&output).trim(), "4 10"),
            ExecOutcome::Errored(error) => panic!("baseline named/capture-free values failed: {error}"),
        }

        let update = session.update_file("main.rock", &v1).expect("update v1");
        let overlay = emit_overlay_checked(session.workspace(), &mut cache, &update.reinferred);
        assert!(overlay.lua.contains("__rock_code_slot_stage"));
        assert!(overlay.lua.contains("__rock_code_slot_publish"));
        let composed = run_parts(&[&full0.lua, "start()", &overlay.lua, "invoke()"]);
        let fresh = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v1").0;
        let fresh_run = run_parts(&[&fresh.lua, "start()", "invoke()"]);
        match (composed, fresh_run) {
            (ExecOutcome::Output(composed), ExecOutcome::Output(fresh)) => {
                assert_eq!(normalize_stdout(&composed).trim(), "5 20");
                assert_eq!(normalize_stdout(&composed), normalize_stdout(&fresh));
            }
            (ExecOutcome::Errored(composed), ExecOutcome::Errored(fresh)) => {
                panic!("slot values both failed: composed={composed}; fresh={fresh}")
            }
            (ExecOutcome::Errored(composed), _) => panic!("composed slot values failed: {composed}"),
            (_, ExecOutcome::Errored(fresh)) => panic!("fresh slot values failed: {fresh}"),
        }
    }

    /// A lambda-local edit leaves the coroutine frame stamp stable, but changes
    /// the compatibility record for its escaping lambda body. It must request a
    /// safe restart rather than redirect a retained CodeRef.
    #[test]
    fn cofn_lambda_local_edit_requests_safe_restart() {
        // v0 body: `x + base`; v1 body: `{ let y = base + base; x + y }` (used
        // local -- reason (A): raw-id shift, not the dead-cell fallback (B)).
        let prog = |body: &str| {
            format!(
                "enum Ev {{ Tick(int), Act(() -> ()) }}\n\
                 global GEN: Coroutine<Ev>? = null\n\
                 co fn gen() -> Coroutine<Ev> {{\n  let base = 4\n  let f = (x: int) => {body}\n  yield Ev::Tick(f(3))\n  let g = (x: int) => {body}\n  yield Ev::Tick(g(3))\n}}\n\
                 fn score(e: Ev) -> int {{\n  return match e {{\n    Ev::Tick(x) => x\n    Ev::Act(_) => -1\n  }}\n}}\n\
                 @nomangle\n\
                 fn start() {{ GEN = gen() }}\n\
                 @nomangle\n\
                 fn pump() {{\n  let c = GEN\n  if c != null {{\n    let e = c.resume()\n    if e != null {{ print(\"{{score(e)}}\") }} else {{ print(\"-1\") }}\n  }}\n}}\n"
            )
        };
        let v0 = prog("x + base");
        let v1 = prog("{ let y = base + base; x + y }");

        let mut session = bone_session(files(&v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");

        let result = session.update_file("main.rock", &v1).expect("update v1");
        assert!(
            matches!(result.tier, bone_incremental::session::UpdateTier::BodyOnly),
            "adding a lambda-local is a body edit, got {:?}",
            result.tier
        );
        let fresh = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v1").0;

        assert_eq!(
            layout_stamp(&full0.lua, "gen"),
            layout_stamp(&fresh.lua, "gen"),
            "the lambda-local edit must not change the coroutine frame stamp"
        );
        let err = match skull::mir_emit::emit_overlay(session.workspace(), &mut cache, &result.reinferred) {
            Ok(_) => panic!("a changed escaping lambda body must not overlay across its compatibility record"),
            Err(err) => err,
        };
        assert!(
            err.message.contains("safe restart"),
            "incompatible lambda body must request the safe restart path, got: {}",
            err.message
        );
    }

    /// A co-fn-level local changes the coroutine-frame schema, so the overlay
    /// must request a safe restart before publishing any CodeRef body.
    #[test]
    fn cofn_toplevel_local_edit_requests_safe_restart() {
        // `mk()` keeps the inserted co-fn local as a real call so DCE can't
        // erase its frame cell (same guard as the cell-shift stamp test).
        let prog = |extra: &str| {
            format!(
                "enum Ev {{ Tick(int), Act(() -> ()) }}\n\
                 global GEN: Coroutine<Ev>? = null\n\
                 fn mk(n: int) -> int {{ return n }}\n\
                 co fn gen() -> Coroutine<Ev> {{\n  let base = mk(4)\n{extra}  let f = (x: int) => x + base\n  yield Ev::Tick(f(3))\n  yield Ev::Tick(f(3))\n}}\n\
                 fn score(e: Ev) -> int {{\n  return match e {{\n    Ev::Tick(x) => x\n    Ev::Act(_) => -1\n  }}\n}}\n\
                 @nomangle\n\
                 fn start() {{ GEN = gen() }}\n\
                 @nomangle\n\
                 fn pump() {{\n  let c = GEN\n  if c != null {{\n    let e = c.resume()\n    if e != null {{ print(\"{{score(e)}}\") }} else {{ print(\"-1\") }}\n  }}\n}}\n"
            )
        };
        let (v0, v1) = (prog(""), prog("  let z = mk(9)\n  yield Ev::Tick(z)\n"));

        let mut session = bone_session(files(&v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");

        let result = session.update_file("main.rock", &v1).expect("update v1");
        assert!(
            matches!(result.tier, bone_incremental::session::UpdateTier::BodyOnly),
            "adding a co-fn local is a body edit, got {:?}",
            result.tier
        );
        let fresh = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v1").0;

        assert_ne!(
            layout_stamp(&full0.lua, "gen"),
            layout_stamp(&fresh.lua, "gen"),
            "a co-fn-level local must shift the frame layout stamp"
        );

        let err = match skull::mir_emit::emit_overlay(session.workspace(), &mut cache, &result.reinferred) {
            Ok(_) => panic!("a changed coroutine frame schema must not overlay"),
            Err(err) => err,
        };
        assert!(
            err.message.contains("safe restart"),
            "incompatible coroutine frame must request the safe restart path, got: {}",
            err.message
        );
    }
}
#[cfg(test)]
mod shape_drift_tests {
    //! The schema-drift restart trigger (docs/hot-reload-schema-drift-plan.md):
    //! a type-SHAPE edit returns `ShapeChanged` (restart request) and DEFERS the
    //! `main.lua` write -- the running process must never hot-reload a program
    //! whose live data layout it doesn't have. Body and signature-only edits
    //! stay on the hot paths. The premise repros for the corruption itself are
    //! `struct_field_add_full_reload_nil_poisons_live_instances` and
    //! `value_struct_reorder_full_reload_misassigns_positional_ctor`.

    use super::super::{SkullReload, SkullStone};
    use std::path::PathBuf;

    fn boot(v0: &str) -> (tempfile::TempDir, SkullStone, PathBuf, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let dir_str = dir.path().to_str().unwrap().to_string();
        let main_lua = dir.path().join("main.lua");
        let inc_lua = dir.path().join("incremental.lua");
        let rock = dir.path().join("main.rock");
        std::fs::write(&rock, v0).unwrap();
        let stone = SkullStone::init(&dir_str, &main_lua, &inc_lua).expect("init");
        (dir, stone, main_lua, inc_lua, rock)
    }

    #[test]
    fn struct_shape_edit_requests_restart_and_defers_the_write() {
        let v0 = "struct Mob { hp: int }\n\
                  global M: Mob? = null\n\
                  @nomangle\n\
                  fn spawn() { M = Mob { hp: 10 } }\n\
                  fn main() { }\n";
        let v1 = "struct Mob { hp: int, armor: int }\n\
                  global M: Mob? = null\n\
                  @nomangle\n\
                  fn spawn() { M = Mob { hp: 10, armor: 5 } }\n\
                  fn main() { }\n";
        let (_dir, mut stone, main_lua, inc_lua, rock) = boot(v0);
        let baseline = std::fs::read_to_string(&main_lua).unwrap();

        std::fs::write(&rock, v1).unwrap();
        let reload = stone.recompile(&[rock.clone()], &main_lua, &inc_lua).expect("recompile v1");
        match &reload {
            SkullReload::ShapeChanged { types } => assert!(
                types.iter().any(|t| t == "Mob"),
                "the restart request must name the drifted type, got {types:?}"
            ),
            other => panic!("a struct shape edit must request a restart (ShapeChanged), got {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(&main_lua).unwrap(),
            baseline,
            "the new-shape main.lua write must be DEFERRED -- the old process must never dofile it"
        );

        // The restart path canonicalizes: force_full writes the new program.
        stone.force_full(&main_lua, &inc_lua).expect("force_full");
        assert_ne!(
            std::fs::read_to_string(&main_lua).unwrap(),
            baseline,
            "canonicalize after the kill must write the deferred new-shape program"
        );

        // Re-baselined: recompiling the same source again is a no-op, not a
        // second restart request.
        let again = stone.recompile(&[rock], &main_lua, &inc_lua).expect("recompile same");
        assert!(
            matches!(again, SkullReload::NoChange),
            "the shape delta must be consumed by the restart, got {again:?}"
        );
    }

    /// Marking a `value` type `nobox` moves every global of that type from one
    /// boxed slot to K leaf globals, while the rendered type stays `"NV2"`. The
    /// emitted per-global preservation guard compares only that rendered type, so
    /// it would preserve across the move and leave the new leaves nil -- this
    /// restart is what stops the reload reaching it. `struct_shape_hash` hashing
    /// `is_nobox` is the one line that makes it fire.
    #[test]
    fn marking_a_type_nobox_requests_restart() {
        let v0 = "value NV2 { x: float, y: float }\n\
                  global SPOT: NV2 = NV2 { x: 1.0, y: 2.0 }\nfn main() { }\n";
        let v1 = "nobox value NV2 { x: float, y: float }\n\
                  global SPOT: NV2 = NV2 { x: 1.0, y: 2.0 }\nfn main() { }\n";
        let (_dir, mut stone, main_lua, inc_lua, rock) = boot(v0);
        std::fs::write(&rock, v1).unwrap();
        let reload = stone.recompile(&[rock], &main_lua, &inc_lua).expect("recompile v1");
        assert!(
            matches!(&reload, SkullReload::ShapeChanged { types } if types.iter().any(|t| t == "NV2")),
            "a nobox flip relocates live global storage -- must restart, got {reload:?}"
        );
    }

    #[test]
    fn value_struct_layout_edit_requests_restart() {
        let v0 = "value P { x: int, y: int }\nfn probe(p: P) -> int { p.x }\nfn main() { }\n";
        let v1 = "value P { y: int, x: int }\nfn probe(p: P) -> int { p.x }\nfn main() { }\n";
        let (_dir, mut stone, main_lua, inc_lua, rock) = boot(v0);
        std::fs::write(&rock, v1).unwrap();
        let reload = stone.recompile(&[rock], &main_lua, &inc_lua).expect("recompile v1");
        assert!(
            matches!(&reload, SkullReload::ShapeChanged { types } if types.iter().any(|t| t == "P")),
            "a value-struct field reorder freezes against the process ctype -- must restart, got {reload:?}"
        );
    }

    #[test]
    fn mutable_global_membership_edit_requests_restart_and_keeps_old_session_schema() {
        let v0 = "global A: int = 1\nfn main() { }\n";
        let v1 = "global A: int = 1\nglobal B: int = 2\nfn main() { }\n";
        let (_dir, mut stone, main_lua, inc_lua, rock) = boot(v0);
        let baseline = std::fs::read_to_string(&main_lua).expect("baseline");
        let old_schema = stone
            .cache
            .state_schema()
            .expect("baseline StateSchema")
            .schema_id
            .clone();

        std::fs::write(&rock, v1).expect("edited source");
        let reload = stone
            .recompile(std::slice::from_ref(&rock), &main_lua, &inc_lua)
            .expect("membership edit");
        match reload {
            SkullReload::FullRequired { reasons } => assert!(
                reasons.iter().any(|reason| reason.contains("StateSchema")),
                "membership drift must identify StateSchema restart: {reasons:?}"
            ),
            other => panic!("adding a mutable global must require restart, got {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(&main_lua).expect("main after refusal"),
            baseline,
            "an incompatible schema must not replace the old process artifact"
        );
        assert_eq!(
            stone
                .cache
                .state_schema()
                .expect("retained StateSchema")
                .schema_id
                .as_str(),
            old_schema.as_str(),
            "a refused publication must retain the old compiler schema"
        );

        stone
            .force_full(&main_lua, &inc_lua)
            .expect("restart canonicalization");
        let current = std::fs::read_to_string(&main_lua).expect("new main");
        assert_ne!(current, baseline, "force_full must materialize the new schema");
        assert!(
            current.contains("identity=\"global:") && current.contains("::B\""),
            "the new logical global identity must be present in the published schema: {current}"
        );
        assert_ne!(
            stone
                .cache
                .state_schema()
                .expect("new StateSchema")
                .schema_id
                .as_str(),
            old_schema.as_str(),
            "restart canonicalization must publish the changed schema"
        );
    }

    #[test]
    fn body_and_signature_edits_stay_on_the_hot_paths() {
        let v0 = "struct Mob { hp: int }\n\
                  fn heal(m: Mob) -> int { m.hp }\n\
                  fn main() { }\n";
        let body = "struct Mob { hp: int }\n\
                    fn heal(m: Mob) -> int { m.hp + 1 }\n\
                    fn main() { }\n";
        let sig = "struct Mob { hp: int }\n\
                   fn heal(m: Mob, bonus: int) -> int { m.hp + bonus }\n\
                   fn main() { }\n";
        let (_dir, mut stone, main_lua, inc_lua, rock) = boot(v0);

        std::fs::write(&rock, body).unwrap();
        let reload = stone.recompile(&[rock.clone()], &main_lua, &inc_lua).expect("body edit");
        assert!(
            matches!(reload, SkullReload::Incremental { .. }),
            "a body edit must stay a hot overlay, got {reload:?}"
        );

        std::fs::write(&rock, sig).unwrap();
        let reload = stone.recompile(&[rock], &main_lua, &inc_lua).expect("sig edit");
        assert!(
            matches!(reload, SkullReload::Full { .. }),
            "a signature-only edit carries no data-shape drift -- full reload, NOT a restart, got {reload:?}"
        );
    }

    #[test]
    fn shader_resource_interface_edit_requests_full_reload() {
        let v0 = "@shader(fragment)\n\
                  fn frag(tc: Vec2) -> Vec4 {\n\
                    return Vec4 { x: tc.x, y: 0.0, z: 1.0, w: 1.0 }\n\
                  }\n\
                  global KEY: string = rock_shader(frag)\n\
                  fn main() { }\n";
        let v1 = "@shader(fragment)\n\
                  fn frag(tex: Sampler2D, tc: Vec2) -> Vec4 {\n\
                    return texture(tex, tc)\n\
                  }\n\
                  global KEY: string = rock_shader(frag)\n\
                  fn main() { }\n";
        let (_dir, mut stone, main_lua, inc_lua, rock) = boot(v0);
        let baseline = std::fs::read_to_string(&main_lua).unwrap();
        assert!(baseline.contains("__rock_shader_register(\"frag\""));

        std::fs::write(&rock, v1).unwrap();
        let reload = stone.recompile(&[rock], &main_lua, &inc_lua).expect("shader recompile");
        assert!(
            matches!(reload, SkullReload::Full { .. }),
            "a shader resource/interface change must use the full path, got {reload:?}"
        );
        let current = std::fs::read_to_string(&main_lua).unwrap();
        assert!(
            current.contains("texture("),
            "the full shader reassembly must carry the new resource-consuming source"
        );
        assert_ne!(current, baseline, "the resource edit must change the registered artifact");
    }
}

#[cfg(test)]
mod shader_registration_removal_tests {
    use super::super::{SkullReload, SkullStone};
    use std::collections::BTreeSet;
    use std::path::PathBuf;
    use std::sync::Arc;

    const V0: &str = r#"
@shader(fragment)
fn frag_a(tc: Vec2) -> Vec4 {
    return Vec4 { x: tc.x, y: 0.0, z: 0.0, w: 1.0 }
}

@shader(fragment)
fn frag_b(tc: Vec2) -> Vec4 {
    return Vec4 { x: 0.0, y: tc.x, z: 0.0, w: 1.0 }
}

fn choose_a() -> string { return rock_shader(frag_a) }
fn choose_b() -> string { return rock_shader(frag_b) }
fn main() { print(choose_a()); print(choose_b()) }
"#;

    fn v1() -> String {
        V0.replace("return rock_shader(frag_b)", "return \"removed\"")
    }

    fn key(
        ws: &bone_semantic::inference::Workspace,
        program: &skull::shader_artifact::ShaderProgram,
    ) -> String {
        let name = |fref: skull_mir::mir::FuncRef| {
            ws.module(fref.module).functions[fref.func].name.to_string()
        };
        match program.vert {
            Some(vert) => format!("{}+{}", name(vert), name(program.frag)),
            None => name(program.frag),
        }
    }

    fn logical_keys(
        ws: &bone_semantic::inference::Workspace,
        graph: &skull::instance_graph::InstanceGraph,
    ) -> BTreeSet<String> {
        let collection =
            skull_mir::lower::collect_shader_collection(ws).expect("shader collection");
        let programs: Vec<_> = collection
            .pairs
            .iter()
            .map(|pair| skull::shader_artifact::ShaderProgram {
                vert: pair.vert,
                frag: pair.frag,
            })
            .collect();
        let scx = skull_mir::lower::ScalarCtx::new_values_first_class(ws)
            .sharing(Arc::clone(graph.interner()));
        let view = skull::shader_artifact::realize_shader_with_context(
            ws,
            graph,
            &skull::shader_artifact::ShaderArtifactRequest {
                spec: skull::target_projection::TargetSpec {
                    shader: Some(skull::target_projection::ShaderTargetSpec::Glsl(
                        skull_codegen_glsl::GlslProfile::Core330,
                    )),
                    ..skull::target_projection::LUAJIT_DESKTOP
                },
                programs: &programs,
            },
            &scx,
        )
        .expect("shader artifact view");
        assert!(
            view.program_views
                .iter()
                .all(|program| !program.stages.is_empty())
        );
        view.program_views
            .iter()
            .map(|program| key(ws, &program.program))
            .collect()
    }

    fn registration_keys(text: &str) -> BTreeSet<String> {
        let prefix = "__rock_shader_register(\"";
        text.lines()
            .filter_map(|line| {
                let start = line.find(prefix).map(|index| index + prefix.len())?;
                let end = line[start..].find('"')?;
                Some(line[start..start + end].to_string())
            })
            .collect()
    }

    fn boot(source: &str) -> (tempfile::TempDir, SkullStone, PathBuf, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_str().expect("tempdir path").to_string();
        let main = dir.path().join("main.lua");
        let incremental = dir.path().join("incremental.lua");
        let rock = dir.path().join("main.rock");
        std::fs::write(&rock, source).expect("source");
        let stone = SkullStone::init(&root, &main, &incremental).expect("initial compile");
        (dir, stone, main, incremental, rock)
    }

    #[test]
    fn native_reload_removing_one_shader_callsite_removes_one_runtime_key() {
        let (_dir, mut stone, main, incremental, rock) = boot(V0);
        let wanted = BTreeSet::from(["frag_a".to_string(), "frag_b".to_string()]);
        let baseline = std::fs::read_to_string(&main).unwrap();
        assert_eq!(registration_keys(&baseline), wanted);
        assert_eq!(logical_keys(stone.project.workspace(), stone.cache.graph()), wanted);

        std::fs::write(&rock, v1()).expect("edited source");
        let reload = stone
            .recompile(std::slice::from_ref(&rock), &main, &incremental)
            .expect("reload");
        assert!(
            matches!(reload, SkullReload::FullRequired { .. }),
            "a shader key removal must require a process restart: {reload:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&main).unwrap(),
            baseline,
            "FullRequired must not write a program for the old live process"
        );

        stone
            .force_full(&main, &incremental)
            .expect("canonicalize shader removal");

        let current = BTreeSet::from(["frag_a".to_string()]);
        assert_eq!(logical_keys(stone.project.workspace(), stone.cache.graph()), current);
        let runtime = registration_keys(&std::fs::read_to_string(&main).unwrap());
        assert_eq!(
            runtime, current,
            "native runtime registration population retained a removed key"
        );
    }
}

#[cfg(test)]
mod compute_registration_contract_tests {
    use super::super::{SkullReload, SkullStone};
    use std::path::PathBuf;

    const V0: &str = r#"
value Params { n: int }

@shader(compute)
@workgroup(64, 1, 1)
@dispatch(1)
fn copy_values(
    @storage(read) input: StorageBuffer<int>,
    @storage(read_write) output: StorageBuffer<int>,
    @uniform params: Params,
) {
    let i = global_invocation_id().x
    if i >= params.n { return }
    storage_store(output, i, storage_load(input, i))
}

global COPY_VALUES: string = rock_compute(copy_values)
fn main() { }
"#;

    fn boot(source: &str) -> (tempfile::TempDir, SkullStone, PathBuf, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_str().expect("tempdir path").to_string();
        let main = dir.path().join("main.lua");
        let incremental = dir.path().join("incremental.lua");
        let rock = dir.path().join("main.rock");
        std::fs::write(&rock, source).expect("source");
        let stone = SkullStone::init(&root, &main, &incremental).expect("initial compile");
        (dir, stone, main, incremental, rock)
    }

    #[test]
    fn compute_abi_edit_requires_restart_and_does_not_write_old_process_files() {
        let (_dir, mut stone, main, incremental, rock) = boot(V0);
        let baseline = std::fs::read_to_string(&main).expect("baseline");
        let v1 = V0.replace("@workgroup(64, 1, 1)", "@workgroup(32, 1, 1)");
        std::fs::write(&rock, v1).expect("edited source");

        let reload = stone
            .recompile(std::slice::from_ref(&rock), &main, &incremental)
            .expect("compute ABI recompile");
        match reload {
            SkullReload::FullRequired { reasons } => assert!(
                reasons.iter().any(|reason| reason == "shader registration ABI changed"),
                "the restart reason must identify the ABI contract: {reasons:?}"
            ),
            other => panic!("a compute workgroup ABI edit must require restart, got {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(&main).expect("main after rejected reload"),
            baseline,
            "a FullRequired compute edit must not write the program for the old process"
        );

        stone.force_full(&main, &incremental).expect("canonicalize ABI edit");
        let current = std::fs::read_to_string(&main).expect("canonicalized main");
        assert_ne!(current, baseline, "force_full must materialize the new ABI after restart");
        assert!(current.contains("@workgroup_size(32, 1, 1)"), "new compute ABI was not materialized");
    }

    #[test]
    fn compute_root_addition_and_removal_require_restart() {
        let (_dir_removed, mut removed_stone, removed_main, removed_incremental, removed_rock) = boot(V0);
        let removed_baseline = std::fs::read_to_string(&removed_main).expect("removed baseline");
        std::fs::write(
            &removed_rock,
            V0.replace("global COPY_VALUES: string = rock_compute(copy_values)", ""),
        )
        .expect("removed compute root");
        let removed = removed_stone
            .recompile(std::slice::from_ref(&removed_rock), &removed_main, &removed_incremental)
            .expect("removed compute root reload");
        match removed {
            SkullReload::FullRequired { reasons } => assert!(
                reasons.iter().any(|reason| reason == "shader registration key set changed"),
                "root removal must identify the registration key set: {reasons:?}"
            ),
            other => panic!("removing a compute root must require restart, got {other:?}"),
        }
        assert_eq!(std::fs::read_to_string(&removed_main).unwrap(), removed_baseline);

        let (_dir_added, mut added_stone, added_main, added_incremental, added_rock) = boot(V0);
        let added_baseline = std::fs::read_to_string(&added_main).expect("added baseline");
        let addition = format!(
            "{V0}\n\n@shader(compute)\n@workgroup(64, 1, 1)\n@dispatch(1)\nfn copy_values_extra(\n    @storage(read) input: StorageBuffer<int>,\n    @storage(read_write) output: StorageBuffer<int>,\n    @uniform params: Params,\n) {{\n    let i = global_invocation_id().x\n    if i >= params.n {{ return }}\n    storage_store(output, i, storage_load(input, i))\n}}\n\nglobal EXTRA_COPY_VALUES: string = rock_compute(copy_values_extra)\n"
        );
        std::fs::write(&added_rock, addition).expect("added compute root");
        let added = added_stone
            .recompile(std::slice::from_ref(&added_rock), &added_main, &added_incremental)
            .expect("added compute root reload");
        match added {
            SkullReload::FullRequired { reasons } => assert!(
                reasons.iter().any(|reason| reason == "shader registration key set changed"),
                "root addition must identify the registration key set: {reasons:?}"
            ),
            other => panic!("adding a compute root must require restart, got {other:?}"),
        }
        assert_eq!(std::fs::read_to_string(&added_main).unwrap(), added_baseline);
    }
}

#[test]
fn mag_panel_definitions_do_not_duplicate_the_catalog_key_in_panel_defs() {
    let source = include_str!("../../../mag/panels.rock");
    let field_like_keys = source
        .lines()
        .filter(|line| line.trim_start().starts_with("key:"))
        .count();
    assert_eq!(field_like_keys, 0, "PanelDef factories must not carry a second membership key");
}

#[cfg(test)]
mod definition_transition_tests {
    use super::super::{SkullReload, SkullStone};
    use std::path::PathBuf;

    const V0: &str = r#"
@artifact_definition(catalog="widgets", key_param="key")
deco init widget(key: string) {}

@widget(key="alpha") fn make_alpha() -> int { return 1 }
@widget(key="beta") fn make_beta() -> int { return 2 }
fn main() {}
"#;

    const V1: &str = r#"
@artifact_definition(catalog="widgets", key_param="key")
deco init widget(key: string) {}

@widget(key="beta") fn make_beta() -> int { return 20 }
@widget(key="gamma") fn make_gamma() -> int { return 3 }
fn main() {}
"#;

    #[test]
    fn full_rebuild_transition_replaces_definition_membership_and_rejects_failed_build() {
        let mut harness = DefinitionTransitionHarness::boot(V0);
        let old_artifact = harness.current_artifact();
        let old = harness.published_catalog_from(old_artifact.clone());
        assert_eq!(old, vec!["2", "alpha", "beta"]);
        assert!(old.first().is_some_and(|count| count != "0"), "catalog population must be nonzero");

        let reload = harness.replace_source(V1).expect("full definition rebuild");
        assert!(matches!(reload, SkullReload::Full { .. }), "structural definition edit must use full path: {reload:?}");
        assert_eq!(harness.published_catalog(), vec!["2", "beta", "gamma"]);

        let failed = harness.replace_source(
            r#"
@artifact_definition(catalog="widgets", key_param="key")
deco init widget(key: string) {}
@widget(key="beta") fn make_beta() -> int { return 20 }
            @widget(key="beta") fn make_other() -> int { return 4 }
fn main() {}
"#,
        );
        assert!(failed.is_err(), "duplicate keys must fail the full rebuild");
        assert_eq!(harness.last_good_catalog(), vec!["2", "beta", "gamma"]);
        assert_eq!(harness.published_catalog_from(old_artifact), vec!["2", "alpha", "beta"]);
    }

    #[test]
    fn late_full_publication_failure_keeps_the_last_good_artifact() {
        let mut harness = DefinitionTransitionHarness::boot(V0);
        let old_main = std::fs::read_to_string(&harness.main).expect("last-good main");
        let blocked_main = harness._dir.path().join("blocked-main");
        std::fs::create_dir(&blocked_main).expect("blocked publication path");

        let failed = harness.replace_source_at(V1, &blocked_main);
        assert!(failed.is_err(), "a publication failure must surface");
        assert_eq!(
            std::fs::read_to_string(&harness.main).expect("main after failed publication"),
            old_main,
            "failed full publication must not replace the last-good artifact"
        );
        assert_eq!(
            harness
                .stone
                .source_text(&harness.source)
                .expect("retained source"),
            V0,
            "failed full publication must roll the compiler Session back with the artifact"
        );
        assert_eq!(harness.last_good_catalog(), vec!["2", "alpha", "beta"]);
    }

    #[test]
    fn failed_definition_materialization_keeps_old_definitions_and_state() {
        let mut harness = StateTransitionHarness::boot(STATE_V0);
        let failing_source = STATE_V1.replace(
            "@widget(key=\"gamma\") fn make_gamma() -> WidgetDef { return WidgetDef { callback: () => 3 } }",
            "fn materialization_should_fail() -> bool { return true }\n@widget(key=\"gamma\") fn make_gamma() -> WidgetDef { if materialization_should_fail() { error(\"definition materialization failed\") } return WidgetDef { callback: () => 3 } }",
        );
        harness
            .replace_source(&failing_source)
            .expect("the failing materializer must compile and publish its new plan");
        let output = harness.run_materialization_failure_script(harness.current_artifact());
        assert_eq!(
            output,
            vec!["old:2:alpha:beta:9:1:41:1"],
            "a failed materialization must not leak a new definition or PanelState"
        );
    }

    #[test]
    fn definition_reconciliation_preserves_prunes_and_recreates_state() {
        let mut harness = StateTransitionHarness::boot(STATE_V0);
        harness.replace_source(STATE_V1).expect("remove alpha and add gamma");
        let v1 = harness.current_artifact();
        harness.replace_source(STATE_V2).expect("remove gamma and re-add alpha");
        let v2 = harness.current_artifact();
        let output = harness.run_state_script(v1, v2);
        assert_eq!(
            output,
            vec![
                "v0:2:alpha:beta",
                "v1:2:beta:gamma:41:1:20:0:0:3",
                "v2:2:alpha:beta:0:0:10:41:1:30",
            ],
            "state transition verdict must cover the nonzero catalog and every membership/state arm"
        );
    }

    #[test]
    fn body_only_definition_callback_changes_body_and_preserves_state() {
        let mut harness = StateTransitionHarness::boot(STATE_V0);
        let body_source = STATE_V0.replace("() => 2", "() => 22");
        let reload = harness.replace_source(&body_source).expect("body-only definition edit");
        assert!(matches!(reload, SkullReload::Incremental { .. }), "callback body edit must stay on overlay path: {reload:?}");
        let overlay = std::fs::read_to_string(&harness.incremental).expect("definition overlay");
        assert!(!overlay.contains("__rock_definition_plan_stage"), "body-only overlay must not replace catalog membership");
        assert_eq!(harness.run_body_script(overlay), vec!["body:41:1:22"]);
    }

    struct DefinitionTransitionHarness {
        _dir: tempfile::TempDir,
        stone: SkullStone,
        main: PathBuf,
        incremental: PathBuf,
        source: PathBuf,
        initial_artifact: String,
        last_good_artifact: String,
    }

    type StateTransitionHarness = DefinitionTransitionHarness;

    const STATE_V0: &str = r#"
struct WidgetDef {
    callback: fn() -> int,
}
struct WidgetState {
    seq: int = 0,
    presence: int = 0,
}
@artifact_definition(catalog="widgets", key_param="key")
deco init widget(key: string) {}
@lifecycle(phase="wire", owner="session")
deco init wire {}
@artifact_catalog(catalog="widgets")
@intrinsic extern fn widget_definition_plan() -> [ArtifactDefinition<WidgetDef>]
global WIDGET_STATES: Map<string, WidgetState> = map_new()
@widget(key="alpha") fn make_alpha() -> WidgetDef { return WidgetDef { callback: () => 1 } }
@widget(key="beta") fn make_beta() -> WidgetDef { return WidgetDef { callback: () => 2 } }
@wire fn reconcile_widget_state() {
    let present: Map<string, bool> = map_new()
    for entry in widget_definition_plan() {
        present[entry.key] = true
        if WIDGET_STATES.get(entry.key) == null { WIDGET_STATES[entry.key] = WidgetState {} }
    }
    let removed: [string] = []
    for (key, _) in WIDGET_STATES {
        if present.get(key) == null { removed.push(key) }
    }
    for key in removed { WIDGET_STATES.remove(key) }
}
fn main() {}
"#;

    const STATE_V1: &str = r#"
struct WidgetDef { callback: fn() -> int }
struct WidgetState { seq: int = 0, presence: int = 0 }
@artifact_definition(catalog="widgets", key_param="key")
deco init widget(key: string) {}
@lifecycle(phase="wire", owner="session")
deco init wire {}
@artifact_catalog(catalog="widgets")
@intrinsic extern fn widget_definition_plan() -> [ArtifactDefinition<WidgetDef>]
global WIDGET_STATES: Map<string, WidgetState> = map_new()
@widget(key="beta") fn make_beta() -> WidgetDef { return WidgetDef { callback: () => 20 } }
@widget(key="gamma") fn make_gamma() -> WidgetDef { return WidgetDef { callback: () => 3 } }
@wire fn reconcile_widget_state() {
    let present: Map<string, bool> = map_new()
    for entry in widget_definition_plan() {
        present[entry.key] = true
        if WIDGET_STATES.get(entry.key) == null { WIDGET_STATES[entry.key] = WidgetState {} }
    }
    let removed: [string] = []
    for (key, _) in WIDGET_STATES {
        if present.get(key) == null { removed.push(key) }
    }
    for key in removed { WIDGET_STATES.remove(key) }
}
fn main() {}
"#;

    const STATE_V2: &str = r#"
struct WidgetDef { callback: fn() -> int }
struct WidgetState { seq: int = 0, presence: int = 0 }
@artifact_definition(catalog="widgets", key_param="key")
deco init widget(key: string) {}
@lifecycle(phase="wire", owner="session")
deco init wire {}
@artifact_catalog(catalog="widgets")
@intrinsic extern fn widget_definition_plan() -> [ArtifactDefinition<WidgetDef>]
global WIDGET_STATES: Map<string, WidgetState> = map_new()
@widget(key="alpha") fn make_alpha() -> WidgetDef { return WidgetDef { callback: () => 10 } }
@widget(key="beta") fn make_beta() -> WidgetDef { return WidgetDef { callback: () => 30 } }
@wire fn reconcile_widget_state() {
    let present: Map<string, bool> = map_new()
    for entry in widget_definition_plan() {
        present[entry.key] = true
        if WIDGET_STATES.get(entry.key) == null { WIDGET_STATES[entry.key] = WidgetState {} }
    }
    let removed: [string] = []
    for (key, _) in WIDGET_STATES {
        if present.get(key) == null { removed.push(key) }
    }
    for key in removed { WIDGET_STATES.remove(key) }
}
fn main() {}
"#;

    impl DefinitionTransitionHarness {
        fn boot(source: &str) -> Self {
            let dir = tempfile::tempdir().expect("transition tempdir");
            let root = dir.path().to_str().expect("transition root").to_string();
            let main = dir.path().join("main.lua");
            let incremental = dir.path().join("incremental.lua");
            let source_path = dir.path().join("main.rock");
            std::fs::write(&source_path, source).expect("initial source");
            let stone = SkullStone::init(&root, &main, &incremental).expect("initial build");
            let mut harness = Self {
                _dir: dir,
                stone,
                main,
                incremental,
                source: source_path,
                initial_artifact: String::new(),
                last_good_artifact: String::new(),
            };
            harness.initial_artifact = harness.current_artifact();
            harness.last_good_artifact = harness.current_artifact();
            harness
        }

        fn replace_source(&mut self, source: &str) -> anyhow::Result<SkullReload> {
            let reload = self.replace_source_at(source, &self.main.clone())?;
            self.last_good_artifact = self.current_artifact();
            Ok(reload)
        }

        fn replace_source_at(
            &mut self,
            source: &str,
            main: &std::path::Path,
        ) -> anyhow::Result<SkullReload> {
            std::fs::write(&self.source, source).expect("replacement source");
            let reload = self.stone.recompile(
                std::slice::from_ref(&self.source),
                main,
                &self.incremental,
            )?;
            Ok(reload)
        }

        fn current_artifact(&self) -> String {
            let full = std::fs::read_to_string(&self.main).expect("written main artifact");
            full.strip_suffix("\n__rock_main_wrapper(main)\n")
                .unwrap_or(&full)
                .to_string()
        }

        fn published_catalog(&self) -> Vec<String> {
            self.published_catalog_from(self.current_artifact())
        }

        fn last_good_catalog(&self) -> Vec<String> {
            self.published_catalog_from(self.last_good_artifact.clone())
        }

        fn published_catalog_from(&self, artifact: String) -> Vec<String> {
            let probe = r#"
local catalog = (_G.__ROCK_DEFINITION_PLAN or {}).widgets
assert(catalog ~= nil, "widgets catalog was not published")
io.write("__DEFINITION__", tostring(catalog.len), "\n")
for i = 1, catalog.len do
  io.write("__DEFINITION__", catalog._data[i].key, "\n")
end
"#;
            let output = match super::exec_lua(&format!("{artifact}\n{probe}")) {
                super::ExecOutcome::Output(output) => output,
                super::ExecOutcome::Errored(error) => {
                    panic!("definition transition artifact failed: {error}")
                }
            };
            output
                .lines()
                .filter_map(|line| line.strip_prefix("__DEFINITION__"))
                .map(str::to_string)
                .collect()
        }

        fn run_state_script(&self, v1: String, v2: String) -> Vec<String> {
            let driver = r#"
_G.HOT_RELOAD = true
local function wire()
  local plan = _G.__ROCK_LIFECYCLE_PLAN.wire
  assert(plan ~= nil and plan.len == 1, "state fixture wire population")
  plan._data[1].func()
end
local function published_plan()
  return (_G.__ROCK_DEFINITION_PLAN or {}).widgets
end
local function find(key)
  local plan = published_plan()
  for i = 1, plan.len do
    local entry = plan._data[i]
    if entry.key == key then return entry.definition end
  end
  return nil
end
local function line(s) io.write("__STATE__" .. s .. "\n") end

REPLACE_INITIAL
wire()
local plan = published_plan()
line("v0:" .. plan.len .. ":" .. plan._data[1].key .. ":" .. plan._data[2].key)
WIDGET_STATES["beta"].seq = 41
WIDGET_STATES["beta"].presence = 1
WIDGET_STATES["alpha"].seq = 9
WIDGET_STATES["alpha"].presence = 1

REPLACE_V1
wire()
local beta = find("beta")
local gamma = find("gamma")
assert(find("alpha") == nil, "removed key still has a definition")
plan = published_plan()
line("v1:" .. plan.len .. ":" .. plan._data[1].key .. ":" .. plan._data[2].key .. ":" .. WIDGET_STATES["beta"].seq .. ":" .. WIDGET_STATES["beta"].presence .. ":" .. beta.callback() .. ":" .. WIDGET_STATES["gamma"].seq .. ":" .. WIDGET_STATES["gamma"].presence .. ":" .. gamma.callback())

REPLACE_V2
wire()
local alpha = find("alpha")
beta = find("beta")
assert(find("gamma") == nil, "removed key still has state or definition")
plan = published_plan()
line("v2:" .. plan.len .. ":" .. plan._data[1].key .. ":" .. plan._data[2].key .. ":" .. WIDGET_STATES["alpha"].seq .. ":" .. WIDGET_STATES["alpha"].presence .. ":" .. alpha.callback() .. ":" .. WIDGET_STATES["beta"].seq .. ":" .. WIDGET_STATES["beta"].presence .. ":" .. beta.callback())
"#;
            let script = driver
                .replace("REPLACE_INITIAL", &self.initial_artifact)
                .replace("REPLACE_V1", &v1)
                .replace("REPLACE_V2", &v2);
            let output = match super::exec_lua(&script) {
                super::ExecOutcome::Output(output) => output,
                super::ExecOutcome::Errored(error) => panic!("state transition artifact failed: {error}"),
            };
            output
                .lines()
                .filter_map(|line| line.strip_prefix("__STATE__"))
                .map(str::to_string)
                .collect()
        }

        fn run_body_script(&self, overlay: String) -> Vec<String> {
            let driver = r#"
_G.HOT_RELOAD = true
local function published_plan()
  return (_G.__ROCK_DEFINITION_PLAN or {}).widgets
end
REPLACE_INITIAL
local plan = _G.__ROCK_LIFECYCLE_PLAN.wire
assert(plan ~= nil and plan.len == 1, "body fixture wire population")
plan._data[1].func()
WIDGET_STATES["beta"].seq = 41
WIDGET_STATES["beta"].presence = 1
REPLACE_OVERLAY
plan._data[1].func()
local beta
plan = published_plan()
for i = 1, plan.len do
  if plan._data[i].key == "beta" then beta = plan._data[i].definition end
end
assert(beta ~= nil, "body overlay retained beta definition")
io.write("__STATE__body:" .. WIDGET_STATES["beta"].seq .. ":" .. WIDGET_STATES["beta"].presence .. ":" .. beta.callback() .. "\n")
"#;
            let script = driver
                .replace("REPLACE_INITIAL", &self.initial_artifact)
                .replace("REPLACE_OVERLAY", &overlay);
            let output = match super::exec_lua(&script) {
                super::ExecOutcome::Output(output) => output,
                super::ExecOutcome::Errored(error) => panic!("body transition artifact failed: {error}"),
            };
            output
                .lines()
                .filter_map(|line| line.strip_prefix("__STATE__"))
                .map(str::to_string)
                .collect()
        }

        fn run_materialization_failure_script(&self, failed_artifact: String) -> Vec<String> {
            let driver = r#"
_G.HOT_RELOAD = true
local function published_plan()
  return (_G.__ROCK_DEFINITION_PLAN or {}).widgets
end
REPLACE_INITIAL
local plan = _G.__ROCK_LIFECYCLE_PLAN.wire
assert(plan ~= nil and plan.len == 1, "materialization fixture wire population")
plan._data[1].func()
WIDGET_STATES["alpha"].seq = 9
WIDGET_STATES["alpha"].presence = 1
WIDGET_STATES["beta"].seq = 41
WIDGET_STATES["beta"].presence = 1

local ok = pcall(function()
REPLACE_FAILED
end)
assert(not ok, "the materialization failure must be observed")
assert(_G.__rock_definition_plan_pending ~= nil, "failed materialization must leave a pending plan for abort")
__rock_code_slot_abort()
assert(_G.__rock_definition_plan_pending == nil, "definition plan abort did not clear pending state")
plan = published_plan()
assert(plan.len == 2, "failed materialization replaced old definitions")
assert(plan._data[1].key == "alpha" and plan._data[2].key == "beta", "old definitions were not retained")
assert(plan._data[1].definition.callback() == 1 and plan._data[2].definition.callback() == 2, "old published definitions were not retained")
assert(WIDGET_STATES["alpha"].seq == 9 and WIDGET_STATES["alpha"].presence == 1, "alpha state changed")
assert(WIDGET_STATES["beta"].seq == 41 and WIDGET_STATES["beta"].presence == 1, "beta state changed")
assert(WIDGET_STATES["gamma"] == nil, "failed materialization leaked gamma state")
io.write("__STATE__old:" .. plan.len .. ":" .. plan._data[1].key .. ":" .. plan._data[2].key .. ":" .. WIDGET_STATES["alpha"].seq .. ":" .. WIDGET_STATES["alpha"].presence .. ":" .. WIDGET_STATES["beta"].seq .. ":" .. WIDGET_STATES["beta"].presence .. "\n")
"#;
            let script = driver
                .replace("REPLACE_INITIAL", &self.initial_artifact)
                .replace("REPLACE_FAILED", &failed_artifact);
            let output = match super::exec_lua(&script) {
                super::ExecOutcome::Output(output) => output,
                super::ExecOutcome::Errored(error) => panic!("materialization failure artifact failed: {error}"),
            };
            output
                .lines()
                .filter_map(|line| line.strip_prefix("__STATE__"))
                .map(str::to_string)
                .collect()
        }
    }
}

#[cfg(test)]
mod wire_transition_tests {
    use super::super::{SkullReload, SkullStone};
    use std::path::PathBuf;

    const V0: &str = r#"
@lifecycle(phase="wire", owner="session")
deco init wire {}

const DIRECT: int = 1
const BASE: int = 2
const DERIVED: int = BASE + 1
const UNRELATED: int = 9

fn unrelated() -> int { return UNRELATED }
@wire fn plain_wire() { let _ = unrelated() }
@wire(onchange=[DIRECT]) fn direct_wire() { let _ = DIRECT }
@wire(onchange=[DERIVED]) fn transitive_wire() { let _ = DERIVED }
fn main() {}
"#;

    fn boot(source: &str) -> (tempfile::TempDir, SkullStone, PathBuf, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().expect("wire transition tempdir");
        let root = dir.path().to_str().expect("wire transition root").to_string();
        let main = dir.path().join("main.lua");
        let incremental = dir.path().join("incremental.lua");
        let source_path = dir.path().join("main.rock");
        std::fs::write(&source_path, source).expect("initial wire source");
        let stone = SkullStone::init(&root, &main, &incremental).expect("initial wire build");
        (dir, stone, main, incremental, source_path)
    }

    fn replace_source(
        stone: &mut SkullStone,
        source_path: &PathBuf,
        source: &str,
        main: &PathBuf,
        incremental: &PathBuf,
    ) -> anyhow::Result<SkullReload> {
        std::fs::write(source_path, source).expect("wire replacement source");
        stone.recompile(std::slice::from_ref(source_path), main, incremental)
    }

    #[test]
    fn compatible_wire_edits_overlay_and_structural_changes_take_full_path() {
        let (_dir, mut stone, main, incremental, source_path) = boot(V0);
        let baseline = std::fs::read_to_string(&main).expect("wire baseline");
        assert!(baseline.contains("__ROCK_LIFECYCLE_PLAN"), "baseline must publish LifecyclePlan");

        let unrelated = V0.replace("return UNRELATED", "return UNRELATED + 1");
        assert!(matches!(
            replace_source(&mut stone, &source_path, &unrelated, &main, &incremental),
            Ok(SkullReload::Incremental { .. })
        ));
        let overlay = std::fs::read_to_string(&incremental).expect("wire overlay");
        assert!(overlay.contains("__rock_lifecycle_plan_stage("));
        assert!(overlay.contains("__rock_code_slot_publish()"));

        let direct = unrelated.replace("const DIRECT: int = 1", "const DIRECT: int = 2");
        assert!(matches!(
            replace_source(&mut stone, &source_path, &direct, &main, &incremental),
            Ok(SkullReload::Incremental { .. })
        ));

        let structural = direct.replace(
            "@wire(onchange=[DIRECT])",
            "@wire(onchange=[BASE])",
        );
        assert!(matches!(
            replace_source(&mut stone, &source_path, &structural, &main, &incremental),
            Ok(SkullReload::Full { .. })
        ));
        assert!(!incremental.exists(), "full LifecyclePlan transition must wipe the overlay");
        let full = std::fs::read_to_string(&main).expect("structural wire artifact");
        assert_ne!(full, baseline, "structural wire transition must publish the candidate artifact");

        let (_dir_added, mut added_stone, added_main, added_incremental, added_source) = boot(V0);
        let added = format!(
            "{V0}\n@wire(onchange=[UNRELATED]) fn added_wire() {{ let _ = UNRELATED }}\n"
        );
        assert!(matches!(
            replace_source(
                &mut added_stone,
                &added_source,
                &added,
                &added_main,
                &added_incremental,
            ),
            Ok(SkullReload::Full { .. })
        ));
        assert!(!added_incremental.exists(), "wire addition must take the full path");
    }

    #[test]
    fn gated_wire_removal_requires_restart_and_keeps_the_last_good_artifact() {
        let (_dir, mut stone, main, incremental, source_path) = boot(V0);
        let baseline = std::fs::read_to_string(&main).expect("wire baseline");
        let removed = V0.replace("@wire(onchange=[DIRECT]) fn direct_wire() { let _ = DIRECT }\n", "");
        let reload = replace_source(&mut stone, &source_path, &removed, &main, &incremental)
            .expect("gated removal route");
        match reload {
            SkullReload::FullRequired { reasons } => assert!(
                reasons.iter().any(|reason| reason.contains("removing a gated @wire")),
                "removal reason must request a fresh projection: {reasons:?}"
            ),
            other => panic!("gated removal must not publish live state: {other:?}"),
        }
        assert_eq!(std::fs::read_to_string(&main).expect("retained artifact"), baseline);
        assert_eq!(
            stone.source_text(&source_path).as_deref(),
            Some(removed.as_str()),
            "the current source may remain restart-pending while the published artifact stays old"
        );
    }

    #[test]
    fn failed_compile_and_failed_publication_retain_last_good_wire_artifacts() {
        let (_dir, mut stone, main, incremental, source_path) = boot(V0);
        let baseline = std::fs::read_to_string(&main).expect("wire baseline");
        let invalid = V0.replace("@wire(onchange=[DIRECT])", "@wire(onchange=[UNKNOWN])");
        assert!(
            replace_source(&mut stone, &source_path, &invalid, &main, &incremental).is_err(),
            "an unknown dependency must fail before publication"
        );
        assert_eq!(std::fs::read_to_string(&main).expect("last-good main"), baseline);
        assert_eq!(stone.source_text(&source_path).as_deref(), Some(V0));

        let blocked = source_path.parent().unwrap().join("blocked-incremental");
        std::fs::create_dir(&blocked).expect("blocked publication directory");
        let body_edit = V0.replace("return UNRELATED", "return UNRELATED + 1");
        std::fs::write(&source_path, &body_edit).expect("body edit");
        let failed = stone.recompile(
            std::slice::from_ref(&source_path),
            &main,
            &blocked,
        );
        assert!(failed.is_err(), "a failed overlay write must surface");
        assert_eq!(std::fs::read_to_string(&main).expect("main after failed publication"), baseline);
        assert_eq!(stone.source_text(&source_path).as_deref(), Some(V0));
    }
}

#[cfg(test)]
mod force_full_tests {
    //! `force_full` is the stone restart-path canonicalizer: after body edits
    //! have accumulated into `incremental.lua` (overlaying a stale `main.lua`),
    //! it must fold the overlay back so the next `main.lua`-only process load is
    //! the complete current program. Gate: the produced `main.lua` is
    //! byte-identical to a from-scratch full build of the edited source, and the
    //! overlay file is gone.

    use super::super::SkullStone;
    use crate::skull_backend::SkullReload;

    /// `wipe_overlay` treats a MISSING overlay as fine (the normal "no overlay"
    /// case) but SURFACES any genuine removal failure -- a stale incremental.lua
    /// left beside a fresh main.lua is exactly the overlay/main desync we guard.
    #[test]
    fn wipe_overlay_missing_ok_real_failure_surfaces() {
        let dir = tempfile::tempdir().expect("tempdir");
        // No overlay on disk -> Ok.
        assert!(
            super::super::wipe_overlay(&dir.path().join("incremental.lua")).is_ok(),
            "a missing overlay must be Ok (nothing to wipe)"
        );
        // A real failure: the path is a directory, so remove_file can't remove it
        // and the error is NOT NotFound -> must surface as Err.
        let as_dir = dir.path().join("overlay_is_a_dir");
        std::fs::create_dir(&as_dir).expect("mkdir");
        assert!(
            super::super::wipe_overlay(&as_dir).is_err(),
            "a genuine overlay-removal failure must surface, not be swallowed"
        );
    }

    #[test]
    fn full_rebuilds_rewrite_stick_asset_overlay_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dir_str = dir.path().to_str().unwrap().to_string();
        let main_lua = dir.path().join("main.lua");
        let incremental = dir.path().join("incremental.lua");
        let source = dir.path().join("main.rock");
        let stick = dir.path().join("stick.toml");
        std::fs::write(&source, "fn main() {}\n").unwrap();
        let write_stick = |overlay: &str| {
            std::fs::write(
                &stick,
                format!(
                    "[package]\nname = \"preamble-refresh\"\n[assets]\noverlays = [{overlay:?}]\n"
                ),
            )
            .unwrap();
        };

        write_stick("/tmp/overlay-v1");
        let mut skull = SkullStone::init(&dir_str, &main_lua, &incremental).expect("init");
        assert!(std::fs::read_to_string(&main_lua).unwrap().contains("/tmp/overlay-v1"));

        write_stick("/tmp/overlay-v2");
        let reload = skull.recompile(std::slice::from_ref(&stick), &main_lua, &incremental).unwrap();
        assert!(matches!(reload, SkullReload::Full { .. }));
        let rebuilt = std::fs::read_to_string(&main_lua).unwrap();
        assert!(rebuilt.contains("/tmp/overlay-v2"));
        assert!(!rebuilt.contains("/tmp/overlay-v1"));

        write_stick("/tmp/overlay-v3");
        skull.force_full(&main_lua, &incremental).unwrap();
        let forced = std::fs::read_to_string(&main_lua).unwrap();
        assert!(forced.contains("/tmp/overlay-v3"));
        assert!(!forced.contains("/tmp/overlay-v2"));
    }

    /// init at v1 -> body-edit to v2 (writes the overlay) -> force_full. The
    /// canonical `main.lua` must equal a fresh full build of v2, must DIFFER from
    /// the v1 baseline (proving the edit was actually folded in, not served
    /// stale), and `incremental.lua` must be wiped.
    #[test]
    fn force_full_folds_overlay_matches_fresh_and_wipes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dir_str = dir.path().to_str().unwrap().to_string();
        let src = |p: &str| dir.path().join(p);

        let main_lua = src("main.lua");
        let inc_lua = src("incremental.lua");
        let rock = src("main.rock");

        // Needs a `main`: emission is reachability-driven, so a fixture with no
        // entry point at all emits no user code and the two builds would compare
        // equal for a reason that has nothing to do with the fold.
        let v1 = "fn helper() -> int { 3 }\nfn use_it() -> int { helper() }\nfn main() { print(\"{use_it()}\") }\n";
        let v2 = "fn helper() -> int { 5 }\nfn use_it() -> int { helper() }\nfn main() { print(\"{use_it()}\") }\n";

        // init at v1: writes main.lua(v1), wipes any overlay.
        std::fs::write(&rock, v1).unwrap();
        let mut skull = SkullStone::init(&dir_str, &main_lua, &inc_lua).expect("init v1");
        let main_v1 = std::fs::read_to_string(&main_lua).expect("main.lua after init");
        assert!(!inc_lua.exists(), "init must leave no overlay");

        // Body edit to v2: a BodyOnly recompile writes the overlay; main.lua
        // stays the v1 baseline. Passing the changed file exercises the fast path
        // (re-read only `main.rock`); if it failed to pick up v2, the byte-identity
        // check against a fresh v2 build below would catch it.
        std::fs::write(&rock, v2).unwrap();
        let reload = skull.recompile(&[rock.clone()], &main_lua, &inc_lua).expect("recompile v2");
        assert!(
            matches!(reload, SkullReload::Incremental { .. }),
            "a const body edit should route to the overlay (Incremental tier)"
        );
        assert!(inc_lua.exists(), "body edit must produce an overlay");
        assert_eq!(
            std::fs::read_to_string(&main_lua).unwrap(),
            main_v1,
            "a body edit must NOT touch main.lua (overlay carries the change)"
        );

        // force_full: fold the overlay back into main.lua, wipe the overlay.
        skull.force_full(&main_lua, &inc_lua).expect("force_full");
        let main_forced = std::fs::read_to_string(&main_lua).expect("main.lua after force_full");
        assert!(!inc_lua.exists(), "force_full must wipe the overlay");
        assert_ne!(
            main_forced, main_v1,
            "force_full must fold the v2 edit into main.lua, not serve the stale v1 baseline"
        );

        // Ground truth: a from-scratch full build of v2 (separate output path,
        // same on-disk source). force_full's main.lua must match it byte-for-byte.
        let fresh_main = src("fresh_main.lua");
        let fresh_inc = src("fresh_incremental.lua");
        let fresh = SkullStone::init(&dir_str, &fresh_main, &fresh_inc).expect("fresh init v2");
        let main_fresh = std::fs::read_to_string(&fresh_main).expect("fresh main.lua");
        assert_eq!(
            main_forced, main_fresh,
            "force_full main.lua must be byte-identical to a fresh full build of the edited source"
        );
        assert_eq!(
            skull.artifact_fingerprint(),
            fresh.artifact_fingerprint(),
            "force_full must publish the fingerprint of the Artifact it writes"
        );
    }

    /// Global DCE prunes a fn no root reaches, so main.lua does NOT define it.
    /// A later BODY edit that adds the first call to that fn must still leave it
    /// defined somewhere: the overlay redefines only CHANGED fns, so without the
    /// graph-delta construction the composed program calls a
    /// global that exists in neither file and dies on load.
    #[test]
    fn overlay_delivers_a_graph_added_fn_the_edit_makes_reachable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dir_str = dir.path().to_str().unwrap().to_string();
        let src = |p: &str| dir.path().join(p);

        let main_lua = src("main.lua");
        let inc_lua = src("incremental.lua");
        let rock = src("main.rock");

        // A LIBRARY fn, since the project's own modules are always roots. `split`
        // is not inlined and pulls `find`/`substring`/`len` with it, so this also
        // covers the multi-hop case rather than just the first edge.
        let v1 = "fn main() { print(\"start\") }\n";
        let v2 = "fn main() { print(\"{split(\\\"a,b\\\", \\\",\\\").len}\") }\n";
        // Only `split` itself: its own callees are inlined into it, so they are
        // legitimately absent from the composed program. The transitive case is
        // covered by the driver's own tests (crates/skull-mir/tests/reach_driver.rs).
        let needed = ["function split_string_string_ret_arr_string("];

        std::fs::write(&rock, v1).unwrap();
        let mut skull = SkullStone::init(&dir_str, &main_lua, &inc_lua).expect("init v1");
        let main_v1 = std::fs::read_to_string(&main_lua).expect("main.lua after init");
        for n in needed {
            assert!(
                !main_v1.contains(n),
                "`{n}` must be pruned at v1 -- otherwise this test proves nothing"
            );
        }

        // Body edit: main now calls `split`, which routes to the overlay.
        std::fs::write(&rock, v2).unwrap();
        let reload = skull.recompile(&[rock.clone()], &main_lua, &inc_lua).expect("recompile v2");
        assert!(
            matches!(reload, SkullReload::Incremental { .. }),
            "a body edit should route to the overlay (Incremental tier)"
        );
        let main_after = std::fs::read_to_string(&main_lua).expect("main.lua after edit");
        let overlay = std::fs::read_to_string(&inc_lua).expect("overlay after edit");
        for n in needed {
            assert!(
                main_after.contains(n) || overlay.contains(n),
                "the edit made `{n}` reachable, so the composed program must define it \
                 somewhere (main.lua: {}, overlay: {})",
                main_after.contains(n),
                overlay.contains(n),
            );
        }
    }

    /// A project with a type error (a call to a non-existent function) must make
    /// `init` FAIL, not silently emit a partial `main.lua`. The bug: emit_full
    /// drops the un-checkable body but keeps its `__deco_init` registration, so
    /// the registered `func` is a nil global -> a baffling runtime nil-call
    /// instead of the clean compile error. `recompile` gates on
    /// `session.diagnostics()`; `init` must too.
    #[test]
    fn init_rejects_project_with_type_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dir_str = dir.path().to_str().unwrap().to_string();
        let src = |p: &str| dir.path().join(p);

        let main_lua = src("main.lua");
        let inc_lua = src("incremental.lua");
        let rock = src("main.rock");

        // `boot` calls a function that does not exist -> bone collects a
        // diagnostic but Session::new still succeeds, so without the gate
        // emit_full produces a partial program.
        std::fs::write(&rock, "fn boot() { does_not_exist() }\n").unwrap();

        let res = SkullStone::init(&dir_str, &main_lua, &inc_lua);
        assert!(
            res.is_err(),
            "init must reject a project whose body references a non-existent function"
        );
        assert!(
            !main_lua.exists(),
            "init must NOT write a partial main.lua when the project has type errors"
        );
    }

    /// A recompile that hits a type error must retain the last-good compiler
    /// Session as well as the last-good written artifact. The restart fold can
    /// therefore proceed against the retained program instead of discovering a
    /// broken intermediate state.
    #[test]
    fn failed_recompile_retains_session_and_last_good_artifact() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dir_str = dir.path().to_str().unwrap().to_string();
        let src = |p: &str| dir.path().join(p);

        let main_lua = src("main.lua");
        let inc_lua = src("incremental.lua");
        let rock = src("main.rock");

        // init at a valid v1 -> main.lua(v1).
        let v1 = "fn helper() -> int { 3 }\nfn use_it() -> int { helper() }\n";
        std::fs::write(&rock, v1).unwrap();
        let mut skull = SkullStone::init(&dir_str, &main_lua, &inc_lua).expect("init v1");
        let main_v1 = std::fs::read_to_string(&main_lua).expect("main.lua after init");

        // Edit to a broken v2 (calls a non-existent function). recompile bails
        // and must restore the v1 compiler Session before returning.
        let v2 = "fn helper() -> int { 3 }\nfn use_it() -> int { nope() }\n";
        std::fs::write(&rock, v2).unwrap();
        let reload = skull.recompile(&[rock.clone()], &main_lua, &inc_lua);
        assert!(reload.is_err(), "recompile must bail on the broken edit");

        // A fold after the failed update now sees the retained v1 Session and
        // succeeds without changing the last-good artifact.
        let res = skull.force_full(&main_lua, &inc_lua);
        assert!(res.is_ok(), "force_full must retain the previous good Session");
        assert_eq!(
            std::fs::read_to_string(&main_lua).unwrap(),
            main_v1,
            "a failed recompile must leave the last-good main.lua untouched"
        );
    }
}

#[cfg(test)]
mod skull_stone_lint_snapshot_tests {
    use super::super::{SkullReload, SkullRouteProfile, SkullStone};
    use std::time::Instant;

    fn print_route_profile(label: &str, profile: &SkullRouteProfile) {
        eprintln!(
            "{label}: total={:.3} project_call={:.3} route_call={:.3} snapshot={:.3} live_update={:.3} live_unaccounted={:.3} route_unaccounted={:.3} source_feed={:.3} session={:.3} diagnostics={:.3} lint={:.3} local_lint={:.3} global_lint={:.3} global_findings={:.3} suppression={:.3} suppression_calls={} report_prepare={:.3} summary_sort={:.3} render={:.3} finding={:.3} shape={:.3} checkpoint={:.3} transactions={} commit={:.3} affected={:.3} emit={:.3} write={:.3} rollback={:.3} unaccounted={:.3}",
            profile.total_ns as f64 / 1_000_000.0,
            profile.live_project_call_ns as f64 / 1_000_000.0,
            profile.route_emit_call_ns as f64 / 1_000_000.0,
            profile.session_snapshot_ns as f64 / 1_000_000.0,
            profile.live_update_ns as f64 / 1_000_000.0,
            profile.live_project_unaccounted_ns as f64 / 1_000_000.0,
            profile.route_unaccounted_ns as f64 / 1_000_000.0,
            profile.source_lookup_feed_ns as f64 / 1_000_000.0,
            profile.session_update_ns as f64 / 1_000_000.0,
            profile.diagnostics_ns as f64 / 1_000_000.0,
            profile.lint_total_ns as f64 / 1_000_000.0,
            profile.local_lint_derivation_ns as f64 / 1_000_000.0,
            profile.global_lint_dependency_ns as f64 / 1_000_000.0,
            profile.global_lint_derivation_ns as f64 / 1_000_000.0,
            profile.suppression_filtering_ns as f64 / 1_000_000.0,
            profile.suppression_filter_calls,
            profile.report_prepare_ns as f64 / 1_000_000.0,
            profile.summary_sort_ns as f64 / 1_000_000.0,
            profile.render_ns as f64 / 1_000_000.0,
            profile.finding_aggregation_ns as f64 / 1_000_000.0,
            profile.publication_shape_checks_ns as f64 / 1_000_000.0,
            profile.transaction_checkpoint_ns as f64 / 1_000_000.0,
            profile.body_transactions_started,
            profile.transaction_commit_ns as f64 / 1_000_000.0,
            profile.affected_chunk_selection_ns as f64 / 1_000_000.0,
            profile.emit_overlay_ns as f64 / 1_000_000.0,
            profile.atomic_output_write_ns as f64 / 1_000_000.0,
            profile.rollback_ns as f64 / 1_000_000.0,
            profile.unaccounted_ns as f64 / 1_000_000.0,
        );
    }

    #[test]
    fn skull_stone_body_edit_uses_one_lint_transaction_snapshot() {
        if !crate::lint_pass::census_enabled() {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_str().unwrap().to_string();
        let main = dir.path().join("main.lua");
        let incremental = dir.path().join("incremental.lua");
        let source = dir.path().join("main.rock");
        let v1 = "fn helper() -> int { 3 }\nfn main() { let unused = helper() }\n";
        let v2 = "fn helper() -> int { 5 }\nfn main() { let unused = helper() }\n";
        std::fs::write(&source, v1).expect("source");
        let mut stone = SkullStone::init(&root, &main, &incremental).expect("init");
        let _ = crate::lint_pass::take_census().expect("census enabled");

        let started = Instant::now();
        let reload = stone
            .apply_edit(&source, v2, &main, &incremental)
            .expect("body edit");
        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        assert!(matches!(reload, SkullReload::Incremental { .. }), "reload: {reload:?}");
        let census = crate::lint_pass::take_census().expect("census enabled");
        assert_eq!(
            census.cache_snapshots.snapshots, 1,
            "SkullStone body edit should own one lint transaction snapshot: {:?}",
            census.cache_snapshots
        );
        assert!(census.cache_snapshots.cached_entries > 0);
        assert!(census.cache_snapshots.cached_findings > 0);
        assert_eq!(
            census.cache_snapshots.clone_ns, 0,
            "body publication must checkpoint the lint cache without cloning it: {:?}",
            census.cache_snapshots
        );
        assert!(census.source_view_requests > 0, "source-view denominator must be populated");
        assert!(census.source_files_borrowed > 0, "source-view file population must be nonzero");
        assert_eq!(
            census.source_views_aliased, census.source_files_borrowed,
            "a body edit must borrow source text instead of cloning the workspace"
        );
        assert_eq!(census.detached_source_bytes, 0);
        assert_eq!(
            census.cache.reuse_totals_reads, 4,
            "reuse accounting must read the four maintained totals once"
        );
        assert_eq!(
            census.cache.suppression_source_scans, 0,
            "unchanged suppression metadata must be reused"
        );
        assert!(census.cache.output_refresh_requests > 0);
        assert!(census.cache.output_files_refreshed > 0);
        assert_eq!(census.cache.output_refresh_requests, 1);
        eprintln!(
            "skull-stone one-edit: elapsed_ms={elapsed_ms:.3} snapshots={} entries={} findings={} clone_ms={:.3}",
            census.cache_snapshots.snapshots,
            census.cache_snapshots.cached_entries,
            census.cache_snapshots.cached_findings,
            census.cache_snapshots.clone_ns as f64 / 1_000_000.0,
        );
    }

    fn direct_overlay(root: &str, target_suffix: &str, edited: &str) -> String {
        let spec = crate::unified_compile::SourceSpec::Directory(std::path::PathBuf::from(root));
        let sources = crate::unified_compile::discover_sources(&spec).expect("discover");
        let files: Vec<(String, String)> = sources
            .files
            .iter()
            .map(|file| {
                let file = file.borrow();
                (file.file.to_id_path(), file.contents.clone())
            })
            .collect();
        let target = files
            .iter()
            .find(|(path, _)| path.ends_with(target_suffix))
            .map(|(path, _)| path.clone())
            .expect("main source");
        let mut session = crate::bone_backend::bone_session_for_emission(
            files,
            std::path::Path::new(root),
        )
        .expect("session");
        let renderer = super::super::skull_renderer_target(root).expect("renderer");
        let mid = skull::user_module_id(session.workspace()).expect("user module");
        let (_, mut cache) = skull::mir_emit::emit_full_in_for_renderer(
            session.workspace(),
            mid,
            Some(root),
            renderer,
        )
        .expect("fresh emit");
        let update = session.update_file(&target, edited).expect("edit");
        assert!(matches!(
            update.tier,
            bone_incremental::session::UpdateTier::BodyOnly
        ));
        let edits = update
            .body_update
            .edit
            .iter()
            .map(|edit| skull::instance_graph::SpanEdit {
                file: edit.file,
                at: edit.at,
                byte_delta: edit.byte_delta,
                line_delta: edit.line_delta,
            })
            .collect::<Vec<_>>();
        skull::mir_emit::emit_overlay_with_span_edits(
            session.workspace(),
            &mut cache,
            &update.reinferred,
            &edits,
        )
        .expect("overlay")
        .lua
    }

    #[test]
    fn skull_stone_overlay_matches_cache_independent_route() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_str().unwrap().to_string();
        let main = dir.path().join("main.lua");
        let incremental = dir.path().join("incremental.lua");
        let source = dir.path().join("main.rock");
        let v1 = "fn helper() -> int { 3 }\nfn main() { let unused = helper() }\n";
        let v2 = "fn helper() -> int { 5 }\nfn main() { let unused = helper() }\n";
        std::fs::write(&source, v1).expect("source");
        let mut stone = SkullStone::init(&root, &main, &incremental).expect("init");
        let reload = stone
            .apply_edit(&source, v2, &main, &incremental)
            .expect("body edit");
        assert!(matches!(reload, SkullReload::Incremental { .. }), "reload: {reload:?}");
        let route_overlay = std::fs::read_to_string(&incremental).expect("route overlay");
        let direct = direct_overlay(&root, "main.rock", v2);
        assert!(!route_overlay.is_empty(), "overlay population must be nonzero");
        assert_eq!(route_overlay, direct, "lint-cache integration changed overlay bytes");
        if let Some(capture) = std::env::var_os("ROCK_STONE_OVERLAY_CAPTURE_DIR") {
            let capture = std::path::Path::new(&capture);
            std::fs::create_dir_all(capture).expect("capture dir");
            std::fs::write(capture.join("route-overlay.lua"), route_overlay.as_bytes())
                .expect("capture route");
            std::fs::write(capture.join("direct-overlay.lua"), direct.as_bytes())
                .expect("capture direct");
        }
    }

    #[test]
    fn skull_stone_mag_overlay_matches_cache_independent_route() {
        let Some(project) = std::env::var_os("ROCK_STONE_MAG_PROJECT") else {
            return;
        };
        let project = std::path::PathBuf::from(project);
        let source = project.join("mobs.rock");
        let v1 = std::fs::read_to_string(&source).expect("MAG source");
        let v2 = super::super::body_digit_flip(&v1, "update_mobs").expect("MAG edit");
        let outputs = tempfile::tempdir().expect("output dir");
        let root = project.to_str().unwrap();
        let main = outputs.path().join("main.lua");
        let incremental = outputs.path().join("incremental.lua");
        let mut stone = SkullStone::init(root, &main, &incremental).expect("MAG init");
        let reload = stone
            .apply_edit(&source, &v2, &main, &incremental)
            .expect("MAG body edit");
        assert!(matches!(reload, SkullReload::Incremental { .. }), "reload: {reload:?}");
        let route_overlay = std::fs::read_to_string(&incremental).expect("route overlay");
        let direct = direct_overlay(root, "mobs.rock", &v2);
        assert!(!route_overlay.is_empty(), "MAG overlay population must be nonzero");
        assert_eq!(route_overlay, direct, "MAG lint-cache integration changed overlay bytes");
        if let Some(capture) = std::env::var_os("ROCK_STONE_OVERLAY_CAPTURE_DIR") {
            let capture = std::path::Path::new(&capture);
            std::fs::create_dir_all(capture).expect("capture dir");
            std::fs::write(capture.join("mag-route-overlay.lua"), route_overlay.as_bytes())
                .expect("capture MAG route");
            std::fs::write(capture.join("mag-direct-overlay.lua"), direct.as_bytes())
                .expect("capture MAG direct");
        }
    }

    #[test]
    fn skull_stone_apply_edit_route_benchmark() {
        if std::env::var_os("ROCK_STONE_ROUTE_BENCH").is_none() {
            return;
        }
        crate::lint_pass::set_policy(crate::lint_pass::LintReport::Off);
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_str().unwrap().to_string();
        let main = dir.path().join("main.lua");
        let incremental = dir.path().join("incremental.lua");
        let source = dir.path().join("main.rock");
        let v1 = "fn helper() -> int { 3 }\nfn main() { let unused = helper() }\n";
        let v2 = "fn helper() -> int { 5 }\nfn main() { let unused = helper() }\n";
        std::fs::write(&source, v1).expect("source");
        let mut stone = SkullStone::init(&root, &main, &incremental).expect("init");
        let _ = crate::lint_pass::take_census();
        let warmup = 20;
        let iters = 80;
        let mut samples = Vec::with_capacity(iters);
        for i in 0..(warmup + iters) {
            let text = if i % 2 == 0 { v2 } else { v1 };
            let started = Instant::now();
            let reload = stone
                .apply_edit(&source, text, &main, &incremental)
                .expect("apply edit");
            assert!(matches!(reload, SkullReload::Incremental { .. }), "reload: {reload:?}");
            if i >= warmup {
                samples.push(started.elapsed().as_secs_f64() * 1000.0);
            }
        }
        let census = crate::lint_pass::take_census().expect("census enabled");
        let mut sorted = samples.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let percentile = |p: f64| sorted[((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize];
        let mean = samples.iter().sum::<f64>() / samples.len() as f64;
        assert_eq!(census.cache_snapshots.snapshots, warmup + iters);
        assert!(census.cache_snapshots.cached_entries > 0);
        assert!(census.cache_snapshots.cached_findings > 0);
        eprintln!(
            "skull-stone apply_edit route: mean={mean:.3} p50={:.3} p99={:.3} snapshots={} entries/edit={:.1} findings/edit={:.1} clone_ms={:.3}",
            percentile(50.0),
            percentile(99.0),
            census.cache_snapshots.snapshots,
            census.cache_snapshots.cached_entries as f64 / census.cache_snapshots.snapshots as f64,
            census.cache_snapshots.cached_findings as f64 / census.cache_snapshots.snapshots as f64,
            census.cache_snapshots.clone_ns as f64 / 1_000_000.0,
        );
        if let Some(profile) = stone.take_last_route_profile() {
            print_route_profile("skull-stone profile", &profile);
        }
    }

    #[test]
    fn skull_stone_mag_apply_edit_route_benchmark() {
        if std::env::var_os("ROCK_STONE_MAG_BENCH").is_none() {
            return;
        }
        crate::lint_pass::set_policy(crate::lint_pass::LintReport::Off);
        let project = std::path::PathBuf::from(
            std::env::var_os("ROCK_STONE_MAG_PROJECT").expect("ROCK_STONE_MAG_PROJECT"),
        );
        let source = project.join("mobs.rock");
        let v1 = std::fs::read_to_string(&source).expect("MAG source");
        let v2 = super::super::body_digit_flip(&v1, "update_mobs").expect("MAG edit");
        let outputs = tempfile::tempdir().expect("output dir");
        let root = project.to_str().unwrap();
        let main = outputs.path().join("main.lua");
        let incremental = outputs.path().join("incremental.lua");
        let mut stone = SkullStone::init(root, &main, &incremental).expect("MAG init");
        let _ = crate::lint_pass::take_census();
        let warmup = 20;
        let iters = 80;
        let mut samples = Vec::with_capacity(iters);
        for i in 0..(warmup + iters) {
            let text = if i % 2 == 0 { &v2 } else { &v1 };
            let started = Instant::now();
            let reload = stone
                .apply_edit(&source, text, &main, &incremental)
                .expect("MAG apply edit");
            assert!(matches!(reload, SkullReload::Incremental { .. }), "reload: {reload:?}");
            if i >= warmup {
                samples.push(started.elapsed().as_secs_f64() * 1000.0);
            }
        }
        let census = crate::lint_pass::take_census().expect("census enabled");
        let mut sorted = samples.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let percentile = |p: f64| sorted[((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize];
        let mean = samples.iter().sum::<f64>() / samples.len() as f64;
        assert_eq!(census.cache_snapshots.snapshots, warmup + iters);
        assert_eq!(census.source_views_aliased, census.source_files_borrowed);
        assert_eq!(census.detached_source_bytes, 0);
        assert_eq!(census.cache.reuse_totals_reads, (warmup + iters) * 4);
        assert_eq!(census.cache.suppression_source_scans, 0);
        assert_eq!(census.cache.suppression_entry_parses, 0);
        assert_eq!(census.cache.output_refresh_requests, warmup + iters);
        assert!(census.cache.output_files_refreshed > 0);
        assert!(census.cache.output_file_rows_read > 0);
        assert!(
            census.source_files_borrowed > census.cache.output_file_rows_read,
            "finding output must visit populated rows, not every borrowed source"
        );
        eprintln!(
            "skull-stone MAG apply_edit: mean={mean:.3} p50={:.3} p99={:.3} snapshots={} entries/edit={:.1} findings/edit={:.1} clone_ms={:.3} source_files={} output_files={} output_owners={}",
            percentile(50.0),
            percentile(99.0),
            census.cache_snapshots.snapshots,
            census.cache_snapshots.cached_entries as f64 / census.cache_snapshots.snapshots as f64,
            census.cache_snapshots.cached_findings as f64 / census.cache_snapshots.snapshots as f64,
            census.cache_snapshots.clone_ns as f64 / 1_000_000.0,
            census.source_files_borrowed,
            census.cache.output_file_rows_read,
            census.cache.output_owner_entries_visited,
        );
        if let Some(profile) = stone.take_last_route_profile() {
            print_route_profile("skull-stone MAG profile", &profile);
        }
    }

    #[test]
    fn skull_stone_apply_edit_profile_covers_the_real_route() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_str().unwrap().to_string();
        let main = dir.path().join("main.lua");
        let incremental = dir.path().join("incremental.lua");
        let source = dir.path().join("main.rock");
        let v1 = "fn helper() -> int { 3 }\nfn main() { let unused = helper() }\n";
        let v2 = "fn helper() -> int { 5 }\nfn main() { let unused = helper() }\n";
        std::fs::write(&source, v1).expect("source");
        let mut stone = SkullStone::init(&root, &main, &incremental).expect("init");
        stone.enable_route_profile();

        let reload = stone
            .apply_edit(&source, v2, &main, &incremental)
            .expect("body edit");
        assert!(matches!(reload, SkullReload::Incremental { .. }), "reload: {reload:?}");
        let profile = stone.take_last_route_profile().expect("route profile");
        assert!(profile.session_snapshot_ns > 0);
        assert!(profile.source_lookup_feed_ns > 0);
        assert!(profile.session_update_ns > 0);
        assert!(profile.diagnostics_ns > 0);
        assert!(profile.local_lint_derivation_ns > 0);
        assert!(profile.global_lint_dependency_ns > 0);
        assert!(profile.suppression_filter_calls > 0);
        assert!(profile.report_prepare_ns > 0);
        assert!(profile.render_ns > 0);
        assert!(profile.finding_aggregation_ns > 0);
        assert!(profile.affected_chunk_selection_ns > 0);
        assert!(profile.emit_overlay_ns > 0);
        assert!(profile.publication_shape_checks_ns > 0);
        assert!(profile.transaction_checkpoint_ns > 0);
        assert_eq!(profile.body_transactions_started, 1);
        assert!(profile.transaction_commit_ns > 0);
        assert!(profile.atomic_output_write_ns > 0);
        assert!(profile.total_ns > 0);
        assert!(profile.unaccounted_ns < profile.total_ns);
    }

    #[test]
    fn failed_body_publication_rolls_back_cache_before_the_next_edit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_str().unwrap().to_string();
        let main = dir.path().join("main.lua");
        let incremental = dir.path().join("incremental.lua");
        let source = dir.path().join("main.rock");
        let v1 = "fn helper() -> int { 3 }\nfn other() -> int { 10 }\nfn main() { helper() }\n";
        let v2 = "fn helper() -> int { 5 }\nfn other() -> int { 10 }\nfn main() { helper() }\n";
        let v3 = "fn helper() -> int { 3 }\nfn other() -> int { 11 }\nfn main() { helper() }\n";
        std::fs::write(&source, v1).expect("source");
        let mut stone = SkullStone::init(&root, &main, &incremental).expect("init");
        if incremental.is_file() {
            std::fs::remove_file(&incremental).expect("initial overlay");
        }
        std::fs::create_dir(&incremental).expect("blocked overlay");

        assert!(
            stone.apply_edit(&source, v2, &main, &incremental).is_err(),
            "a body publication to a directory must fail"
        );
        assert_eq!(stone.source_text(&source).as_deref(), Some(v1));

        std::fs::remove_dir(&incremental).expect("unblock overlay");
        let reload = stone
            .apply_edit(&source, v3, &main, &incremental)
            .expect("next body edit");
        assert!(matches!(reload, SkullReload::Incremental { .. }), "reload: {reload:?}");
        let route_overlay = std::fs::read_to_string(&incremental).expect("overlay");
        let direct = direct_overlay(&root, "main.rock", v3);
        assert_eq!(route_overlay, direct, "rollback must retain the last-good cache");
    }
}

#[cfg(test)]
mod value_test_exec {
    //! `@test(returns = <literal>)` end-to-end on the luajit executor: the
    //! harness emits a VALUE marker, the host compares
    //! (`test_protocol::compare_value`). Discovery rejections (the
    //! returns+expect_failure contradiction) must fail the unit, never route
    //! through the expect-compile-failure escape.

    use super::super::*;

    #[test]
    fn value_mismatch_fails_with_detail() {
        let src = "@test(returns = 43)\nfn r() -> int { return 42 }\n";
        match skull_build("val.rock", src).expect("build ok") {
            SkullOutcome::TestFail { passed, fails } => {
                assert_eq!(passed, 0);
                assert_eq!(fails, vec!["r: value mismatch: expected <43> got <42>".to_string()]);
            }
            SkullOutcome::Pass { .. } => panic!("mismatching value test must FAIL"),
        }
    }

    #[test]
    fn value_tests_pass_int_float_bool_and_with_output() {
        let src = "@test(returns = 42)\nfn a() -> int { return 42 }\n\
                   @test(returns = -1.5)\nfn b() -> float { return -1.5 }\n\
                   @test(returns = true)\nfn c() -> bool { return true }\n\
                   @test(returns = 6, output = \"hi\")\nfn d() -> int { print(\"hi\")\n return 6 }\n";
        match skull_build("val.rock", src).expect("build ok") {
            SkullOutcome::Pass { tests } => assert_eq!(tests, 4),
            SkullOutcome::TestFail { fails, .. } => panic!("expected all pass, got {fails:?}"),
        }
    }

    #[test]
    fn value_test_error_still_fails() {
        let src = "@test(returns = 1)\nfn e() -> int { error(\"boom\")\n return 1 }\n";
        match skull_build("val.rock", src).expect("build ok") {
            SkullOutcome::TestFail { fails, .. } => {
                assert!(fails[0].contains("error:"), "got {fails:?}");
            }
            SkullOutcome::Pass { .. } => panic!("erroring value test must FAIL"),
        }
    }

    #[test]
    fn returns_plus_expect_failure_is_a_unit_failure_not_expected_pass() {
        let src = "@test(expect_failure = true, returns = 42)\nfn contra() -> int { return 42 }\n";
        let err = skull_build("val.rock", src).expect_err("contradiction must reject the unit");
        assert!(
            err.reason.contains("contradictory"),
            "unexpected reason: {}",
            err.reason
        );
    }
}

#[cfg(test)]
mod backend_stats_workspace_tests {
    use super::super::skull_workspace_timed;
    use crate::unified_compile::{discover_sources, SourceSpec};
    use std::path::PathBuf;

    #[test]
    fn timed_emission_prepares_comptime_inputs_and_populates_attribution() {
        let dir = tempfile::tempdir().expect("temporary project");
        std::fs::write(
            dir.path().join("stick.toml"),
            "[package]\nname = \"timed-preparation\"\n\n[build.vars]\nREQUIRED = \"prepared\"\n",
        )
        .expect("stick.toml");
        std::fs::write(
            dir.path().join("main.rock"),
            "fn main() { print(build_var(\"REQUIRED\")) }\n",
        )
        .expect("main.rock");

        let sources = discover_sources(&SourceSpec::Directory(PathBuf::from(dir.path())))
            .expect("discover timed project");
        let (ok, times) = skull_workspace_timed(&sources);

        assert!(ok, "timed emission must succeed for a prepared project");
        assert!(
            times.attribution.bodies_examined > 0,
            "successful timed emission must populate its body denominator"
        );
        assert!(
            !times.emit_phases.is_empty(),
            "successful timed emission must populate production phase attribution"
        );
    }
}
