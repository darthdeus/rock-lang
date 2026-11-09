//! `rock test <path> --backend glsl` -- execute the value-test class
//! (`@test(returns = <literal>)`) on the GPU via the GLSL emitter
//! (`skull-codegen-glsl`).
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
//! The desktop profile (330 core) runs by default, with per-profile verdicts
//! (`name [330core]`). The heavy lifting -- creating a headless
//! GL context, compiling the shader, drawing to a 1x1 R32I/R32F target and
//! reading the pixel back -- lives in the standalone external GLSL runner binary
//! (spawned per unit+profile), so the `rock` compiler/LSP binary never links
//! SDL2. The runner is a workspace artifact, so a missing/broken runner is a
//! hard error (a gate that can't run must not read as green); only a genuine
//! no-display/driver environment (runner exit 2) may be skipped with the explicit
//! `--skip-gpu` option. Without it, every profile requires a display and GL
//! driver. A GL error mid-run is a loud FAIL, never a silent skip.

use crate::Result;
use crate::bone_backend::{categorize, short_path};
use crate::test_protocol::{compare_value, parse_skull_markers, reason_histogram};
use bone_core::prelude::Instant;
use colored::Colorize;
use skull::ValueExpect;
use skull_codegen_glsl::{GlslProfile, ValueTestShaders, emit_value_test};
use skull_mir::mir::FuncRef;

/// GLSL is validated widely enough that f32 arithmetic and the R32F round-trip
/// introduce error well above the 1e-12 luajit/node tolerance -- ~1e-6 is f32
/// territory (24-bit mantissa). int/bool stay exact regardless.
const GLSL_FLOAT_REL_TOL: f64 = 1e-6;

const PROFILES: [(&str, GlslProfile); 1] = [("330core", GlslProfile::Core330)];

/// One profile's tallies over the whole run.
pub(crate) struct GlslProfileSummary {
    /// `"330core"`.
    pub profile: &'static str,
    pub passed: usize,
    /// FAIL detail lines (a GL compile/link/render error, or a value mismatch).
    pub failed: Vec<String>,
    /// Tier-2 SKIP reasons (emission rejected), bucketed count-desc.
    pub skip_reasons: Vec<(String, usize)>,
    /// True once a GL context was created for this profile; false = no display
    /// / driver -> explicitly skipped GPU verdicts.
    pub gl_available: bool,
}

/// Whole-run tallies -- the testable core consumed by the ratchet test.
pub(crate) struct GlslSuiteSummary {
    pub files: usize,
    /// Tier-1: `@test` fns with no `returns=` (outside the value class). One
    /// collapsed number, never a histogram.
    pub outside_value_class: usize,
    /// bone-rejected units (unexpected compile failure) / harness breaches.
    pub unit_failures: usize,
    pub per_profile: Vec<GlslProfileSummary>,
}

/// `rock test --backend glsl` entry: run, print per-profile lines + the
/// end-of-run summary, exit non-zero on any FAIL or unit failure.
pub(crate) fn glsl_test(paths: &[String], skip_gpu: bool) -> Result<()> {
    let start = Instant::now();
    let summary = glsl_test_run(paths, true, skip_gpu)?;

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
                format!(
                    "=== [{}] skip reasons (glsl) -- the A1 fn-emission work list ===",
                    p.profile
                )
                .bold()
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
        crate::invocation_log::exit(1);
    }
    Ok(())
}

/// Run the suite; optionally print per-unit lines as they complete. Public for
/// the ratchet test (`print = false`).
pub(crate) fn glsl_test_run(
    paths: &[String],
    print: bool,
    skip_gpu: bool,
) -> Result<GlslSuiteSummary> {
    let (files, package_dirs) = crate::skull_backend::collect_test_units(paths)?;

    // A missing runner is a hard error (workspace artifact -- build it). Probe
    // once per profile: exit 2 requires an explicit GPU opt-out; any other
    // probe anomaly is a hard error.
    let runner = locate_runner()?;
    let mut gl_available: [bool; PROFILES.len()] = [false; PROFILES.len()];
    for (i, (pname, _)) in PROFILES.iter().enumerate() {
        gl_available[i] = probe_gl(&runner, pname, skip_gpu)?;
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
    let mut skip_reasons: [Vec<String>; PROFILES.len()] = [Vec::new()];

    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let mut outcomes: Vec<(String, std::thread::Result<UnitOutcome>)> = Vec::new();
    for file in &files {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            build_file(file, &runner, &gl_available)
        }));
        outcomes.push((short_path(file), outcome));
    }
    for dir in &package_dirs {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            build_package(dir, &runner, &gl_available)
        }));
        outcomes.push((short_path(dir), outcome));
    }
    std::panic::set_hook(prev_hook);

    for (label, outcome) in outcomes {
        summary.files += 1;
        match outcome {
            Ok(UnitOutcome::Ran {
                outside_value_class,
                profiles,
            }) => {
                summary.outside_value_class += outside_value_class;
                for (i, pr) in profiles.into_iter().enumerate() {
                    summary.per_profile[i].passed += pr.passed;
                    let any = pr.passed + pr.failed.len() + pr.skips.len();
                    summary.per_profile[i]
                        .failed
                        .extend(pr.failed.iter().map(|d| format!("{d} [{}]", PROFILES[i].0)));
                    for (_, reason) in &pr.skips {
                        skip_reasons[i].push(reason.clone());
                    }
                    if print && gl_available[i] && any > 0 {
                        let status = if pr.failed.is_empty() {
                            "ok".green()
                        } else {
                            "FAILED".red()
                        };
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
        summary.per_profile[i].skip_reasons = reason_histogram(reasons.iter().map(|s| s.as_str()));
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
/// emit + run the value tests under the desktop profile.
fn build_file(
    file: &str,
    runner: &std::path::Path,
    gl_available: &[bool; PROFILES.len()],
) -> UnitOutcome {
    let src = match fs_err::read_to_string(file) {
        Ok(s) => s,
        Err(e) => {
            return UnitOutcome::UnitFail {
                reason: e.to_string(),
                rendered: e.to_string(),
            };
        }
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
    let tests: Vec<(FuncRef, skull::SkullTest)> = match skull::module_tests_with_ids(&ws, user_mid)
    {
        Ok(tests) => tests
            .into_iter()
            .map(|(fid, t)| {
                (
                    FuncRef {
                        module: user_mid,
                        func: fid,
                    },
                    t,
                )
            })
            .collect(),
        Err(e) => {
            return UnitOutcome::UnitFail {
                reason: categorize(&e.message),
                rendered: crate::skull_backend::render_file_errs(file, &src, &[e]),
            };
        }
    };
    run_tests(&ws, &tests, runner, gl_available)
}

/// A `stick.toml` package built as ONE workspace (tests included), mirroring
/// the JS/luajit package path.
fn build_package(
    dir: &str,
    runner: &std::path::Path,
    gl_available: &[bool; PROFILES.len()],
) -> UnitOutcome {
    // Held through the tests: their shaders are emitted from the checked
    // workspace, which stays resident until the unit ends.
    let _slot = match crate::compile_slots::acquire(dir) {
        Ok(slot) => slot,
        Err(error) => {
            return UnitOutcome::UnitFail {
                reason: "no compile slot".to_string(),
                rendered: format!("{error:#}"),
            };
        }
    };
    let spec = crate::unified_compile::SourceSpec::Directory(std::path::PathBuf::from(dir));
    let sources = match crate::unified_compile::discover_sources(&spec) {
        Ok(s) => s,
        Err(e) => {
            return UnitOutcome::UnitFail {
                reason: "cannot load project".to_string(),
                rendered: e.to_string(),
            };
        }
    };
    let (mut modules, _entry) = match crate::skull_backend::skull_assemble_project(&sources) {
        Ok(v) => v,
        Err(errs) => {
            return UnitOutcome::UnitFail {
                reason: categorize(&errs[0].message),
                rendered: crate::skull_backend::render_project_errs(&sources, &errs),
            };
        }
    };
    let ws = match crate::bone_backend::check_modules(&mut modules) {
        Ok(ws) => ws,
        Err(errs) => {
            return UnitOutcome::UnitFail {
                reason: categorize(&errs[0].message),
                rendered: crate::skull_backend::render_project_errs(&sources, &errs),
            };
        }
    };
    drop(modules);
    let tests = match crate::skull_backend::collect_package_test_fns(&ws, dir) {
        Ok(tests) => tests,
        Err(e) => {
            return UnitOutcome::UnitFail {
                reason: categorize(&e.message),
                rendered: e.message,
            };
        }
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
                .map(|_| ProfileRun {
                    passed: 0,
                    failed: Vec::new(),
                    skips: Vec::new(),
                })
                .collect(),
        };
    }
    UnitOutcome::UnitFail {
        reason: categorize(&errs[0].message),
        rendered: crate::skull_backend::render_file_errs(file, src, errs),
    }
}

/// Emit + GPU-run one unit's value tests under the desktop profile.
fn run_tests(
    ws: &bone_semantic::inference::Workspace,
    tests: &[(FuncRef, skull::SkullTest)],
    runner: &std::path::Path,
    gl_available: &[bool; PROFILES.len()],
) -> UnitOutcome {
    let mut outside_value_class = 0usize;
    // Value tests only (returns=). Non-value @tests are permanently outside the
    // class -- one collapsed count, never a histogram. So are tests that
    // `@test(backends = ...)` limits to other backends.
    let value_tests: Vec<(FuncRef, &skull::SkullTest, &ValueExpect)> = tests
        .iter()
        .filter_map(|(fr, t)| match &t.returns {
            Some(expect) if t.runs_on(bone_core::annotations::TestBackend::Glsl) => {
                Some((*fr, t, expect))
            }
            _ => {
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
                        "value test: returns literal exceeds i32 range (GLSL has no int64)"
                            .to_string(),
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
            match run_batch(runner, pname, &runnable) {
                Ok(markers) => {
                    failed.extend(markers.fails);
                    passed += markers.passed;
                    let expects: std::collections::HashMap<&str, &ValueExpect> =
                        runnable.iter().map(|(n, e, _)| (n.as_str(), e)).collect();
                    for (name, val) in &markers.values {
                        let Some(expect) = expects.get(name.as_str()) else {
                            failed.push(format!(
                                "harness breach: VALUE for `{name}` without an expectation"
                            ));
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
        profiles.push(ProfileRun {
            passed,
            failed,
            skips,
        });
    }

    UnitOutcome::Ran {
        outside_value_class,
        profiles,
    }
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
            ));
        }
    };
    Ok(RunnerMarkers {
        passed: markers.passed,
        fails: markers.fails,
        values: markers.values,
    })
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
/// Only an explicit GPU opt-out permits skipping exit 2. A spawn failure or
/// any other exit code remains a hard error.
fn probe_gl(runner: &std::path::Path, profile: &str, skip_gpu: bool) -> Result<bool> {
    match spawn_runner(runner, profile, "[]") {
        Ok((Some(0), _, _)) => Ok(true),
        Ok((Some(2), _, stderr)) => {
            if !skip_gpu {
                crate::bail!(
                    "no display or GL driver for profile {profile}: {}. Install a display/GL driver or pass --skip-gpu to skip unavailable GPU profiles",
                    stderr.trim()
                );
            }
            log::warn!(
                target: "GLSL",
                "no GL context for profile {profile}: {}; verdicts skipped (--skip-gpu)",
                stderr.trim()
            );
            Ok(false)
        }
        Ok((code, _, stderr)) => crate::bail!(
            "stone-glsl-runner probe for profile {profile} exited {code:?} (expected 0, or 2 for no GL context): {}",
            stderr.trim()
        ),
        Err(e) => crate::bail!("cannot spawn stone-glsl-runner ({}): {e}", runner.display()),
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
    rock_host::compiler_thp::configure_runtime_command(&mut cmd);
    cmd.arg("--profile").arg(profile);
    cmd = crate::runtime_process::configure(cmd, crate::runtime_process::RuntimeProcess::Native);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
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

// --- Compile-validate registered render shaders on real GL contexts ---
// The value-test corpus above proves the EMITTER on desktop GL; this proves
// the shaders games actually register.

/// One profile's tallies over a `shader-validate` run.
pub(crate) struct ValidateProfileSummary {
    /// `"330core"`.
    pub profile: &'static str,
    pub passed: usize,
    /// FAIL detail lines (driver compile/link log, or a harness breach).
    pub failed: Vec<String>,
    /// False = no GL context for this profile (explicitly skipped).
    pub gl_available: bool,
}

/// Whole-run tallies for `rock shader-validate` -- the testable core.
pub(crate) struct ValidateSummary {
    /// (project dir, registered shader-pair count).
    pub projects: Vec<(String, usize)>,
    pub per_profile: Vec<ValidateProfileSummary>,
}

/// `rock shader-validate <project>...`: print verdicts, exit non-zero on any
/// FAIL; missing GL profiles require the explicit GPU opt-out.
pub(crate) fn shader_validate(dirs: &[String], skip_gpu: bool) -> Result<()> {
    if dirs.is_empty() {
        crate::bail!(
            "rock shader-validate: usage: rock shader-validate <project-dir> [project-dir...]"
        );
    }
    let start = Instant::now();
    let summary = shader_validate_run(dirs, true, skip_gpu)?;

    let shader_total: usize = summary.projects.iter().map(|(_, n)| n).sum();
    println!(
        "\nshader-validate: {} registered pair(s) across {} project(s) ({:.2}s)",
        shader_total,
        summary.projects.len(),
        start.elapsed().as_secs_f64(),
    );
    let mut any_failed = false;
    for p in &summary.per_profile {
        if !p.gl_available {
            println!(
                "  [{}] {} -- verdicts SKIPPED (no GL context)",
                p.profile.cyan(),
                "no display / driver".yellow()
            );
            continue;
        }
        println!(
            "  [{}] {}/{} compiled+linked",
            p.profile.cyan(),
            p.passed.to_string().green(),
            p.passed + p.failed.len(),
        );
        for detail in &p.failed {
            any_failed = true;
            println!("    {} {}", "FAIL".red(), detail);
        }
    }
    if any_failed {
        crate::invocation_log::exit(1);
    }
    Ok(())
}

/// Run the validation; public for the gate test (`print = false`). Hard errors
/// (missing runner, a project that fails to build, a pair that fails to EMIT)
/// are `Err` -- the same pairs emit inside every game build, so an emit failure
/// here is a regression or a profile-specific emitter gap, never a skip.
pub(crate) fn shader_validate_run(
    dirs: &[String],
    print: bool,
    skip_gpu: bool,
) -> Result<ValidateSummary> {
    let runner = locate_runner()?;
    let mut gl_available: [bool; PROFILES.len()] = [false; PROFILES.len()];
    for (i, (pname, _)) in PROFILES.iter().enumerate() {
        gl_available[i] = probe_gl(&runner, pname, skip_gpu)?;
    }

    let mut summary = ValidateSummary {
        projects: Vec::new(),
        per_profile: PROFILES
            .iter()
            .enumerate()
            .map(|(i, (name, _))| ValidateProfileSummary {
                profile: name,
                passed: 0,
                failed: Vec::new(),
                gl_available: gl_available[i],
            })
            .collect(),
    };

    for dir in dirs {
        // Held while this project's shaders are emitted and linked from its
        // resident workspace.
        let _slot = crate::compile_slots::acquire(dir)?;
        let ws = validate_workspace(dir)?;
        let pairs = skull_mir::lower::collect_shader_pairs(&ws)
            .map_err(|e| anyhow::anyhow!("{dir}: collect_shader_pairs: {}", e.message))?;
        summary.projects.push((dir.clone(), pairs.len()));
        let programs: Vec<skull::shader_artifact::ShaderProgram> = pairs
            .iter()
            .map(|pair| skull::shader_artifact::ShaderProgram {
                vert: pair.vert,
                frag: pair.frag,
            })
            .collect();
        let roots: Vec<FuncRef> = programs
            .iter()
            .flat_map(|program| {
                program
                    .vert
                    .into_iter()
                    .chain(std::iter::once(program.frag))
            })
            .collect();
        let graph = skull::instance_graph::build(&ws, roots, false);
        let shader_scx = skull_mir::lower::ScalarCtx::new_values_first_class(&ws)
            .sharing(std::sync::Arc::clone(graph.interner()));

        for (pi, (pname, profile)) in PROFILES.iter().enumerate() {
            if !gl_available[pi] {
                continue;
            }
            let view = skull::shader_artifact::realize_shader_with_context(
                &ws,
                &graph,
                &skull::shader_artifact::ShaderArtifactRequest {
                    spec: skull::target_projection::TargetSpec {
                        shader: Some(skull::target_projection::ShaderTargetSpec::Glsl(*profile)),
                        ..skull::target_projection::LUAJIT_DESKTOP
                    },
                    programs: &programs,
                },
                &shader_scx,
            )
            .ok_or_else(|| anyhow::anyhow!("{dir}: GLSL shader view is unavailable"))?;
            if view.program_views.len() != pairs.len() {
                anyhow::bail!("{dir}: GLSL shader view/collection denominator differs");
            }
            let mut jobs: Vec<(String, String, String)> = Vec::with_capacity(pairs.len());
            for (pair, program_view) in pairs.iter().zip(&view.program_views) {
                let emission = match pair.vert {
                    Some(_) => skull_codegen_glsl::emit_shader_pipeline_with_view(
                        &ws,
                        &shader_scx,
                        program_view,
                        *profile,
                    ),
                    None => skull_codegen_glsl::emit_shader_entry_with_view(
                        &ws,
                        &shader_scx,
                        program_view,
                        *profile,
                    ),
                }
                .map_err(|e| {
                    anyhow::anyhow!(
                        "{dir}: `{}` failed to emit for {pname}: {}",
                        pair.key,
                        e.message
                    )
                })?;
                let (vertex, fragment) = split_stages(&emission.source);
                jobs.push((format!("{dir}:{}", pair.key), vertex, fragment));
            }
            if jobs.is_empty() {
                continue;
            }
            let (passed, failed) = compile_batch(&runner, pname, &jobs)?;
            if print {
                let status = if failed.is_empty() {
                    "ok".green()
                } else {
                    "FAILED".red()
                };
                println!(
                    "{:.<52} [{}] {}  ({} pass, {} fail)",
                    dir,
                    pname.cyan(),
                    status,
                    passed,
                    failed.len(),
                );
            }
            summary.per_profile[pi].passed += passed;
            summary.per_profile[pi].failed.extend(failed);
        }
    }
    Ok(summary)
}

/// Build a project directory into ONE type-checked workspace, tests included
/// (sims may register their own `rock_shader` pairs -- validate those too).
pub(crate) fn validate_workspace(dir: &str) -> Result<bone_semantic::inference::Workspace> {
    let spec = crate::unified_compile::SourceSpec::Directory(std::path::PathBuf::from(dir));
    let sources = crate::unified_compile::discover_sources(&spec)
        .map_err(|e| anyhow::anyhow!("{dir}: cannot load project: {e}"))?;
    validate_sources(sources)
}

fn validate_sources(
    sources: rock_core::SourceFiles,
) -> Result<bone_semantic::inference::Workspace> {
    let (mut modules, _entry) =
        crate::skull_backend::skull_assemble_project(&sources).map_err(|errs| {
            anyhow::anyhow!(
                "{}",
                crate::skull_backend::render_project_errs(&sources, &errs)
            )
        })?;
    crate::bone_backend::check_modules(&mut modules).map_err(|errs| {
        anyhow::anyhow!(
            "{}",
            crate::skull_backend::render_project_errs(&sources, &errs)
        )
    })
}

/// Split a single-file `#ifdef VERTEX` / `#ifdef PIXEL` shader into the two
/// stage sources EXACTLY the way the engine loader does
/// (`engine-rs/src/shader.rs`): inject `#define VERTEX` / `#define PIXEL` on the
/// line after `#version`. Validating anything else would validate a shader the
/// engine never compiles.
fn split_stages(combined: &str) -> (String, String) {
    let insert_point = combined
        .find("#version")
        .and_then(|vp| combined[vp..].find('\n').map(|p| vp + p + 1))
        .unwrap_or(0);
    let (prefix, suffix) = combined.split_at(insert_point);
    (
        format!("{prefix}#define VERTEX\n{suffix}"),
        format!("{prefix}#define PIXEL\n{suffix}"),
    )
}

/// One runner invocation with compile-only jobs; returns (passed, FAIL details).
/// Marker-count and `compiled` payloads are verified -- a protocol drift is a
/// loud error, never a silent pass.
fn compile_batch(
    runner: &std::path::Path,
    profile: &str,
    jobs: &[(String, String, String)],
) -> Result<(usize, Vec<String>)> {
    let items: Vec<serde_json::Value> = jobs
        .iter()
        .map(|(name, vertex, fragment)| {
            serde_json::json!({ "name": name, "vertex": vertex, "fragment": fragment, "ret": "compile" })
        })
        .collect();
    let payload = serde_json::Value::Array(items).to_string();
    let (code, stdout, stderr) = spawn_runner(runner, profile, &payload)
        .map_err(|e| anyhow::anyhow!("runner [{profile}]: {e}"))?;
    if code != Some(0) {
        crate::bail!(
            "runner [{profile}] exited {code:?} mid-run: {}",
            stderr.trim()
        );
    }
    let markers = parse_skull_markers(&stdout)
        .filter(|m| m.passed + m.fails.len() + m.values.len() == jobs.len())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "invalid runner output (expected {} markers): stdout=<{}> stderr=<{}>",
                jobs.len(),
                stdout.trim(),
                stderr.trim()
            )
        })?;
    let mut passed = 0usize;
    let mut failed: Vec<String> = markers
        .fails
        .iter()
        .map(|d| format!("{d} [{profile}]"))
        .collect();
    for (name, val) in &markers.values {
        if val == "compiled" {
            passed += 1;
        } else {
            failed.push(format!(
                "harness breach: `{name}` returned `{val}`, expected `compiled` [{profile}]"
            ));
        }
    }
    Ok((passed, failed))
}

/// Locate the external GLSL runner binary: `ROCK_GLSL_RUNNER` override, then a
/// sibling of the running `rock` binary (production + release layout), then one
/// level up (cargo test binaries live in `target/<profile>/deps/`). Missing is
/// a hard error: the runner is a workspace artifact, and a gate whose executor
/// is absent must not read as green.
fn locate_runner() -> Result<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("ROCK_GLSL_RUNNER") {
        let p = std::path::PathBuf::from(p);
        if !p.exists() {
            crate::bail!(
                "ROCK_GLSL_RUNNER points at a missing stone-glsl-runner: {}",
                p.display()
            );
        }
        return Ok(p);
    }
    let name = if cfg!(windows) {
        "stone-glsl-runner.exe"
    } else {
        "stone-glsl-runner"
    };
    let exe = std::env::current_exe().map_err(|e| anyhow::anyhow!("current_exe: {e}"))?;
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("exe has no parent dir"))?;
    for cand in [
        dir.join(name),
        dir.parent().map(|d| d.join(name)).unwrap_or_default(),
    ] {
        if cand.exists() {
            return Ok(cand);
        }
    }
    crate::bail!(
        "stone-glsl-runner not found next to {} -- build it (`cargo build -p stone-glsl-runner`) or set ROCK_GLSL_RUNNER",
        dir.display()
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn package_tests_wait_for_a_compile_slot() {
        let package = tempfile::tempdir().expect("package");
        std::fs::write(
            package.path().join("stick.toml"),
            "[package]\nname = \"slot-probe\"\ntype = \"lib\"\n",
        )
        .unwrap();
        std::fs::write(
            package.path().join("probe.rock"),
            "@test fn probe() -> Int = 1\n",
        )
        .unwrap();
        let dir = package.path().to_str().unwrap().to_owned();
        // No GL profile is available, so no runner is spawned.
        crate::compile_slots::assert_compile_waits_for_slot(move || {
            super::build_package(
                &dir,
                std::path::Path::new("unused-runner"),
                &[false; super::PROFILES.len()],
            );
        });
    }
}
