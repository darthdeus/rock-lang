//! The string ABI validates before touching any part of native depth state.
#![cfg(target_os = "linux")]

use glow::HasContext;
use serial_test::serial;
use std::ffi::{c_char, CString, OsString};
use stone_rs::{stone_gpu_host_acquire, stone_init, stone_teardown};

extern "C" {
    fn stone_set_depth_mode(compare: *const c_char, write: bool);
    fn stone_gl_pass_state_begin() -> u32;
    fn stone_gl_pass_state_end(scope: u32) -> bool;
}

#[derive(Debug, PartialEq)]
struct DepthState(bool, u32, bool);

fn depth_state(gl: &glow::Context) -> DepthState {
    unsafe {
        DepthState(
            gl.is_enabled(glow::DEPTH_TEST),
            gl.get_parameter_i32(glow::DEPTH_FUNC) as u32,
            gl.get_parameter_bool(glow::DEPTH_WRITEMASK),
        )
    }
}

struct PassScope(u32);

impl PassScope {
    fn begin() -> Self {
        let scope = unsafe { stone_gl_pass_state_begin() };
        assert_ne!(scope, 0);
        Self(scope)
    }
}

impl Drop for PassScope {
    fn drop(&mut self) {
        // Do not turn a failed assertion into a double panic during cleanup.
        if !unsafe { stone_gl_pass_state_end(self.0) } {
            log::error!("depth fixture failed to restore its native pass scope");
            if !std::thread::panicking() {
                panic!("depth fixture scope restore failed");
            }
        }
    }
}

struct HostFixture {
    headless: Option<OsString>,
}

impl Drop for HostFixture {
    fn drop(&mut self) {
        let teardown = stone_teardown();
        match &self.headless {
            Some(value) => std::env::set_var("STONE_HEADLESS", value),
            None => std::env::remove_var("STONE_HEADLESS"),
        }
        if !teardown {
            log::error!("depth fixture native cleanup failed");
            if !std::thread::panicking() {
                panic!("depth fixture native cleanup failed");
            }
        }
    }
}

fn with_depth_host(body: impl FnOnce(&glow::Context)) {
    let fixture = HostFixture {
        headless: std::env::var_os("STONE_HEADLESS"),
    };
    std::env::set_var("STONE_HEADLESS", "1");
    assert!(stone_init());
    assert!(
        !stone_gpu_host_acquire().is_null(),
        "a real GL context is required"
    );
    let gl = unsafe {
        glow::Context::from_loader_function(|name| {
            sdl2::sys::SDL_GL_GetProcAddress(CString::new(name).unwrap().as_ptr()).cast()
        })
    };
    let scope = PassScope::begin();
    body(&gl);
    drop(scope);
    drop(fixture);
}

fn rejects(compare: *const c_char, diagnostic: &str) {
    with_depth_host(|gl| {
        for enabled in [false, true] {
            for write in [false, true] {
                unsafe {
                    if enabled {
                        gl.enable(glow::DEPTH_TEST);
                    } else {
                        gl.disable(glow::DEPTH_TEST);
                    }
                    gl.depth_func(glow::GREATER);
                    gl.depth_mask(write);
                }
                let before = depth_state(gl);
                let errors = rock_log::records_total(log::Level::Error);
                unsafe {
                    stone_set_depth_mode(compare, !write);
                }
                let records = rock_log::records(log::Level::Error);
                let logged = rock_log::records_total(log::Level::Error) == errors + 1
                    && records.iter().any(|r| {
                        r.target == "RENDER"
                            && r.message.contains("stone_set_depth_mode")
                            && r.message.contains(diagnostic)
                    });
                assert_eq!((depth_state(gl), logged), (before, true),
                    "rejected input must preserve enable/function/write and emit a shared-logger error: {records:?}");
            }
        }
        assert_eq!(unsafe { gl.get_error() }, glow::NO_ERROR);
    });
}

#[test]
#[serial]
fn unknown_depth_name_is_rejected_before_gl_mutation() {
    rejects(c"not-a-depth-comparison".as_ptr(), "not-a-depth-comparison");
}

#[test]
#[serial]
fn invalid_utf8_depth_name_is_rejected_before_gl_mutation() {
    rejects([0xff_u8, 0].as_ptr().cast(), "UTF-8");
}

#[test]
#[serial]
#[expect(
    clippy::assertions_on_constants,
    reason = "This fixture verifies host teardown after assertion failure."
)]
fn depth_fixture_cleans_up_after_failed_assertion() {
    let headless = std::env::var_os("STONE_HEADLESS");
    let failure = std::panic::catch_unwind(|| {
        with_depth_host(|_| {
            unsafe {
                stone_set_depth_mode(c"never".as_ptr(), true);
            }
            assert!(false, "intentional host fixture assertion failure");
        });
    });
    assert!(failure.is_err());
    assert_eq!(std::env::var_os("STONE_HEADLESS"), headless);
    assert!(
        !stone_rs::stone_is_initialized(),
        "fixture tears down after failure"
    );
}

#[test]
#[serial]
fn valid_depth_names_and_scope_cleanup_preserve_native_contract() {
    with_depth_host(|gl| {
        let errors = rock_log::records_total(log::Level::Error);
        for (name, func) in [
            (c"never", glow::NEVER),
            (c"less", glow::LESS),
            (c"equal", glow::EQUAL),
            (c"lequal", glow::LEQUAL),
            (c"lessequal", glow::LEQUAL),
            (c"greater", glow::GREATER),
            (c"notequal", glow::NOTEQUAL),
            (c"gequal", glow::GEQUAL),
            (c"greaterequal", glow::GEQUAL),
            (c"always", glow::ALWAYS),
        ] {
            for write in [true, false] {
                unsafe {
                    stone_set_depth_mode(name.as_ptr(), write);
                }
                assert_eq!(
                    depth_state(gl),
                    DepthState(name != c"always" || write, func, write),
                    "{name:?}"
                );
            }
        }
        for write in [true, false] {
            unsafe {
                stone_set_depth_mode(std::ptr::null(), write);
            }
            assert_eq!(depth_state(gl), DepthState(write, glow::ALWAYS, write));
        }
        // The disable sentinel preserves the previous function, even when it
        // was installed directly as a raw enum rather than through the ABI.
        unsafe {
            gl.depth_func(glow::GEQUAL);
            stone_set_depth_mode(c"always".as_ptr(), false);
        }
        let before = depth_state(gl);
        assert_eq!(before, DepthState(false, glow::GEQUAL, false));
        let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _scope = PassScope::begin();
            unsafe {
                stone_set_depth_mode(c"less".as_ptr(), true);
            }
            assert_eq!(
                depth_state(gl),
                before,
                "intentional depth fixture assertion failure"
            );
        }));
        assert!(failure.is_err(), "the cleanup probe must really fail");
        assert_eq!(
            depth_state(gl),
            before,
            "scope restores raw depth state after assertion failure"
        );
        assert_eq!(rock_log::records_total(log::Level::Error), errors);
        assert_eq!(unsafe { gl.get_error() }, glow::NO_ERROR);
    });
}
