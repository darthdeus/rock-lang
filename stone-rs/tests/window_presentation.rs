//! A headless screen must reach the actual window after any runtime show.
#![cfg(target_os = "linux")]

use glow::HasContext;
use std::ffi::CString;
use stone_rs::*;
use x11::xlib;

#[path = "support/x11_window.rs"]
mod x11_window;

extern "C" {
    fn stone_set_canvas(handle: u64);
    fn stone_clear_canvas(r: f32, g: f32, b: f32, a: f32);
    fn stone_read_window_rect(x: i32, y: i32, w: i32, h: i32, out: *mut u8);
}

#[test]
fn headless_screen_reaches_shown_window() {
    const CHILD: &str = "STONE_PRESENT_CHILD";
    const FAIL: &str = "STONE_PRESENT_FAIL";
    if std::env::var_os(CHILD).is_none() {
        if std::env::var_os("DISPLAY").is_none() {
            println!("skipping: needs an isolated X11 display");
            return;
        }
        let before: Vec<_> = std::env::vars_os().collect();
        for fail in [true, false] {
            let state = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "headless_screen_reaches_shown_window",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env(FAIL, if fail { "1" } else { "0" })
                .env("XDG_STATE_HOME", state.path())
                .env("SDL_VIDEODRIVER", "x11")
                .env("STONE_HEADLESS", "1")
                .env("STONE_NO_WINDOW_FIT", "1")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stdout.contains("presentation fixture cleaned up"),
                "{stdout}\n{stderr}"
            );
            if fail {
                assert!(
                    !output.status.success()
                        && stderr.contains("intentional presentation fixture failure"),
                    "{stdout}\n{stderr}"
                );
            } else {
                assert!(output.status.success(), "{stdout}\n{stderr}");
            }
        }
        assert_eq!(std::env::vars_os().collect::<Vec<_>>(), before);
        return;
    }

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        assert!(stone_init());
        let title = CString::new("headless-presentation-keeper").unwrap();
        let window = stone_create_window_ex(title.as_ptr(), 20, 20, 320, 240, -1, true, 0);
        assert!(!window.is_null());
        assert!(stone_is_window_hidden());
        let gl = glow::Context::from_loader_function(|name| {
            sdl2::sys::SDL_GL_GetProcAddress(CString::new(name).unwrap().as_ptr()).cast()
        });
        stone_set_canvas(0);
        let screen = gl.get_parameter_i32(glow::FRAMEBUFFER_BINDING);
        assert_ne!(
            screen, 0,
            "boot must use the production owned headless screen"
        );
        if std::env::var(FAIL).unwrap() == "1" {
            panic!("intentional presentation fixture failure");
        }

        stone_set_window_size(400, 280);
        stone_set_canvas(0);
        // A never-mapped native framebuffer can discard writes and still
        // read black. Make a forbidden hidden-screen read observably invalid
        // instead, then restore the attachments before asserting the outcome.
        let attachments = [glow::COLOR_ATTACHMENT0, glow::DEPTH_ATTACHMENT];
        let objects = attachments.map(|attachment| {
            let object = gl.get_framebuffer_attachment_parameter_i32(
                glow::FRAMEBUFFER,
                attachment,
                glow::FRAMEBUFFER_ATTACHMENT_OBJECT_NAME,
            );
            glow::NativeRenderbuffer(std::num::NonZeroU32::new(object as u32).unwrap())
        });
        for attachment in attachments {
            gl.framebuffer_renderbuffer(glow::FRAMEBUFFER, attachment, glow::RENDERBUFFER, None);
        }
        let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
        let errors_before = stone_gl_error_count();
        stone_swap_window(window);
        let errors_after = stone_gl_error_count();
        let raw_error = gl.get_error();
        for (attachment, object) in attachments.into_iter().zip(objects) {
            gl.framebuffer_renderbuffer(
                glow::FRAMEBUFFER,
                attachment,
                glow::RENDERBUFFER,
                Some(object),
            );
        }
        assert_ne!(
            status,
            glow::FRAMEBUFFER_COMPLETE,
            "hidden probe must make a read invalid"
        );
        assert_eq!(
            gl.check_framebuffer_status(glow::FRAMEBUFFER),
            glow::FRAMEBUFFER_COMPLETE
        );
        assert_eq!(
            errors_after, errors_before,
            "hidden swap must not read the screen"
        );
        assert_eq!(
            raw_error,
            glow::NO_ERROR,
            "hidden swap must not read the screen"
        );

        let handles = x11_window::current();
        let display = handles.display;
        // All runtime entry points share swap: ordinary show, windowed mode,
        // fullscreen mode, and hide/show after a resize.
        for (index, mode) in [None, Some(0), Some(1), Some(0)].into_iter().enumerate() {
            stone_hide_window();
            stone_set_window_size(400, 280);
            match mode {
                None => stone_show_window(),
                Some(mode) => stone_show_window_mode(mode),
            }
            // Generous because software GL (llvmpipe under a private Xvfb) can
            // take seconds to map a desktop-fullscreen window. The assertion is
            // that the show maps at all, not that it maps within 2s.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            loop {
                let mut event = StoneEvent::default();
                while stone_poll_event(&mut event) {}
                stone_show_window_if_deferred();
                xlib::XSync(display, 0);
                let mut attrs: xlib::XWindowAttributes = std::mem::zeroed();
                assert_ne!(
                    xlib::XGetWindowAttributes(display, handles.window, &mut attrs),
                    0
                );
                if attrs.map_state == xlib::IsViewable {
                    // Mapping can finish after the poll above. Let Stone observe
                    // this confirmed map before asking it to present pixels.
                    stone_show_window_if_deferred();
                    break;
                }
                assert!(std::time::Instant::now() < deadline,
                    "runtime show must map the window: mode={mode:?}, map_state={}, SDL flags={:#x}",
                    attrs.map_state, sdl2::sys::SDL_GetWindowFlags(sdl2::sys::SDL_GL_GetCurrentWindow()));
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert!(!stone_is_window_hidden());
            if mode != Some(1) {
                stone_set_window_size(400 + index as i32 * 20, 280);
            }
            stone_set_canvas(0);
            let mut width = 0;
            let mut height = 0;
            stone_get_drawable_size(&mut width, &mut height);
            // Seed FBO 0 explicitly: an undefined initial back buffer cannot
            // be a reliable black-window regression oracle.
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.clear_color(0.0, 0.0, 0.0, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
            stone_set_canvas(0);
            let colour = [51u8 + index as u8 * 20, 153, 230];
            stone_clear_canvas(
                colour[0] as f32 / 255.0,
                colour[1] as f32 / 255.0,
                colour[2] as f32 / 255.0,
                1.0,
            );
            let mut capture = vec![0; (width * height * 4) as usize];
            stone_read_window_rect(0, 0, width, height, capture.as_mut_ptr());
            assert_eq!(&capture[..3], &colour);
            if let Some(path) = std::env::var_os("STONE_PRESENT_CAPTURE") {
                image::save_buffer(
                    path,
                    &capture,
                    width as u32,
                    height as u32,
                    image::ColorType::Rgba8,
                )
                .unwrap();
            }
            // Present must ignore stale clipping, and preserve separate read
            // and draw bindings for the next frame and capture readers.
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
            gl.enable(glow::SCISSOR_TEST);
            gl.scissor(0, 0, 1, 1);
            stone_swap_window(window);
            gl.finish();
            assert_eq!(gl.get_parameter_i32(glow::READ_FRAMEBUFFER_BINDING), 0);
            assert_eq!(gl.get_parameter_i32(glow::DRAW_FRAMEBUFFER_BINDING), screen);
            assert!(gl.is_enabled(glow::SCISSOR_TEST));
            gl.disable(glow::SCISSOR_TEST);
            assert_eq!(gl.get_error(), glow::NO_ERROR);
            xlib::XSync(display, 0);
            let mut attrs: xlib::XWindowAttributes = std::mem::zeroed();
            assert_ne!(
                xlib::XGetWindowAttributes(display, handles.window, &mut attrs),
                0
            );
            assert_eq!(attrs.map_state, xlib::IsViewable);
            // Without a WM, Xvfb may retain the window extent during SDL's
            // desktop-fullscreen transition. Read the real X drawable extent.
            for (x, y) in [
                (1, 1),
                (attrs.width / 2, attrs.height / 2),
                (attrs.width - 2, attrs.height - 2),
            ] {
                let pixels =
                    xlib::XGetImage(display, handles.window, x, y, 1, 1, !0, xlib::ZPixmap);
                assert!(!pixels.is_null());
                let pixel = xlib::XGetPixel(pixels, 0, 0);
                let masks = [
                    (*pixels).red_mask,
                    (*pixels).green_mask,
                    (*pixels).blue_mask,
                ];
                let rgb = masks.map(|mask| ((pixel & mask) >> mask.trailing_zeros()) as u8);
                xlib::XDestroyImage(pixels);
                assert_eq!(
                    rgb, colour,
                    "shown window pixels at ({x},{y}), mode {mode:?}"
                );
            }
            let mut after = vec![0; capture.len()];
            stone_read_window_rect(0, 0, width, height, after.as_mut_ptr());
            assert_eq!(
                after, capture,
                "presentation leaves the capture source byte-identical"
            );
        }
        assert!(stone_teardown());
        std::env::remove_var("STONE_HEADLESS");
        let visible = stone_create_window_ex(title.as_ptr(), 20, 20, 320, 240, -1, true, 0);
        assert!(!visible.is_null());
        stone_show_window_if_deferred();
        stone_show_window_if_deferred();
        stone_set_canvas(0);
        assert_eq!(
            gl.get_parameter_i32(glow::FRAMEBUFFER_BINDING),
            0,
            "a subsequent visible boot must not inherit the old owned screen"
        );
        stone_clear_canvas(0.1, 0.2, 0.3, 1.0);
        stone_swap_window(visible);
        assert_eq!(gl.get_error(), glow::NO_ERROR);
    }));
    assert!(stone_teardown());
    println!("presentation fixture cleaned up");
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}
