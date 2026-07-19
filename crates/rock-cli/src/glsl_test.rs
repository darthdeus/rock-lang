//! `rock test <path> --backend glsl` -- execute the value-test class
//! (`@test(returns = <literal>)`) on the GPU via the GLSL emitter
//! (`skull-codegen-glsl`), G1 of `docs/multi-backend-testing-plan.md`.
//!
//! Shares the backend-neutral half of the test driver with the luajit and node
//! paths: unit discovery (`collect_test_units`), the `@test` metadata
//! (`skull::module_tests_with_ids`), the `__SKULL__` protocol parser and the
//! ONE comparison site (`test_protocol::{parse_skull_markers, compare_value}`).
//! The host side never learns the lines came from a GPU -- G1 is a transport
//! for `__SKULL__` lines (the seam contract).
//!
//! GLSL cannot print, so ONLY the value class transfers. Two-tier skip:
//!   1. a test with no `returns=` is outside the value class -- reported as ONE
//!      collapsed count, never a histogram line (most of the suite is
//!      *permanently* ineligible, unlike JS).
//!   2. a test WITH `returns=` whose GLSL emission rejects is a real
//!      SKIP(reason); that per-profile histogram is the A1 fn-emission work
//!      list.
//!
//! Both profiles (330 core + 300 es) run by default, with per-profile verdicts
//! (`name [330core]` / `name [300es]`). The heavy lifting -- creating a headless
//! GL context, compiling the shader, drawing to a 1x1 R32I/R32F target and
//! reading the pixel back -- lives in the standalone `stone-glsl-runner` binary
//! (spawned per unit+profile), so the `rock` compiler/LSP binary never links
//! SDL2. No GL context available -> skip-with-log, mirroring `node` being
//! absent in the JS path. A GL error mid-run is a loud FAIL, never a silent skip.

use crate::bone_backend::{categorize, short_path};
use crate::test_protocol::{compare_value, parse_skull_markers, reason_histogram};
use crate::Result;
use colored::Colorize;
use skull::ValueExpect;
use skull_codegen_glsl::{emit_value_test, GlslProfile, ValueTestShaders};
use skull_mir::mir::FuncRef;
use std::time::Instant;

/// GLSL is validated widely enough that f32 arithmetic and the R32F round-trip
/// introduce error well above the 1e-12 luajit/node tolerance -- ~1e-6 is f32
/// territory (24-bit mantissa). int/bool stay exact regardless.
const GLSL_FLOAT_REL_TOL: f64 = 1e-6;

/// The two emission profiles run by default.
const PROFILES: [(&str, GlslProfile); 2] =
    [("330core", GlslProfile::Core330), ("300es", GlslProfile::Es300)];

/// One profile's tallies over the whole run.
pub struct GlslProfileSummary {
    /// `"330core"` / `"300es"`.
    pub profile: &'static str,
    pub passed: usize,
    /// FAIL detail lines (a GL compile/link/render error, or a value mismatch).
    pub failed: Vec<String>,
    /// Tier-2 SKIP reasons (emission rejected), bucketed count-desc.
    pub skip_reasons: Vec<(String, usize)>,
    /// True once a GL context was created for this profile; false = no display
    /// / driver -> the value verdicts were skipped-with-log.
    pub gl_available: bool,
}

/// Whole-run tallies -- the testable core consumed by the ratchet test.
pub struct GlslSuiteSummary {
    pub files: usize,
    /// Tier-1: `@test` fns with no `returns=` (outside the value class). One
    /// collapsed number, never a histogram.
    pub outside_value_class: usize,
    /// bone-rejected units (unexpected compile failure) / harness breaches.
    pub unit_failures: usize,
    pub per_profile: Vec<GlslProfileSummary>,
}

impl GlslSuiteSummary {
    /// The profile summary for a wire name (`"330core"` / `"300es"`).
    pub fn profile(&self, name: &str) -> &GlslProfileSummary {
        self.per_profile
            .iter()
            .find(|p| p.profile == name)
            .expect("profile present")
    }
}

/// `rock test --backend glsl` entry: run, print per-profile lines + the
/// end-of-run summary, exit non-zero on any FAIL or unit failure.
pub fn glsl_test(paths: &[String]) -> Result<()> {
    let start = Instant::now();
    let summary = glsl_test_run(paths, true)?;

    let total_failed: usize = summary.per_profile.iter().map(|p| p.failed.len()).sum();
    println!(
        "\nglsl backend: {} @tests outside the value class (no returns=), across {} files ({:.2}s)",
        summary.outside_value_class.to_string().yellow(),
        summary.files,
        start.elapsed().as_secs_f64(),
    );
    for p in &summary.per_profile {
        if !p.gl_available {
            println!(
                "  [{}] {} -- value verdicts SKIPPED (no GL context)",
                p.profile.cyan(),
                "no display / driver".yellow()
            );
            continue;
        }
        let total = p.passed + p.failed.len();
        println!(
            "  [{}] {}/{} value tests passed ({} failed)",
            p.profile.cyan(),
            p.passed.to_string().green(),
            total,
            if p.failed.is_empty() {
                p.failed.len().to_string().green()
            } else {
                p.failed.len().to_string().red()
            },
        );
        for detail in &p.failed {
            println!("    {} {}", "FAIL".red(), detail);
        }
        if !p.skip_reasons.is_empty() {
            println!(
                "    {}",
                format!("=== [{}] skip reasons (glsl) -- the A1 fn-emission work list ===", p.profile).bold()
            );
            for (reason, count) in p.skip_reasons.iter().take(50) {
                println!("      {:>5}  {}", count.to_string().yellow(), reason);
            }
        }
    }
    if summary.unit_failures > 0 {
        println!(
            "  {} unit(s) failed to build (see FAILED lines above)",
            summary.unit_failures.to_string().red()
        );
    }
    if total_failed > 0 || summary.unit_failures > 0 {
        std::process::exit(1);
    }
    Ok(())
}

/// Run the suite; optionally print per-unit lines as they complete. Public for
/// the ratchet test (`print = false`).
pub fn glsl_test_run(paths: &[String], print: bool) -> Result<GlslSuiteSummary> {
    let (files, package_dirs) = crate::skull_backend::collect_test_units(paths)?;

    let runner = locate_runner();
    // Probe once per profile: which can create a GL context here? Absent runner
    // or exit-2 (no display/driver) -> that profile's verdicts skip-with-log.
    let mut gl_available: [bool; 2] = [false, false];
    if let Some(runner) = &runner {
        for (i, (_, _)) in PROFILES.iter().enumerate() {
            gl_available[i] = probe_gl(runner, PROFILES[i].0);
        }
    } else {
        log::warn!(
            target: "GLSL",
            "stone-glsl-runner binary not found (build the workspace); GLSL value verdicts skipped"
        );
    }

    let mut summary = GlslSuiteSummary {
        files: 0,
        outside_value_class: 0,
        unit_failures: 0,
        per_profile: PROFILES
            .iter()
            .enumerate()
            .map(|(i, (name, _))| GlslProfileSummary {
                profile: name,
                passed: 0,
                failed: Vec::new(),
                skip_reasons: Vec::new(),
                gl_available: gl_available[i],
            })
            .collect(),
    };
    // Per-profile accumulated skip-reason strings (bucketed at the end).
    let mut skip_reasons: [Vec<String>; 2] = [Vec::new(), Vec::new()];

    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let mut outcomes: Vec<(String, std::thread::Result<UnitOutcome>)> = Vec::new();
    for file in &files {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            build_file(file, runner.as_deref(), &gl_available)
        }));
        outcomes.push((short_path(file), outcome));
    }
    for dir in &package_dirs {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            build_package(dir, runner.as_deref(), &gl_available)
        }));
        outcomes.push((short_path(dir), outcome));
    }
    std::panic::set_hook(prev_hook);

    for (label, outcome) in outcomes {
        summary.files += 1;
        match outcome {
            Ok(UnitOutcome::Ran { outside_value_class, profiles }) => {
                summary.outside_value_class += outside_value_class;
                for (i, pr) in profiles.into_iter().enumerate() {
                    summary.per_profile[i].passed += pr.passed;
                    let any = pr.passed + pr.failed.len() + pr.skips.len();
                    summary.per_profile[i].failed.extend(
                        pr.failed.iter().map(|d| format!("{d} [{}]", PROFILES[i].0)),
                    );
                    for (_, reason) in &pr.skips {
                        skip_reasons[i].push(reason.clone());
                    }
                    if print && gl_available[i] && any > 0 {
                        let status = if pr.failed.is_empty() { "ok".green() } else { "FAILED".red() };
                        println!(
                            "{:.<52} [{}] {}  ({} pass, {} fail, {} skip)",
                            label,
                            PROFILES[i].0.cyan(),
                            status,
                            pr.passed,
                            pr.failed.len(),
                            pr.skips.len(),
                        );
                    }
                }
            }
            Ok(UnitOutcome::UnitFail { reason, rendered }) => {
                summary.unit_failures += 1;
                if print {
                    println!("{:.<60} {}  {}", label, "FAILED".red(), reason.dimmed());
                    if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                        println!("{rendered}");
                    }
                }
            }
            Err(panic) => {
                summary.unit_failures += 1;
                let msg = panic
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown panic".to_string());
                if print {
                    println!("{:.<60} {}  {}", label, "PANIC".red().bold(), msg.dimmed());
                }
            }
        }
    }

    for (i, reasons) in skip_reasons.into_iter().enumerate() {
        summary.per_profile[i].skip_reasons =
            reason_histogram(reasons.iter().map(|s| s.as_str()));
    }
    Ok(summary)
}

/// One profile's per-unit results.
struct ProfileRun {
    passed: usize,
    failed: Vec<String>,
    /// (test name, reason) for value tests whose GLSL emission rejected.
    skips: Vec<(String, String)>,
}

enum UnitOutcome {
    Ran {
        outside_value_class: usize,
        /// One entry per profile, in `PROFILES` order.
        profiles: Vec<ProfileRun>,
    },
    UnitFail {
        reason: String,
        rendered: String,
    },
}

/// One isolated test file: parse + bone type-check, discover `@test` fns, then
/// emit + run the value tests under both profiles.
fn build_file(file: &str, runner: Option<&std::path::Path>, gl_available: &[bool; 2]) -> UnitOutcome {
    let src = match fs_err::read_to_string(file) {
        Ok(s) => s,
        Err(e) => return UnitOutcome::UnitFail { reason: e.to_string(), rendered: e.to_string() },
    };
    let user = match skull_parser::parse_module(&src, ustr::Ustr::from(file)) {
        Ok(u) => u,
        Err(e) => return compile_failure(file, &src, &[e]),
    };
    let user_file = user.file;
    let ws = match crate::bone_backend::check_with_stdlib([user]) {
        Ok(ws) => ws,
        Err(errs) => return compile_failure(file, &src, &errs),
    };
    let user_mid = crate::bone_backend::entry_module_id(&ws, user_file);
    let tests: Vec<(FuncRef, skull::SkullTest)> = match skull::module_tests_with_ids(&ws, user_mid) {
        Ok(tests) => tests
            .into_iter()
            .map(|(fid, t)| (FuncRef { module: user_mid, func: fid }, t))
            .collect(),
        Err(e) => {
            return UnitOutcome::UnitFail {
                reason: categorize(&e.message),
                rendered: crate::skull_backend::render_file_errs(file, &src, &[e]),
            }
        }
    };
    run_tests(&ws, &tests, runner, gl_available)
}

/// A `stick.toml` package built as ONE workspace (tests included), mirroring
/// the JS/luajit package path.
fn build_package(dir: &str, runner: Option<&std::path::Path>, gl_available: &[bool; 2]) -> UnitOutcome {
    let spec = crate::unified_compile::SourceSpec::Directory(std::path::PathBuf::from(dir));
    let sources = match crate::unified_compile::discover_sources(&spec) {
        Ok(s) => s,
        Err(e) => {
            return UnitOutcome::UnitFail {
                reason: "cannot load project".to_string(),
                rendered: e.to_string(),
            }
        }
    };
    let (mut modules, _entry) = match crate::skull_backend::skull_assemble_project(&sources) {
        Ok(v) => v,
        Err(errs) => {
            return UnitOutcome::UnitFail {
                reason: categorize(&errs[0].message),
                rendered: crate::skull_backend::render_project_errs(&sources, &errs),
            }
        }
    };
    let ws = match crate::bone_backend::check_modules(&mut modules) {
        Ok(ws) => ws,
        Err(errs) => {
            return UnitOutcome::UnitFail {
                reason: categorize(&errs[0].message),
                rendered: crate::skull_backend::render_project_errs(&sources, &errs),
            }
        }
    };
    let tests = match crate::skull_backend::collect_package_test_fns(&ws, dir) {
        Ok(tests) => tests,
        Err(e) => return UnitOutcome::UnitFail { reason: categorize(&e.message), rendered: e.message },
    };
    run_tests(&ws, &tests, runner, gl_available)
}

/// A bone rejection where every `@test` expects the failure is the expected
/// (passing) outcome on the luajit path; here it just means zero value tests to
/// run (nothing to emit), so it is not a unit failure.
fn compile_failure(file: &str, src: &str, errs: &[rock_core::CompilerError]) -> UnitOutcome {
    if crate::skull_backend::all_tests_expect_compile_failure(file, src).is_some() {
        return UnitOutcome::Ran {
            outside_value_class: 0,
            profiles: PROFILES
                .iter()
                .map(|_| ProfileRun { passed: 0, failed: Vec::new(), skips: Vec::new() })
                .collect(),
        };
    }
    UnitOutcome::UnitFail {
        reason: categorize(&errs[0].message),
        rendered: crate::skull_backend::render_file_errs(file, src, errs),
    }
}

/// Emit + GPU-run one unit's value tests under both profiles.
fn run_tests(
    ws: &bone_semantic::inference::Workspace,
    tests: &[(FuncRef, skull::SkullTest)],
    runner: Option<&std::path::Path>,
    gl_available: &[bool; 2],
) -> UnitOutcome {
    let mut outside_value_class = 0usize;
    // Value tests only (returns=). Non-value @tests are permanently outside the
    // class -- one collapsed count, never a histogram.
    let value_tests: Vec<(FuncRef, &skull::SkullTest, &ValueExpect)> = tests
        .iter()
        .filter_map(|(fr, t)| match &t.returns {
            Some(expect) => Some((*fr, t, expect)),
            None => {
                outside_value_class += 1;
                None
            }
        })
        .collect();

    let mut profiles: Vec<ProfileRun> = Vec::with_capacity(PROFILES.len());
    for (pi, (pname, profile)) in PROFILES.iter().enumerate() {
        let mut passed = 0usize;
        let mut failed: Vec<String> = Vec::new();
        let mut skips: Vec<(String, String)> = Vec::new();
        // (name, expected, shaders) for the tests that emitted.
        let mut runnable: Vec<(String, ValueExpect, ValueTestShaders)> = Vec::new();

        for (fr, t, expect) in &value_tests {
            // An int expectation outside i32 can't survive the R32I readback
            // (GLSL has no int64) -- a real SKIP, part of the work list.
            if let ValueExpect::Int(i) = expect {
                if *i < i32::MIN as i64 || *i > i32::MAX as i64 {
                    skips.push((
                        t.name.clone(),
                        "value test: returns literal exceeds i32 range (GLSL has no int64)".to_string(),
                    ));
                    continue;
                }
            }
            match emit_value_test(ws, fr.module, &t.name, *profile) {
                Ok(shaders) => runnable.push((t.name.clone(), **expect, shaders)),
                Err(e) => skips.push((t.name.clone(), e.message.clone())),
            }
        }

        // No GL context for this profile -> the runnable ones are skipped with a
        // log; do NOT count them as pass or fail.
        if gl_available[pi] && !runnable.is_empty() {
            match run_batch(runner.expect("runner present when gl_available"), pname, &runnable) {
                Ok(markers) => {
                    failed.extend(markers.fails);
                    passed += markers.passed;
                    let expects: std::collections::HashMap<&str, &ValueExpect> =
                        runnable.iter().map(|(n, e, _)| (n.as_str(), e)).collect();
                    for (name, val) in &markers.values {
                        let Some(expect) = expects.get(name.as_str()) else {
                            failed.push(format!("harness breach: VALUE for `{name}` without an expectation"));
                            continue;
                        };
                        match compare_value(expect, val, GLSL_FLOAT_REL_TOL) {
                            Ok(()) => passed += 1,
                            Err(detail) => failed.push(format!("{name}: {detail}")),
                        }
                    }
                }
                Err(detail) => {
                    // A runner-level failure (bad emitted GLSL, protocol breach)
                    // is a loud FAIL for every runnable test, never a silent skip.
                    for (name, _, _) in &runnable {
                        failed.push(format!("{name}: runner error: {detail}"));
                    }
                }
            }
        }
        profiles.push(ProfileRun { passed, failed, skips });
    }

    UnitOutcome::Ran { outside_value_class, profiles }
}

/// Parsed marker lines from one runner invocation. Reuses the shared parser so
/// the host side is blind to the GPU transport.
struct RunnerMarkers {
    passed: usize,
    fails: Vec<String>,
    values: Vec<(String, String)>,
}

/// Spawn the runner for one unit+profile, feed the shaders as a JSON job array,
/// parse the `__SKULL__` lines back.
fn run_batch(
    runner: &std::path::Path,
    profile: &str,
    runnable: &[(String, ValueExpect, ValueTestShaders)],
) -> std::result::Result<RunnerMarkers, String> {
    let job = jobs_json(runnable);
    let (code, stdout, stderr) = spawn_runner(runner, profile, &job)?;
    if code == Some(2) {
        return Err(format!("runner reported no GL context: {}", stderr.trim()));
    }
    if code != Some(0) {
        return Err(format!("runner exited {code:?}: {}", stderr.trim()));
    }
    let markers = match parse_skull_markers(&stdout) {
        Some(m) if m.passed + m.fails.len() + m.values.len() == runnable.len() => m,
        _ => {
            return Err(format!(
                "invalid runner output (expected {} markers): stdout=<{}> stderr=<{}>",
                runnable.len(),
                stdout.trim(),
                stderr.trim()
            ))
        }
    };
    Ok(RunnerMarkers { passed: markers.passed, fails: markers.fails, values: markers.values })
}

/// Serialize the runnable shaders to the runner's JSON job protocol.
fn jobs_json(runnable: &[(String, ValueExpect, ValueTestShaders)]) -> String {
    let items: Vec<serde_json::Value> = runnable
        .iter()
        .map(|(name, _, s)| {
            serde_json::json!({
                "name": name,
                "vertex": s.vertex,
                "fragment": s.fragment,
                "ret": s.ret.tag(),
            })
        })
        .collect();
    serde_json::Value::Array(items).to_string()
}

/// Probe: can the runner create a GL context for `profile` here? An empty job
/// exercises context creation and exits 0 on success, 2 on no-display/driver.
fn probe_gl(runner: &std::path::Path, profile: &str) -> bool {
    match spawn_runner(runner, profile, "[]") {
        Ok((Some(0), _, _)) => true,
        Ok((code, _, stderr)) => {
            log::warn!(
                target: "GLSL",
                "no GL context for profile {profile} (runner exit {code:?}): {}; verdicts skipped",
                stderr.trim()
            );
            false
        }
        Err(e) => {
            log::warn!(target: "GLSL", "cannot spawn glsl runner: {e}; verdicts skipped");
            false
        }
    }
}

/// Spawn the runner with the game's library-search env (so SDL finds libGL the
/// same way the luajit game does) and pipe the job JSON to stdin.
fn spawn_runner(
    runner: &std::path::Path,
    profile: &str,
    job: &str,
) -> std::result::Result<(Option<i32>, String, String), String> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut cmd = Command::new(runner);
    cmd.arg("--profile").arg(profile);
    cmd.env(rock_host::project::lib_path_var(), runner_lib_path());
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("spawn: {e}"))?;
    child
        .stdin
        .take()
        .ok_or("no stdin")?
        .write_all(job.as_bytes())
        .map_err(|e| format!("write stdin: {e}"))?;
    let out = child.wait_with_output().map_err(|e| format!("wait: {e}"))?;
    Ok((
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

/// The library search path the runner needs: the prebuilt gamelibs (SDL2) +
/// inherited value + `$NIX_LD_LIBRARY_PATH` (where libGL lives on NixOS, the
/// same path the game's nix-ld'd luajit resolves it through).
fn runner_lib_path() -> String {
    let mut value = rock_host::project::lib_path_with_inherited();
    if let Some(nix_ld) = std::env::var_os("NIX_LD_LIBRARY_PATH") {
        if !nix_ld.is_empty() {
            value.push(':');
            value.push_str(&nix_ld.to_string_lossy());
        }
    }
    value
}

/// Locate the `stone-glsl-runner` binary: `ROCK_GLSL_RUNNER` override, then a
/// sibling of the running `rock` binary (production + release layout), then one
/// level up (cargo test binaries live in `target/<profile>/deps/`).
fn locate_runner() -> Option<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("ROCK_GLSL_RUNNER") {
        let p = std::path::PathBuf::from(p);
        return p.exists().then_some(p);
    }
    let name = if cfg!(windows) { "stone-glsl-runner.exe" } else { "stone-glsl-runner" };
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    for cand in [dir.join(name), dir.parent().map(|d| d.join(name)).unwrap_or_default()] {
        if cand.exists() {
            return Some(cand);
        }
    }
    None
}
