#![cfg(target_os = "linux")]

#[path = "support/host.rs"]
mod host;

use glow::HasContext;

extern "C" {
    fn stone_create_canvas(width: i32, height: i32, format: i32, with_depth: bool) -> u64;
    fn stone_destroy_canvas(handle: u64);
    fn stone_canvas_resize(handle: u64, width: i32, height: i32) -> bool;
    fn stone_get_live_canvas_count() -> i32;
    fn stone_canvas_capacity_w(handle: u64) -> i32;
    fn stone_canvas_capacity_h(handle: u64) -> i32;
}

#[test]
fn canvas_allocation_and_resize_respect_the_texture_limit() {
    host::with_host(|gl| unsafe {
        let limit = gl.get_parameter_i32(glow::MAX_TEXTURE_SIZE);
        assert!(limit > 0 && limit < i32::MAX);
        for (width, height) in [(limit, 1), (1, limit)] {
            let canvas = stone_create_canvas(width, height, 0, true);
            assert_ne!(canvas, 0, "the device limit is inclusive");
            assert_eq!(gl.get_error(), glow::NO_ERROR);
            stone_destroy_canvas(canvas);
        }

        let canvas = stone_create_canvas(2, 2, 0, true);
        assert_ne!(canvas, 0);
        for (width, height) in [(limit, 1), (1, limit)] {
            assert!(stone_canvas_resize(canvas, width, height));
            assert_eq!(gl.get_error(), glow::NO_ERROR);
        }
        let count = stone_get_live_canvas_count();
        for (width, height) in [(limit + 1, 1), (1, limit + 1)] {
            let errors = rock_log::records_total(log::Level::Error);
            assert_eq!(stone_create_canvas(width, height, 0, true), 0);
            assert!(!stone_canvas_resize(canvas, width, height));
            assert_eq!(rock_log::records_total(log::Level::Error), errors + 2);
            assert!(rock_log::records(log::Level::Error).iter().any(|record| {
                record.target == "CANVAS"
                    && record.message.contains(&format!("canvas {width}x{height}"))
                    && record
                        .message
                        .contains(&format!("GL texture size range 1..={limit}"))
            }));
            assert_eq!(stone_get_live_canvas_count(), count);
            assert_eq!(stone_canvas_capacity_w(canvas), 1);
            assert_eq!(stone_canvas_capacity_h(canvas), limit);
            assert_eq!(gl.get_error(), glow::NO_ERROR);
        }
        stone_destroy_canvas(canvas);
    });
}
