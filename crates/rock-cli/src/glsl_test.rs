//! Run value tests on the GPU using the external GLSL runner, which owns the GL context.
//! Only the runner's no-context exit can be skipped with --skip-gpu.

use crate::Result;
use crate::test_protocol::{compare_value, parse_skull_markers, reason_histogram};
use crate::test_unit::{UnitCheck, UnitFailure};
use bone_core::prelude::Instant;
use colored::Colorize;
use skull::ValueExpect;
use skull_codegen_glsl::{GlslProfile, ValueTestShaders, emit_value_test};
use skull_mir::mir::FuncRef;

/// Use f32 tolerance for GLSL shader arithmetic and framebuffer readback.
const GLSL_FLOAT_REL_TOL: f64 = 1e-6;

const PROFILE_NAME: &str = "330core";
const PROFILE: GlslProfile = GlslProfile::Core330;

pub(crate) fn glsl_test(paths: &[String], skip_gpu: bool) -> Result<()> {
    let start = Instant::now();
    let (files, package_dirs) = crate::skull_backend::collect_test_units(paths)?;

    let runner = locate_runner()?;
    let gl_available = probe_gl(&runner, PROFILE_NAME, skip_gpu)?;

    let mut outside_value_class = 0;
    let mut unit_failures = 0;
    let mut passed_total = 0;
    // FAIL detail lines (a GL compile/link/render error, or a value mismatch).
    let mut failed_total: Vec<String> = Vec::new();
    let mut skip_reasons: Vec<String> = Vec::new();

    let outcomes = crate::test_unit::run_units(&files, &package_dirs, None, |unit, is_package| {
        build_unit(unit, is_package, &runner, gl_available)
    });
    let file_count = outcomes.len();

    for (label, outcome) in outcomes {
        match outcome {
            Ok(Ok(UnitRun {
                outside_value_class: outside,
                passed,
                failed,
                skips,
            })) => {
                outside_value_class += outside;
                passed_total += passed;
                let any = passed + failed.len() + skips.len();
                if gl_available && any > 0 {
                    let status = if failed.is_empty() {
                        "ok".green()
                    } else {
                        "FAILED".red()
                    };
                    println!(
                        "{:.<52} [{}] {}  ({} pass, {} fail, {} skip)",
                        label,
                        PROFILE_NAME.cyan(),
                        status,
                        passed,
                        failed.len(),
                        skips.len(),
                    );
                }
                failed_total.extend(failed.iter().map(|d| format!("{d} [{PROFILE_NAME}]")));
                skip_reasons.extend(skips.into_iter().map(|(_, reason)| reason));
            }
            Ok(Err(failure)) => {
                unit_failures += 1;
                failure.print(&label);
            }
            Err(panic) => {
                unit_failures += 1;
                crate::test_unit::print_unit_panic(&label, &*panic);
            }
        }
    }

    println!(
        "\nglsl backend: {} @tests outside the value class (no returns=), across {} files ({:.2}s)",
        outside_value_class.to_string().yellow(),
        file_count,
        start.elapsed().as_secs_f64(),
    );
    if !gl_available {
        println!(
            "  [{}] {} -- value verdicts SKIPPED (no GL context)",
            PROFILE_NAME.cyan(),
            "no display / driver".yellow()
        );
    } else {
        let total = passed_total + failed_total.len();
        println!(
            "  [{}] {}/{} value tests passed ({} failed)",
            PROFILE_NAME.cyan(),
            passed_total.to_string().green(),
            total,
            if failed_total.is_empty() {
                failed_total.len().to_string().green()
            } else {
                failed_total.len().to_string().red()
            },
        );
        for detail in &failed_total {
            println!("    {} {}", "FAIL".red(), detail);
        }
        let skip_reasons = reason_histogram(skip_reasons.iter().map(|s| s.as_str()));
        if !skip_reasons.is_empty() {
            println!(
                "    {}",
                format!(
                    "=== [{PROFILE_NAME}] skip reasons (glsl) -- the A1 fn-emission work list ==="
                )
                .bold()
            );
            for (reason, count) in skip_reasons.iter().take(50) {
                println!("      {:>5}  {}", count.to_string().yellow(), reason);
            }
        }
    }
    if unit_failures > 0 {
        println!(
            "  {} unit(s) failed to build (see FAILED lines above)",
            unit_failures.to_string().red()
        );
    }
    if !failed_total.is_empty() || unit_failures > 0 {
        crate::invocation_log::exit(1);
    }
    Ok(())
}

#[derive(Default)]
struct UnitRun {
    outside_value_class: usize,
    passed: usize,
    failed: Vec<String>,
    /// (test name, reason) for value tests whose GLSL emission rejected.
    skips: Vec<(String, String)>,
}

fn build_unit(
    unit: &str,
    is_package: bool,
    runner: &std::path::Path,
    gl_available: bool,
) -> std::result::Result<UnitRun, UnitFailure> {
    // Held through the tests: their shaders are emitted from the checked
    // workspace, which stays resident until the unit ends.
    let _slot = if is_package {
        Some(
            crate::compile_slots::acquire(unit).map_err(|error| UnitFailure {
                reason: "no compile slot".to_string(),
                rendered: format!("{error:#}"),
            })?,
        )
    } else {
        None
    };
    Ok(
        match crate::test_unit::check_unit(unit, is_package, false)? {
            UnitCheck::Checked { ws, tests, .. } => run_tests(&ws, &tests, runner, gl_available),
            // Nothing to emit for a file whose rejection every test expected.
            UnitCheck::ExpectedFailure(_) => UnitRun::default(),
        },
    )
}

fn run_tests(
    ws: &bone_semantic::inference::Workspace,
    tests: &[(FuncRef, skull::SkullTest)],
    runner: &std::path::Path,
    gl_available: bool,
) -> UnitRun {
    let mut run = UnitRun::default();
    // (name, expected, shaders) for the tests that emitted.
    let mut runnable: Vec<(String, ValueExpect, ValueTestShaders)> = Vec::new();
    for (fr, t) in tests {
        let Some(expect) = t
            .returns
            .as_ref()
            .filter(|_| t.runs_on(bone_core::annotations::TestBackend::Glsl))
        else {
            run.outside_value_class += 1;
            continue;
        };
        // An int expectation outside i32 can't survive the R32I readback
        // (GLSL has no int64) -- a real SKIP, part of the work list.
        if let ValueExpect::Int(i) = expect {
            if *i < i32::MIN as i64 || *i > i32::MAX as i64 {
                run.skips.push((
                    t.name.clone(),
                    "value test: returns literal exceeds i32 range (GLSL has no int64)".to_string(),
                ));
                continue;
            }
        }
        match emit_value_test(ws, fr.module, &t.name, PROFILE) {
            Ok(shaders) => runnable.push((t.name.clone(), *expect, shaders)),
            Err(e) => run.skips.push((t.name.clone(), e.message.clone())),
        }
    }

    // No GL context -> the runnable ones are neither pass nor fail.
    if !gl_available || runnable.is_empty() {
        return run;
    }
    match run_batch(runner, PROFILE_NAME, &runnable) {
        Ok(markers) => {
            run.failed.extend(markers.fails);
            run.passed += markers.passed;
            let expects: std::collections::HashMap<&str, &ValueExpect> =
                runnable.iter().map(|(n, e, _)| (n.as_str(), e)).collect();
            for (name, val) in &markers.values {
                let Some(expect) = expects.get(name.as_str()) else {
                    run.failed.push(format!(
                        "harness breach: VALUE for `{name}` without an expectation"
                    ));
                    continue;
                };
                match compare_value(expect, val, GLSL_FLOAT_REL_TOL) {
                    Ok(()) => run.passed += 1,
                    Err(detail) => run.failed.push(format!("{name}: {detail}")),
                }
            }
        }
        Err(detail) => {
            // A runner-level failure (bad emitted GLSL, protocol breach) is a
            // loud FAIL for every runnable test, never a silent skip.
            for (name, _, _) in &runnable {
                run.failed.push(format!("{name}: runner error: {detail}"));
            }
        }
    }
    run
}

/// Spawn the runner for one unit+profile, feed the shaders as a JSON job array,
/// parse the `__SKULL__` lines back.
fn run_batch(
    runner: &std::path::Path,
    profile: &str,
    runnable: &[(String, ValueExpect, ValueTestShaders)],
) -> std::result::Result<crate::test_protocol::SkullMarkers, String> {
    let job = jobs_json(runnable);
    let (code, stdout, stderr) = spawn_runner(runner, profile, &job)?;
    if code == Some(2) {
        return Err(format!("runner reported no GL context: {}", stderr.trim()));
    }
    if code != Some(0) {
        return Err(format!("runner exited {code:?}: {}", stderr.trim()));
    }
    match parse_skull_markers(&stdout) {
        Some(m) if m.passed + m.fails.len() + m.values.len() == runnable.len() => Ok(m),
        _ => Err(format!(
            "invalid runner output (expected {} markers): stdout=<{}> stderr=<{}>",
            runnable.len(),
            stdout.trim(),
            stderr.trim()
        )),
    }
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

pub(crate) fn shader_validate(dirs: &[String], skip_gpu: bool) -> Result<()> {
    if dirs.is_empty() {
        crate::bail!(
            "rock shader-validate: usage: rock shader-validate <project-dir> [project-dir...]"
        );
    }
    let start = Instant::now();
    let runner = locate_runner()?;
    let gl_available = probe_gl(&runner, PROFILE_NAME, skip_gpu)?;
    let mut shader_total = 0;
    let mut total_passed = 0;
    // FAIL detail lines (driver compile/link log, or a harness breach).
    let mut total_failed: Vec<String> = Vec::new();

    for dir in dirs {
        // Held while this project's shaders are emitted and linked from its
        // resident workspace.
        let _slot = crate::compile_slots::acquire(dir)?;
        let ws = validate_workspace(dir)?;
        let pairs = skull_mir::lower::collect_shader_pairs(&ws)
            .map_err(|e| anyhow::anyhow!("{dir}: collect_shader_pairs: {}", e.message))?;
        shader_total += pairs.len();
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

        if !gl_available {
            continue;
        }
        let view = skull::shader_artifact::realize_shader_with_context(
            &ws,
            &graph,
            &skull::shader_artifact::ShaderArtifactRequest {
                spec: skull::target_projection::TargetSpec {
                    shader: Some(skull::target_projection::ShaderTargetSpec::Glsl(PROFILE)),
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
            let emission =
                skull_codegen_glsl::emit_shader_with_view(&ws, &shader_scx, program_view, PROFILE)
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "{dir}: `{}` failed to emit for {PROFILE_NAME}: {}",
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
        let (passed, failed) = compile_batch(&runner, PROFILE_NAME, &jobs)?;
        let status = if failed.is_empty() {
            "ok".green()
        } else {
            "FAILED".red()
        };
        println!(
            "{:.<52} [{}] {}  ({} pass, {} fail)",
            dir,
            PROFILE_NAME.cyan(),
            status,
            passed,
            failed.len(),
        );
        total_passed += passed;
        total_failed.extend(failed);
    }

    println!(
        "\nshader-validate: {} registered pair(s) across {} project(s) ({:.2}s)",
        shader_total,
        dirs.len(),
        start.elapsed().as_secs_f64(),
    );
    if !gl_available {
        println!(
            "  [{}] {} -- verdicts SKIPPED (no GL context)",
            PROFILE_NAME.cyan(),
            "no display / driver".yellow()
        );
        return Ok(());
    }
    println!(
        "  [{}] {}/{} compiled+linked",
        PROFILE_NAME.cyan(),
        total_passed.to_string().green(),
        total_passed + total_failed.len(),
    );
    for detail in &total_failed {
        println!("    {} {}", "FAIL".red(), detail);
    }
    if !total_failed.is_empty() {
        crate::invocation_log::exit(1);
    }
    Ok(())
}

/// Build a project directory into ONE type-checked workspace, tests included
/// (sims may register their own `rock_shader` pairs -- validate those too).
pub(crate) fn validate_workspace(dir: &str) -> Result<bone_semantic::inference::Workspace> {
    let sources = crate::unified_compile::discover_project_sources(std::path::Path::new(dir))
        .map_err(|e| anyhow::anyhow!("{dir}: cannot load project: {e}"))?;
    let (ws, _entry) = crate::skull_backend::check_project(&sources).map_err(|errs| {
        anyhow::anyhow!(
            "{}",
            crate::skull_backend::render_project_errs(&sources, &errs)
        )
    })?;
    Ok(ws)
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
            let _ = super::build_unit(&dir, true, std::path::Path::new("unused-runner"), false);
        });
    }
}
