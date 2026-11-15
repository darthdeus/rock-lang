#![cfg(target_os = "linux")]

use glow::HasContext;
use stone_rs::*;

#[test]
fn delayed_gpu_results_keep_submission_identity() {
    const CHILD: &str = "STONE_FRAME_TIMING_TEST";
    if std::env::var_os(CHILD).is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("frames.jsonl");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "delayed_gpu_results_keep_submission_identity",
                "--nocapture",
            ])
            .env_remove("STONE_GPU_SCOPES")
            .env(CHILD, "1")
            .env("STONE_FRAME_TIMING", &path)
            .env("STONE_HEADLESS", "1")
            .env("XDG_STATE_HOME", dir.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let rows: Vec<serde_json::Value> = std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows[0]["dropped"], 0);
        let frames = |kind: &str| {
            rows.iter()
                .filter(|r| r["event"] == kind)
                .map(|r| r["frame"].as_u64().unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(frames("swap_end"), vec![1, 2, 3, 4, 5, 6]);
        assert_eq!(frames("gpu_drop"), vec![1, 2]);
        assert_eq!(frames("gpu_result"), vec![3, 4, 5, 6]);
        for row in rows.iter().filter(|r| r["event"] == "gpu_result") {
            assert_eq!(row["observed_frame"], 6);
            assert!(row["gpu_end_ns"].as_u64().unwrap() >= row["gpu_start_ns"].as_u64().unwrap());
        }
        let pass = rows.iter().find(|r| r["event"] == "pass_result").unwrap();
        assert_eq!(pass["frame"], 1);
        assert_eq!(pass["observed_frame"], 6);
        assert_eq!(pass["tag"], 42);
        assert_eq!(frames("gpu_pending"), vec![7]);
        return;
    }
    struct Cleanup;
    impl Drop for Cleanup {
        fn drop(&mut self) {
            stone_teardown();
        }
    }
    assert!(stone_init());
    let _cleanup = Cleanup;
    let window = stone_create_window_ex(c"frame timing".as_ptr(), 0, 0, 320, 240, 0, false, 0);
    assert!(!window.is_null());
    stone_set_vsync(0);
    let gl = unsafe {
        glow::Context::from_loader_function(|name| {
            sdl2::sys::SDL_GL_GetProcAddress(std::ffi::CString::new(name).unwrap().as_ptr()).cast()
        })
    };
    stone_gpu_timed_pass_begin();
    stone_gpu_timed_pass_end(42);
    for _ in 0..6 {
        stone_gpu_timer_begin();
        stone_gpu_timer_end();
        stone_swap_window(window);
    }
    unsafe {
        gl.finish();
    }
    for _ in 0..4 {
        assert!(stone_gpu_timer_poll_ms() >= 0.0);
    }
    assert!(stone_gpu_timed_pass_poll_ms() >= 0.0);
    stone_gpu_timer_begin();
    stone_gpu_timer_end();
    assert!(
        !std::path::Path::new(&std::env::var_os("STONE_FRAME_TIMING").unwrap()).exists(),
        "recording must not write while frames are running"
    );
}
