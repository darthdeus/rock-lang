//! Lock the `rock perf --format json` schema so silent regressions can't
//! break downstream consumers (scripts, agents, dashboards). Runs the full
//! perf pipeline against a small game for a short duration and validates
//! the shape of the emitted JSON document.
//!
//! Schema: full uniform nest. Every
//! series -- each `frame_ms` timing variant, each section's duration + alloc,
//! each memory series -- serializes as the SAME block:
//! `{mean, stddev, min, max, p50, p90, p99, n}`. There are no legacy flat
//! `frame_ms.{avg,p50,p99,max}` keys.

use std::path::Path;
use crate::stone_harness::{HeavyCommandExt, SpawnBudget};

use serde_json::Value;

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
}

fn run_perf_json_window_with_sections(
    game: &str,
    gc_window: Option<(usize, usize)>,
    detailed_sections: bool,
    gc_window_mode: Option<&str>,
) -> Value {
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
    if let Some(mode) = gc_window_mode {
        args.extend(["--gc-window-mode".to_string(), mode.to_string()]);
    }
    let mut command = crate::command::rock(rock_bin);
    command.args(&args).current_dir(workspace_root());
    if detailed_sections {
        command.env("PERF_NO_SECTIONS", "0");
    }
    let output = command.output_with_deadline().expect("failed to spawn rock perf");

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

fn run_perf_json_window(game: &str, gc_window: Option<(usize, usize)>) -> Value {
    run_perf_json_window_with_sections(game, gc_window, false, None)
}

fn run_perf_json(game: &str) -> Value {
    run_perf_json_window(game, None)
}

fn run_perf_json_with_sections(game: &str) -> Value {
    run_perf_json_window_with_sections(game, None, true, None)
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
fn perf_json_default_shape_memory_and_graphics() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let doc = run_perf_json("games/match3");
    perf_json_has_top_level_shape(&doc);
    perf_json_memory_has_uniform_blocks(&doc);
    perf_json_graphics_has_counters(&doc);
}

fn perf_json_has_top_level_shape(doc: &Value) {
    for key in [
        "run", "frames", "warmup_s", "frame_ms", "sections", "graphics", "memory", "resources",
    ] {
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

    assert_eq!(
        doc["run"]["sections"],
        Value::String("off".to_string()),
        "rock perf defaults must record detailed sections as off: {}",
        doc["run"]
    );

    let images = &doc["resources"]["images"];
    for field in [
        "load_calls", "load_failures", "decoded_bytes", "load_us",
        "upload_calls", "upload_bytes", "wait_us", "upload_us",
        "active_frames", "max_wait_us",
    ] {
        assert!(
            images[field].as_i64().is_some(),
            "resources.images.{field} must be an integer: {images}"
        );
    }

    let jit_trace = &doc["run"]["jit_trace"];
    let available = jit_trace["available"]
        .as_bool()
        .unwrap_or_else(|| panic!("run.jit_trace.available must be bool: {jit_trace}"));
    assert!(
        jit_trace["logging"].as_bool().is_some(),
        "run.jit_trace.logging must be bool: {jit_trace}"
    );
    if available {
        assert!(
            jit_trace["starts"].as_i64().is_some() && jit_trace["aborts"].as_i64().is_some(),
            "available JIT trace populations must be integers: {jit_trace}"
        );
    } else {
        assert!(
            jit_trace["starts"].is_null() && jit_trace["aborts"].is_null(),
            "unavailable JIT trace populations must be null: {jit_trace}"
        );
    }
}

#[test]
fn perf_json_frame_ms_is_uniform_series() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let doc = run_perf_json_window("games/match3", Some((120, 100)));
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
        assert_eq!(block["n"], doc["frames"], "{series} must cover every measured frame");
    }
    let trailing = &doc["frame_ms_trailing"];
    for series in ["logic_cpu", "render_cpu", "effective_cpu", "present", "rendered"] {
        assert_stat_block(&trailing[series], &format!("frame_ms_trailing.{series}"), true);
        let expected = doc["frames"].as_u64().unwrap().min(120);
        assert_eq!(trailing[series]["n"].as_u64(), Some(expected));
    }
    assert_stat_block(&trailing["gpu"], "frame_ms_trailing.gpu", false);

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
    let _spawn_budget = SpawnBudget::for_test(120);
    let doc = run_perf_json_with_sections("games/match3");
    assert_eq!(doc["run"]["sections"], Value::String("on".to_string()));
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

        // Section duration (ms) and exclusive request traffic (KB/frame) are
        // each a uniform block.
        let ms = s
            .get("ms")
            .unwrap_or_else(|| panic!("sections[{i}].ms block missing: {s}"));
        assert_stat_block(ms, &format!("sections[{i}].ms"), true);

        let request = s
            .get("request_kb")
            .unwrap_or_else(|| panic!("sections[{i}].request_kb block missing: {s}"));
        assert_stat_block(request, &format!("sections[{i}].request_kb"), false);
        assert!(s.get("alloc_kb").is_none(), "old allocation proxy remains: {s}");

        let gc = s
            .get("gc_window_alloc")
            .unwrap_or_else(|| panic!("sections[{i}].gc_window_alloc missing: {s}"));
        assert!(gc["inclusive_kb_per_frame"].is_null());
        assert!(gc["exclusive_kb_per_frame"].is_null());
        assert!(gc["max_exclusive_kb_per_frame"].is_null());
        assert!(gc["events"].is_null());
    }
}

fn perf_json_memory_has_uniform_blocks(doc: &Value) {
    let mem = &doc["memory"];

    for group in ["heap_kb"] {
        let block = mem
            .get(group)
            .unwrap_or_else(|| panic!("memory.{group} block missing: {mem}"));
        assert_stat_block(block, &format!("memory.{group}"), true);
    }
    assert!(mem.get("alloc_kb_per_frame").is_none());

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
    assert!(gc["mode"].is_null());
    assert!(gc["collector_running_at_start"].is_null());
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
    let _spawn_budget = SpawnBudget::for_test(120);
    let doc = run_perf_json_window_with_sections(
        "games/match3", Some((4, 2)), true, Some("stopped"));
    let trace = &doc["memory"]["gc_window"]["jit_trace"];
    assert_eq!(trace["available"].as_bool(), Some(true), "counter-only attach required: {trace}");
    for key in ["starts", "compiles", "aborts", "flushes"] {
        assert!(trace[key].as_i64().is_some_and(|n| n >= 0), "window {key} missing: {trace}");
    }
    let mem = &doc["memory"];
    let gc = &mem["gc_window"];
    let split = &gc["allocation_split"];
    assert_eq!(split["available"], Value::Bool(true), "native origin split required: {split}");
    assert_eq!(split["basis"], "luajit-vmstate");
    let mut bytes = 0u64;
    let mut calls = 0u64;
    for origin in ["mutator", "jit_internal", "gc"] {
        bytes += split[origin]["request_bytes"].as_u64().expect("origin request bytes");
        calls += split[origin]["allocator_calls"].as_u64().expect("origin allocator calls");
        assert!(split[origin]["request_kb_per_frame"].as_f64().unwrap() >= 0.0);
    }
    assert_eq!(calls, gc["allocator_calls"].as_u64().unwrap());
    assert!((bytes as f64 / 1024.0 / 4.0 - gc["gross_request_kb_per_frame"].as_f64().unwrap()).abs() < 0.00011);
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
fn perf_json_gc_window_names_and_applies_both_collector_modes() {
    let _spawn_budget = SpawnBudget::for_test(120);
    for (mode, running) in [("stopped", false), ("running", true)] {
        let doc = run_perf_json_window_with_sections(
            "games/match3", Some((4, 2)), false, Some(mode));
        let gc = &doc["memory"]["gc_window"];
        assert_eq!(gc["mode"].as_str(), Some(mode), "mode label drifted: {gc}");
        assert_eq!(
            gc["collector_running_at_start"].as_bool(),
            Some(running),
            "{mode} did not control the real collector: {gc}"
        );
    }
}

#[test]
fn perf_json_retires_heap_delta_allocation_fields() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let doc = run_perf_json_window_with_sections(
        "games/match3", Some((4, 2)), true, Some("running"));
    let memory = &doc["memory"];
    assert!(
        memory.get("alloc_kb_per_frame").is_none(),
        "heap-delta allocation proxy must be removed: {memory}"
    );
    for section in doc["sections"].as_array().expect("sections array") {
        assert!(section.get("alloc_kb").is_none(), "old section proxy remains: {section}");
        assert!(section.get("request_kb").is_some(), "request traffic missing: {section}");
    }
}

fn perf_json_graphics_has_counters(doc: &Value) {
    let g = &doc["graphics"];
    for key in [
        "draw_calls",
        "text_draws",
        "shader_switches",
        "canvas_switches",
        "stream_upload_calls",
        "stream_storage_replacements",
        "stream_upload_bytes",
        "streamed_vertices",
        "retained_upload_calls",
        "retained_upload_bytes",
        "retained_uploaded_vertices",
        "world_active_batches",
        "world_mesh_batches",
        "world_batch_vertices",
        "ui_active_batches",
        "ui_mesh_batches",
        "ui_batch_vertices",
        "overlay_active_batches",
        "overlay_mesh_batches",
        "overlay_batch_vertices",
    ] {
        assert!(
            g.get(key).and_then(Value::as_i64).is_some(),
            "graphics.{key} missing or not an int: {g}"
        );
    }
    let active = g["world_active_batches"].as_i64().unwrap()
        + g["ui_active_batches"].as_i64().unwrap()
        + g["overlay_active_batches"].as_i64().unwrap();
    let vertices = g["world_batch_vertices"].as_i64().unwrap()
        + g["ui_batch_vertices"].as_i64().unwrap()
        + g["overlay_batch_vertices"].as_i64().unwrap();
    assert!(
        active > 0,
        "graphics batch census observed no active batches: {g}"
    );
    assert!(
        vertices > 0,
        "graphics batch census observed no vertices: {g}"
    );
    let mesh = g["world_mesh_batches"].as_i64().unwrap()
        + g["ui_mesh_batches"].as_i64().unwrap()
        + g["overlay_mesh_batches"].as_i64().unwrap();
    assert!(mesh > 0 && mesh <= active, "invalid mesh/active population: {g}");
}
