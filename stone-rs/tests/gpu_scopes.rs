//! Both GPU scope recorders, timestamp queries and glFinish brackets, report
//! the same scope tree through the perf JSON.
#![cfg(target_os = "linux")]

#[path = "support/host.rs"]
mod host;

use glow::HasContext;
use std::ffi::{c_char, CStr};

extern "C" {
    fn stone_gpu_timer_begin();
    fn stone_gpu_timer_end();
    fn stone_gpu_timer_poll_ms() -> f32;
    fn stone_gpu_timed_pass_begin();
    fn stone_gpu_timed_pass_end(tag: u32);
    fn stone_gpu_timed_pass_poll_ms() -> f32;
    fn stone_gpu_timed_pass_tag() -> u32;
    fn stone_gpu_scopes_requested() -> u8;
    fn stone_gpu_scopes_finish() -> u8;
    fn stone_gpu_scope_begin(name: *const c_char);
    fn stone_gpu_scope_end();
    fn stone_gpu_scopes_reset_stats();
    fn stone_gpu_scopes_json() -> *const c_char;
}

const FRAMES: usize = 6;

unsafe fn scopes_json() -> serde_json::Value {
    let json = CStr::from_ptr(stone_gpu_scopes_json()).to_str().unwrap();
    serde_json::from_str(json).unwrap()
}

/// `FRAMES` balanced frames of `a { b }`, then one with a stray end and one
/// that leaves `a` open. Returns the frame GPU ms polled along the way.
unsafe fn record_frames(gl: &glow::Context) -> Vec<f32> {
    let mut frame_ms = Vec::new();
    for frame in 0..FRAMES + 2 {
        stone_gpu_timer_begin();
        stone_gpu_scope_begin(c"a".as_ptr());
        gl.clear(glow::COLOR_BUFFER_BIT);
        stone_gpu_scope_begin(c"b".as_ptr());
        gl.clear(glow::COLOR_BUFFER_BIT);
        stone_gpu_scope_end();
        if frame != FRAMES + 1 {
            stone_gpu_scope_end();
        }
        if frame == FRAMES {
            stone_gpu_scope_end();
        }
        stone_gpu_timer_end();
        gl.finish();
        let ms = stone_gpu_timer_poll_ms();
        if ms >= 0.0 {
            frame_ms.push(ms);
        }
    }
    frame_ms
}

fn row<'a>(json: &'a serde_json::Value, path: &str) -> &'a serde_json::Value {
    json["scopes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["path"] == path)
        .unwrap_or_else(|| panic!("no scope {path} in {json}"))
}

unsafe fn timestamp_queries(gl: &glow::Context) {
    assert_eq!(
        (stone_gpu_scopes_requested(), stone_gpu_scopes_finish()),
        (1, 0)
    );
    let mut frame_ms = record_frames(gl);
    for _ in 0..100 {
        let ms = stone_gpu_timer_poll_ms();
        if ms == -1.0 {
            break;
        }
        frame_ms.push(ms);
    }
    assert_eq!(frame_ms.len(), FRAMES + 2, "every frame reads back once");
    assert!(frame_ms.iter().all(|ms| *ms >= 0.0), "{frame_ms:?}");

    let json = scopes_json();
    assert_eq!(json["status"], "on", "{json}");
    assert_eq!(json["frames"], FRAMES as u64);
    assert_eq!(json["dropped_frames"], 2);
    for path in ["frame", "frame/a", "frame/a/b"] {
        let row = row(&json, path);
        assert_eq!(row["ms"]["n"], FRAMES as u64, "{path}");
        assert!(row.get("wait_ms_per_frame").is_none(), "{path}");
    }
    assert!(json["scopes"]
        .as_array()
        .unwrap()
        .iter()
        .all(|row| row["path"] != "before_frame"));

    stone_gpu_timed_pass_begin();
    gl.clear(glow::COLOR_BUFFER_BIT);
    stone_gpu_timed_pass_end(7);
    gl.finish();
    assert!(stone_gpu_timed_pass_poll_ms() >= 0.0);
    assert_eq!(stone_gpu_timed_pass_tag(), 7);
    assert_eq!(stone_gpu_timed_pass_poll_ms(), -1.0);

    stone_gpu_scopes_reset_stats();
    assert_eq!(scopes_json()["frames"], 0);
}

unsafe fn finish_brackets(gl: &glow::Context) {
    assert_eq!(
        (stone_gpu_scopes_requested(), stone_gpu_scopes_finish()),
        (1, 1)
    );
    let frame_ms = record_frames(gl);
    assert!(frame_ms.is_empty(), "finish mode reports no frame GPU time");
    assert_eq!(stone_gpu_timer_poll_ms(), -2.0);

    let json = scopes_json();
    assert_eq!(json["status"], "finish", "{json}");
    assert_eq!(json["frames"], FRAMES as u64, "frames ingest as they end");
    assert_eq!(json["dropped_frames"], 2);
    for path in ["before_frame", "frame", "frame/a", "frame/a/b"] {
        let row = row(&json, path);
        assert_eq!(row["ms"]["n"], FRAMES as u64, "{path}");
        let wait = row["wait_ms_per_frame"].as_f64().unwrap();
        assert!(wait >= 0.0 && wait <= row["ms_per_frame"].as_f64().unwrap() + 1e-9);
    }

    stone_gpu_timed_pass_begin();
    stone_gpu_timed_pass_end(7);
    assert_eq!(stone_gpu_timed_pass_poll_ms(), -2.0);
}

// The host cannot start twice in one process, and nextest gives each test its own.
fn with_scopes(mode: &str, body: impl FnOnce(&glow::Context)) {
    std::env::set_var("STONE_GPU_SCOPES", mode);
    host::with_host(body);
    std::env::remove_var("STONE_GPU_SCOPES");
}

#[test]
fn timestamp_query_scopes_read_back_per_frame() {
    with_scopes("1", |gl| unsafe { timestamp_queries(gl) });
}

#[test]
fn finish_scopes_ingest_as_each_frame_ends() {
    with_scopes("finish", |gl| unsafe { finish_brackets(gl) });
}
