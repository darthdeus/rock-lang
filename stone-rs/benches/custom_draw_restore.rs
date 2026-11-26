//! CPU cost of the state restore after one custom draw that changed nothing:
//! the native stage snapshot and restore against the re-sync batch.rock did
//! before stages existed. Run with `cargo bench -p stone-rs --bench
//! custom_draw_restore`; it needs a GL context (headless is fine).

use std::ffi::{c_char, c_int, CString};
use std::time::Instant;

#[path = "../tests/support/host.rs"]
mod host;

extern "C" {
    fn stone_native_stage_begin() -> u32;
    fn stone_native_stage_end(scope: u32) -> u32;
    fn stone_restore_after_custom_draw();
    fn stone_create_canvas(width: c_int, height: c_int, format: c_int, with_depth: bool) -> u64;
    fn stone_destroy_canvas(handle: u64);
    fn stone_set_canvas(handle: u64);
    fn stone_set_depth_mode(compare: *const c_char, write: bool);
    fn stone_set_blend_mode_i(mode: c_int);
    fn stone_create_shader_from_combined(src: *const c_char) -> u64;
    fn stone_destroy_shader(handle: u64);
    fn stone_set_shader(handle: u64);
    fn stone_shader_set_texture(h: u64, n: *const c_char, unit: c_int);
}

const SHADER: &str = r#"#version 330 core
#ifdef VERTEX
layout(location = 0) in vec2 aPos;
void main() { gl_Position = vec4(aPos, 0.0, 1.0); }
#endif
#ifdef PIXEL
uniform sampler2D uTex0;
uniform sampler2D uTex1;
uniform sampler2D uTex2;
uniform sampler2D uTex3;
out vec4 oColor;
void main() {
    vec2 uv = vec2(0.5);
    oColor = texture(uTex0, uv) + texture(uTex1, uv) + texture(uTex2, uv) + texture(uTex3, uv);
}
#endif
"#;

const ROUNDS: usize = 7;
const ITERATIONS: usize = 20_000;

fn median_ns_per_iteration(gl: &glow::Context, mut body: impl FnMut()) -> f64 {
    use glow::HasContext;
    let mut rounds: Vec<f64> = (0..ROUNDS)
        .map(|_| {
            let started = Instant::now();
            for _ in 0..ITERATIONS {
                body();
            }
            unsafe { gl.finish() };
            started.elapsed().as_nanos() as f64 / ITERATIONS as f64
        })
        .collect();
    rounds.sort_by(f64::total_cmp);
    rounds[ROUNDS / 2]
}

fn main() {
    if std::env::args().any(|arg| arg == "--list") {
        return;
    }
    host::with_host(|gl| unsafe {
        let src = CString::new(SHADER).unwrap();
        let shader = stone_create_shader_from_combined(src.as_ptr());
        assert_ne!(shader, 0);
        for (unit, name) in [c"uTex0", c"uTex1", c"uTex2", c"uTex3"].iter().enumerate() {
            stone_shader_set_texture(shader, name.as_ptr(), unit as c_int);
        }
        let target = stone_create_canvas(256, 256, 0, true);
        assert_ne!(target, 0);
        stone_set_canvas(target);
        stone_set_shader(shader);
        stone_set_blend_mode_i(0);
        stone_set_depth_mode(c"always".as_ptr(), false);

        let stage = median_ns_per_iteration(gl, || {
            let scope = stone_native_stage_begin();
            assert_ne!(stone_native_stage_end(scope), 0);
        });
        let resync = median_ns_per_iteration(gl, || {
            stone_restore_after_custom_draw();
            stone_set_canvas(target);
            stone_set_depth_mode(c"always".as_ptr(), false);
            stone_set_blend_mode_i(0);
        });
        // The re-sync also made the next batch bind its shader again.
        let resync_and_shader = median_ns_per_iteration(gl, || {
            stone_restore_after_custom_draw();
            stone_set_canvas(target);
            stone_set_depth_mode(c"always".as_ptr(), false);
            stone_set_blend_mode_i(0);
            stone_set_shader(shader);
        });
        println!("stage begin+end: {stage:.0} ns per custom draw");
        println!("re-sync: {resync:.0} ns per custom draw");
        println!("re-sync + shader re-bind: {resync_and_shader:.0} ns per custom draw");

        stone_set_canvas(0);
        stone_destroy_canvas(target);
        stone_destroy_shader(shader);
    });
}
