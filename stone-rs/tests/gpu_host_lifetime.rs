//! Context loss is terminal for a host whose callers retain GPU resources.
#![cfg(target_os = "linux")]

use std::cell::Cell;
use std::ptr;
use stone_rs::{
    stone_create_window_ex, stone_gpu_host_acquire, stone_init, stone_is_window_hidden,
    stone_teardown,
};

extern "C" {
    fn stone_create_image_data(width: i32, height: i32) -> u64;
    fn stone_new_image_from_data(data: u64, filter: i32) -> u64;
    fn stone_destroy_image_data(data: u64);
    fn stone_get_white_image() -> u64;
}

#[test]
fn gpu_host_observes_native_context_lifetime() {
    const CHILD: &str = "STONE_GPU_HOST_LIFETIME_CHILD";
    let Ok(mode) = std::env::var(CHILD) else {
        let before: Vec<_> = std::env::vars_os().collect();
        for mode in ["hidden", "borrowed", "replacement", "failure"] {
            let state = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "gpu_host_observes_native_context_lifetime",
                    "--nocapture",
                ])
                .env(CHILD, mode)
                .env("STONE_HEADLESS", "1")
                .env("XDG_STATE_HOME", state.path())
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stdout.contains("host fixture cleanup: teardown=true"),
                "{mode}: {stdout}\n{stderr}"
            );
            if mode == "failure" {
                assert!(
                    !output.status.success() && stderr.contains("intentional host fixture failure"),
                    "{stdout}\n{stderr}"
                );
            } else {
                assert!(output.status.success(), "{mode}: {stdout}\n{stderr}");
            }
        }
        assert_eq!(std::env::vars_os().collect::<Vec<_>>(), before);
        return;
    };

    let data = Cell::new(0);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        assert!(stone_init());
        let runner = if mode == "borrowed" {
            let runner = stone_create_window_ex(c"Runner".as_ptr(), 0, 0, 320, 240, 0, false, 0);
            assert!(!runner.is_null());
            runner
        } else {
            ptr::null_mut()
        };
        let host = stone_gpu_host_acquire();
        assert!(!host.is_null());
        if mode == "borrowed" {
            assert_eq!(
                host, runner,
                "the host borrows the runner's actual native window"
            );
        }
        assert!(stone_is_window_hidden());
        assert!(stone_create_window_ex(c"Second".as_ptr(), 0, 0, 320, 240, 0, false, 0).is_null());
        assert_eq!(
            stone_gpu_host_acquire(),
            host,
            "a refused second window preserves the context"
        );
        let white = stone_get_white_image();
        assert_ne!(white, 0);
        data.set(stone_create_image_data(1, 1));
        assert_ne!(data.get(), 0);
        assert_ne!(
            stone_new_image_from_data(data.get(), 0),
            0,
            "the host provides real GPU allocation"
        );
        if mode == "failure" {
            panic!("intentional host fixture failure");
        }
        assert!(stone_teardown());
        assert!(
            stone_gpu_host_acquire().is_null(),
            "destroyed context cannot be borrowed again"
        );
        assert_eq!(
            stone_new_image_from_data(data.get(), 0),
            0,
            "following GPU allocation refuses the destroyed context"
        );
        if mode == "replacement" {
            let replacement =
                stone_create_window_ex(c"Replacement".as_ptr(), 0, 0, 320, 240, 0, false, 0);
            assert!(!replacement.is_null());
            assert!(
                stone_gpu_host_acquire().is_null(),
                "a new window cannot own the host's old resources"
            );
            assert_ne!(
                stone_get_white_image(),
                white,
                "the replacement context starts without the ended context's textures"
            );
        }
    }));
    unsafe {
        stone_destroy_image_data(data.get());
    }
    let teardown = stone_teardown();
    println!("host fixture cleanup: teardown={teardown}");
    if let Err(error) = result {
        if !teardown {
            log::error!("host fixture cleanup failed before rethrow");
        }
        std::panic::resume_unwind(error);
    }
    assert!(teardown);
}
