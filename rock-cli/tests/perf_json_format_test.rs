//! Lock the `rock perf --format json` schema so silent regressions can't
//! break downstream consumers (scripts, agents, dashboards). Runs the full
//! perf pipeline against a small game for a short duration and validates
//! the shape of the emitted JSON document.
//!
//! Schema: full uniform nest (docs/effective-fps-and-stats-plan.md). Every
//! series -- each `frame_ms` timing variant, each section's duration + alloc,
//! each memory series -- serializes as the SAME block:
//! `{mean, stddev, min, max, p50, p90, p99, n}`. There are no legacy flat
//! `frame_ms.{avg,p50,p99,max}` keys.

use std::path::Path;
use std::process::Command;

use serde_json::Value;

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
}

fn run_perf_json(game: &str) -> Value {
    let rock_bin = env!("CARGO_BIN_EXE_rock");
    // Per-test scratch for --output so concurrent invocations don't race
    // on the shared workspace tmp/main.lua. See game_snapshot_tests for
    // the same pattern.
    let scratch = tempfile::tempdir().expect("create scratch dir for --output");
    let scratch_path = scratch.path().to_string_lossy().into_owned();
    let output = Command::new(rock_bin)
        .args([
            "perf",
            game,
            "--seconds",
            "1",
            "--warmup-seconds",
            "0",
            "--format",
            "json",
            "--output",
            &scratch_path,
        ])
        .current_dir(workspace_root())
        .output()
        .expect("failed to spawn rock perf");

    assert!(
        output.status.success(),
        "rock perf {game} exited with {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('{'))
        .unwrap_or_else(|| {
            panic!(
                "no JSON document found in rock perf stdout\nstdout: {stdout}\nstderr: {}",
                String::from_utf8_lossy(&output.stderr)
            )
        });

    serde_json::from_str(line)
        .unwrap_or_else(|e| panic!("perf summary was not valid JSON: {e}\nline: {line}"))
}

/// Validate one uniform stat block: every field present + numeric, the
/// percentile/min/max ordering coherent, and `n` a non-negative int. With
/// `require_samples` the block must carry at least one sample (`n >= 1`) and a
/// non-trivial ordering; otherwise an empty block (`n == 0`, all zeros) is
/// accepted -- the case for an unsupported `gpu` timer on this platform.
fn assert_stat_block(block: &Value, ctx: &str, require_samples: bool) {
    for key in ["mean", "stddev", "min", "max", "p50", "p90", "p99"] {
        let v = block
            .get(key)
            .and_then(Value::as_f64)
            .unwrap_or_else(|| panic!("{ctx}.{key} missing or not a number: {block}"));
        assert!(v >= 0.0, "{ctx}.{key} must be non-negative, got {v}");
    }

    let n = block
        .get("n")
        .and_then(Value::as_i64)
        .unwrap_or_else(|| panic!("{ctx}.n missing or not an int: {block}"));
    assert!(n >= 0, "{ctx}.n must be non-negative, got {n}");

    if require_samples {
        assert!(n >= 1, "{ctx}.n must be >= 1 (block should carry samples): {block}");
    }
    if n == 0 {
        // Empty block: nothing more to order. (gpu when the GL timer is
        // unsupported feeds no samples -- never a fake 0 measurement.)
        return;
    }

    let f = |k: &str| block[k].as_f64().unwrap();
    let (min, p50, p90, p99, max, mean, stddev) = (
        f("min"),
        f("p50"),
        f("p90"),
        f("p99"),
        f("max"),
        f("mean"),
        f("stddev"),
    );
    assert!(min <= p50, "{ctx}: min ({min}) must be <= p50 ({p50})");
    assert!(p50 <= p90, "{ctx}: p50 ({p50}) must be <= p90 ({p90})");
    assert!(p90 <= p99, "{ctx}: p90 ({p90}) must be <= p99 ({p99})");
    assert!(p99 <= max, "{ctx}: p99 ({p99}) must be <= max ({max})");
    assert!(min <= mean && mean <= max, "{ctx}: mean ({mean}) must be in [min ({min}), max ({max})]");
    assert!(stddev >= 0.0, "{ctx}: stddev ({stddev}) must be non-negative");
}

#[test]
fn perf_json_has_top_level_shape() {
    let doc = run_perf_json("games/match3");

    for key in ["frames", "warmup_s", "frame_ms", "sections", "graphics", "memory"] {
        assert!(
            doc.get(key).is_some(),
            "missing top-level key `{key}` in perf JSON: {doc}"
        );
    }

    assert!(
        doc["frames"].as_i64().expect("frames must be int") >= 1,
        "expected at least 1 frame, got {}",
        doc["frames"]
    );
    assert!(
        doc["warmup_s"].as_f64().is_some(),
        "warmup_s must be a number, got {}",
        doc["warmup_s"]
    );
}

#[test]
fn perf_json_frame_ms_is_uniform_series() {
    let doc = run_perf_json("games/match3");
    let frame_ms = &doc["frame_ms"];
    assert!(
        frame_ms.is_object(),
        "frame_ms must be an object of named series, got: {frame_ms}"
    );

    // The six frame-timing series. All but `gpu` are pure CPU-side timing and
    // must carry samples. `gpu` rides a GL timer query that is unsupported on
    // some platforms (macOS, headless) -- it may legitimately have n == 0.
    for series in ["logic_cpu", "render_cpu", "effective_cpu", "present", "rendered"] {
        let block = frame_ms
            .get(series)
            .unwrap_or_else(|| panic!("frame_ms.{series} missing: {frame_ms}"));
        assert_stat_block(block, &format!("frame_ms.{series}"), true);
    }
    let gpu = frame_ms
        .get("gpu")
        .unwrap_or_else(|| panic!("frame_ms.gpu missing: {frame_ms}"));
    assert_stat_block(gpu, "frame_ms.gpu", false);

    // No legacy flat keys leak through.
    for legacy in ["avg", "p50", "p99", "max"] {
        assert!(
            frame_ms.get(legacy).is_none(),
            "frame_ms must not carry legacy flat key `{legacy}`: {frame_ms}"
        );
    }
}

#[test]
fn perf_json_sections_have_uniform_blocks() {
    let doc = run_perf_json("games/match3");
    let sections = doc["sections"]
        .as_array()
        .expect("sections must be an array");

    assert!(!sections.is_empty(), "expected at least one section entry");

    for (i, s) in sections.iter().enumerate() {
        assert!(s["name"].is_string(), "sections[{i}].name must be string: {s}");
        assert!(
            s["depth"].as_i64().map_or(false, |d| d >= 0),
            "sections[{i}].depth must be non-negative int: {s}"
        );
        let share = s
            .get("share")
            .and_then(Value::as_f64)
            .unwrap_or_else(|| panic!("sections[{i}].share missing or not a number: {s}"));
        assert!(share >= 0.0, "sections[{i}].share must be non-negative, got {share}");

        // Section duration (ms) and net alloc (KB) are each a uniform block.
        let ms = s
            .get("ms")
            .unwrap_or_else(|| panic!("sections[{i}].ms block missing: {s}"));
        assert_stat_block(ms, &format!("sections[{i}].ms"), true);

        let alloc = s
            .get("alloc_kb")
            .unwrap_or_else(|| panic!("sections[{i}].alloc_kb block missing: {s}"));
        assert_stat_block(alloc, &format!("sections[{i}].alloc_kb"), true);
    }
}

#[test]
fn perf_json_memory_has_uniform_blocks() {
    let doc = run_perf_json("games/match3");
    let mem = &doc["memory"];

    for group in ["heap_kb", "alloc_kb_per_frame"] {
        let block = mem
            .get(group)
            .unwrap_or_else(|| panic!("memory.{group} block missing: {mem}"));
        assert_stat_block(block, &format!("memory.{group}"), true);
    }

    assert!(
        mem.get("gc_events")
            .and_then(Value::as_i64)
            .map_or(false, |n| n >= 0),
        "memory.gc_events missing or negative: {mem}"
    );
    assert!(
        mem.get("true_alloc_kb_per_frame").and_then(Value::as_f64).is_some(),
        "memory.true_alloc_kb_per_frame missing or not a number: {mem}"
    );

    // Heap should be non-trivial -- a running match3 holds far more than 1 KB
    // of Lua state. Guards against the helper returning a constant.
    assert!(
        mem["heap_kb"]["max"].as_f64().unwrap() > 1.0,
        "memory.heap_kb.max implausibly small: {mem}"
    );
}

#[test]
fn perf_json_graphics_has_counters() {
    let doc = run_perf_json("games/match3");
    let g = &doc["graphics"];
    for key in ["draw_calls", "text_draws"] {
        assert!(
            g.get(key).and_then(Value::as_i64).is_some(),
            "graphics.{key} missing or not an int: {g}"
        );
    }
}
