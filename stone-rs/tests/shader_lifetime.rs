//! Shader construction releases every GL object it created unless registration succeeds.
#![cfg(target_os = "linux")]

use glow::HasContext;
use serial_test::serial;
use std::ffi::{c_char, CString, OsString};
use stone_rs::{stone_gpu_host_acquire, stone_init, stone_teardown};

extern "C" {
    fn stone_create_shader_from_combined(src: *const c_char) -> u64;
    fn stone_destroy_shader(handle: u64);
}

const COMBINED_VALID: &str = "#version 330 core\n#ifdef VERTEX\nlayout(location = 0) in vec2 p;\nout vec2 uv;\nvoid main() { uv = p; gl_Position = vec4(p, 0.0, 1.0); }\n#endif\n#ifdef PIXEL\nin vec2 uv;\nout vec4 c;\nvoid main() { c = vec4(uv, 0.0, 1.0); }\n#endif\n";
const COMBINED_BAD_VERTEX: &str = "#version 330 core\n#ifdef VERTEX\nvoid main() { not valid glsl }\n#endif\n#ifdef PIXEL\nout vec4 c;\nvoid main() { c = vec4(1.0); }\n#endif\n";
const COMBINED_BAD_FRAGMENT: &str = "#version 330 core\n#ifdef VERTEX\nlayout(location = 0) in vec2 p;\nvoid main() { gl_Position = vec4(p, 0.0, 1.0); }\n#endif\n#ifdef PIXEL\nvoid main() { not valid glsl }\n#endif\n";
// Declares `uv` with a different type than the vertex output: both stages compile, linking fails.
const COMBINED_UNLINKABLE: &str = "#version 330 core\n#ifdef VERTEX\nlayout(location = 0) in vec2 p;\nout vec2 uv;\nvoid main() { uv = p; gl_Position = vec4(p, 0.0, 1.0); }\n#endif\n#ifdef PIXEL\nin vec3 uv;\nout vec4 c;\nvoid main() { c = vec4(uv, 1.0); }\n#endif\n";

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
            log::error!("shader fixture native cleanup failed");
            if !std::thread::panicking() {
                panic!("shader fixture native cleanup failed");
            }
        }
    }
}

fn with_host(body: impl FnOnce(&glow::Context)) {
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
    body(&gl);
    drop(fixture);
}

/// Live (shader, program) objects in the context. Names are allocated from 1 in a fresh
/// context, so probing a bounded range sees every object the test can create.
fn live_objects(gl: &glow::Context) -> (usize, usize) {
    const PROBE: u32 = 256;
    unsafe {
        let shaders = (1..=PROBE)
            .filter(|n| gl.is_shader(glow::NativeShader(std::num::NonZeroU32::new(*n).unwrap())))
            .count();
        let programs = (1..=PROBE)
            .filter(|n| gl.is_program(glow::NativeProgram(std::num::NonZeroU32::new(*n).unwrap())))
            .count();
        (shaders, programs)
    }
}

fn combined(src: &str) -> u64 {
    let src = CString::new(src).unwrap();
    unsafe { stone_create_shader_from_combined(src.as_ptr()) }
}

fn assert_failures_release_everything(gl: &glow::Context, create: &dyn Fn() -> u64) {
    let before = live_objects(gl);
    for _ in 0..5 {
        assert_eq!(create(), 0);
        assert_eq!(
            live_objects(gl),
            before,
            "a failed construction left GL objects behind"
        );
    }
}

#[test]
#[serial]
fn combined_constructor_releases_objects_when_a_stage_or_link_fails() {
    with_host(|gl| {
        assert_failures_release_everything(gl, &|| combined(COMBINED_BAD_VERTEX));
        assert_failures_release_everything(gl, &|| combined(COMBINED_BAD_FRAGMENT));
        assert_failures_release_everything(gl, &|| combined(COMBINED_UNLINKABLE));
    });
}

#[test]
#[serial]
fn valid_construction_owns_two_shaders_and_a_program_until_destroyed() {
    with_host(|gl| {
        let before = live_objects(gl);
        let handle = combined(COMBINED_VALID);
        assert_ne!(handle, 0);
        assert_eq!(live_objects(gl), (before.0 + 2, before.1 + 1));
        unsafe { stone_destroy_shader(handle) };
        assert_eq!(live_objects(gl), before);
    });
}
