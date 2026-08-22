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

fn run_perf_json_window(game: &str, gc_window: Option<(usize, usize)>) -> Value {
    let rock_bin = env!("CARGO_BIN_EXE_rock");
    // Per-test scratch for --output so concurrent invocations don't race
    // on the shared workspace tmp/main.lua. See game_snapshot_tests for
    // the same pattern.
    let scratch = tempfile::tempdir().expect("create scratch dir for --output");
    let scratch_path = scratch.path().to_string_lossy().into_owned();
    let mut args = vec![
        "perf".to_string(),
        game.to_string(),
        "--seconds".to_string(),
        "1".to_string(),
        "--warmup-seconds".to_string(),
        "0".to_string(),
        "--format".to_string(),
        "json".to_string(),
        "--output".to_string(),
        scratch_path,
    ];
    if let Some((window, start)) = gc_window {
        args.extend([
            "--gc-window".to_string(),
            window.to_string(),
            "--gc-window-start".to_string(),
            start.to_string(),
        ]);
    }
    let output = Command::new(rock_bin)
        .args(&args)
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

fn run_perf_json(game: &str) -> Value {
    run_perf_json_window(game, None)
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

        let gc = s
            .get("gc_window_alloc")
            .unwrap_or_else(|| panic!("sections[{i}].gc_window_alloc missing: {s}"));
        assert!(gc["inclusive_kb_per_frame"].is_null());
        assert!(gc["exclusive_kb_per_frame"].is_null());
        assert!(gc["max_exclusive_kb_per_frame"].is_null());
        assert!(gc["events"].is_null());
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
        mem.get("gross_lua_alloc_request_kb_per_frame")
            .is_some_and(Value::is_null),
        "memory.gross_lua_alloc_request_kb_per_frame must be null without a GC window: {mem}"
    );
    let gc = mem
        .get("gc_window")
        .unwrap_or_else(|| panic!("memory.gc_window missing: {mem}"));
    assert_eq!(gc["available"], Value::Bool(false));
    assert!(gc["gross_request_kb_per_frame"].is_null());
    for key in [
        "positive_growth_kb_per_frame",
        "explicit_free_kb_per_frame",
        "reallocation_shrink_kb_per_frame",
        "allocator_calls",
        "allocation_calls",
        "reallocation_calls",
        "free_calls",
    ] {
        assert!(gc[key].is_null(), "memory.gc_window.{key} must be null: {gc}");
    }
    assert!(gc["sum_exclusive_section_kb_per_frame"].is_null());
    assert!(gc["unsectioned_kb_per_frame"].is_null());
    assert!(gc["accounting_residual_kb_per_frame"].is_null());
    assert!(gc["unknown_sections"].is_null());
    for key in [
        "mean_kb_per_frame",
        "p50_kb_per_frame",
        "p95_kb_per_frame",
        "p99_kb_per_frame",
        "max_kb_per_frame",
    ] {
        assert!(gc["frame_distribution"][key].is_null());
    }
    assert!(gc["worst_frame"]["frame_index"].is_null());
    assert!(gc["worst_frame"]["total_kb"].is_null());
    assert!(gc["worst_frame"]["max_single_event"]["section_id"].is_null());
    assert!(gc["worst_frame"]["max_single_event"]["kb"].is_null());
    assert!(gc["worst_frame"]["sections"].as_array().unwrap().is_empty());
    assert!(gc["text_layout_cache"].is_null());

    // Heap should be non-trivial -- a running match3 holds far more than 1 KB
    // of Lua state. Guards against the helper returning a constant.
    assert!(
        mem["heap_kb"]["max"].as_f64().unwrap() > 1.0,
        "memory.heap_kb.max implausibly small: {mem}"
    );
}

#[test]
fn perf_json_gc_window_has_trustworthy_reconciliation() {
    let doc = run_perf_json_window("games/match3", Some((4, 2)));
    let mem = &doc["memory"];
    let gc = &mem["gc_window"];
    assert_eq!(gc["requested_frames"].as_i64(), Some(4));
    assert_eq!(gc["measured_frames"].as_i64(), Some(4));
    assert_eq!(gc["available"], Value::Bool(true));
    assert_eq!(gc["valid"], Value::Bool(true));
    for key in [
        "gross_request_kb_per_frame",
        "sum_exclusive_section_kb_per_frame",
        "unsectioned_kb_per_frame",
        "accounting_residual_kb_per_frame",
    ] {
        assert!(
            gc[key].as_f64().is_some(),
            "memory.gc_window.{key} must be numeric: {gc}"
        );
    }
    assert!(gc["gross_request_kb_per_frame"].as_f64().unwrap() >= 0.0);
    for key in [
        "positive_growth_kb_per_frame",
        "explicit_free_kb_per_frame",
        "reallocation_shrink_kb_per_frame",
    ] {
        assert!(gc[key].as_f64().unwrap() >= 0.0);
    }
    for key in ["allocator_calls", "allocation_calls", "reallocation_calls", "free_calls"] {
        assert!(gc[key].as_i64().unwrap() >= 0);
    }
    assert!(gc["sum_exclusive_section_kb_per_frame"].as_f64().unwrap() >= 0.0);
    assert!(gc["unsectioned_kb_per_frame"].as_f64().unwrap() >= 0.0);
    assert_eq!(gc["unknown_sections"].as_i64(), Some(0));
    assert!(gc["section_events"].as_i64().unwrap() > 0);
    for key in [
        "mean_kb_per_frame",
        "p50_kb_per_frame",
        "p95_kb_per_frame",
        "p99_kb_per_frame",
        "max_kb_per_frame",
    ] {
        assert!(gc["frame_distribution"][key].as_f64().is_some());
    }
    assert!(gc["worst_frame"]["frame_index"].as_i64().unwrap() >= 0);
    assert!(gc["worst_frame"]["total_kb"].as_f64().unwrap() >= 0.0);
    assert!(gc["worst_frame"]["max_single_event"]["kb"].as_f64().unwrap() >= 0.0);
    assert!(gc["worst_frame"]["sections"].as_array().unwrap().len() > 0);
    assert!(gc["frame_section_slots"].as_i64().unwrap() > 0);
    assert_eq!(gc["frame_section_overflow"].as_i64(), Some(0));
    let cache = &gc["text_layout_cache"];
    assert!(cache["requests"].as_i64().unwrap() > 0);
    assert_eq!(
        cache["hits"].as_i64().unwrap() + cache["misses"].as_i64().unwrap(),
        cache["requests"].as_i64().unwrap()
    );
    assert_eq!(cache["builds"].as_i64(), cache["misses"].as_i64());
    assert!(cache["ui"]["requests"].as_i64().unwrap() > 0);
    assert!(cache["ui"]["hits"].as_i64().unwrap() > 0);
    assert_eq!(
        cache["requests"].as_i64().unwrap(),
        cache["ui"]["requests"].as_i64().unwrap()
            + cache["world"]["requests"].as_i64().unwrap()
    );
    assert_eq!(
        cache["hits"].as_i64().unwrap(),
        cache["ui"]["hits"].as_i64().unwrap()
            + cache["world"]["hits"].as_i64().unwrap()
    );
    assert_eq!(
        cache["misses"].as_i64().unwrap(),
        cache["ui"]["misses"].as_i64().unwrap()
            + cache["world"]["misses"].as_i64().unwrap()
    );

    let sections = doc["sections"].as_array().expect("sections array");
    assert!(
        sections.iter().any(|s| s["gc_window_alloc"]["events"].as_i64().unwrap_or(0) > 0),
        "at least one GC-window section row must expose a non-zero event population"
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
