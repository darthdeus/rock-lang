//! Tests for placement-preserving hide/show and initially-hidden window placement.

#![cfg(target_os = "linux")]

use serial_test::serial;
use std::ffi::{c_int, c_uchar, c_ulong, CString};
use stone_rs::{
    stone_create_window_ex, stone_get_fullscreen, stone_get_window_position, stone_get_window_size,
    stone_hide_window, stone_init, stone_is_window_hidden, stone_save_window_geometry,
    stone_set_fullscreen, stone_set_window_position, stone_set_window_size, stone_show_window,
    stone_show_window_if_deferred, stone_teardown,
};
use x11::xlib;

#[path = "support/x11_window.rs"]
mod x11_window;

#[path = "support/private_display.rs"]
mod private_display;

#[test]
#[serial]
fn show_after_hide_resets_user_time_zero_when_user_time_was_dirty() {
    if !private_display::available() {
        return;
    }
    std::env::set_var("SDL_VIDEODRIVER", "x11");
    std::env::remove_var("STONE_HEADLESS");
    assert!(stone_init());

    let title = CString::new("test-show-focus-dirty").unwrap();
    let window = stone_create_window_ex(title.as_ptr(), 100, 100, 320, 240, -1, true, 0);
    assert!(!window.is_null());

    stone_show_window_if_deferred();

    // Acquire X11 handle and simulate user interaction by setting _NET_WM_USER_TIME to non-zero
    let handles = x11_window::current();
    let display = handles.display;

    unsafe {
        let atom = xlib::XInternAtom(display, c"_NET_WM_USER_TIME".as_ptr(), 0);
        let nonzero_time: c_ulong = 999999;
        xlib::XChangeProperty(
            display,
            handles.window,
            atom,
            xlib::XA_CARDINAL,
            32,
            xlib::PropModeReplace,
            &nonzero_time as *const _ as *const u8,
            1,
        );
        xlib::XFlush(display);
    }

    stone_hide_window();
    assert!(stone_is_window_hidden());

    stone_show_window();

    unsafe {
        let atom = xlib::XInternAtom(display, c"_NET_WM_USER_TIME".as_ptr(), 0);
        let mut actual_type: xlib::Atom = 0;
        let mut actual_format: c_int = 0;
        let mut nitems: c_ulong = 0;
        let mut bytes_after: c_ulong = 0;
        let mut prop: *mut c_uchar = std::ptr::null_mut();
        let status = xlib::XGetWindowProperty(
            display,
            handles.window,
            atom,
            0,
            1,
            0,
            xlib::XA_CARDINAL,
            &mut actual_type,
            &mut actual_format,
            &mut nitems,
            &mut bytes_after,
            &mut prop,
        );
        assert_eq!(status, 0, "XGetWindowProperty failed");
        assert_eq!(actual_format, 32);
        assert_eq!(nitems, 1);
        let val = *(prop as *const c_ulong);
        xlib::XFree(prop as *mut _);
        assert_eq!(
            val, 0,
            "show_window must re-stamp _NET_WM_USER_TIME = 0 even if window had interaction"
        );
    }

    assert!(stone_teardown());
}

#[test]
#[serial]
fn initially_hidden_session_applies_saved_placement_on_first_show() {
    if !private_display::available() {
        return;
    }
    const CHILD: &str = "STONE_WINDOW_VISIBILITY_GEOMETRY_CHILD";
    const FAIL: &str = "STONE_WINDOW_VISIBILITY_GEOMETRY_FAIL";
    if std::env::var_os(CHILD).is_none() {
        let keys = [
            "XDG_STATE_HOME",
            "SDL_VIDEODRIVER",
            "STONE_HEADLESS",
            "STONE_IMMEDIATE_SHOW",
        ];
        let before: Vec<_> = keys.iter().map(std::env::var_os).collect();
        for fail in [true, false] {
            let temp = tempfile::tempdir().expect("isolated geometry state");
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "initially_hidden_session_applies_saved_placement_on_first_show",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env(FAIL, if fail { "1" } else { "0" })
                .env("XDG_STATE_HOME", temp.path())
                .env("SDL_VIDEODRIVER", "x11")
                .env_remove("STONE_HEADLESS")
                .env("STONE_IMMEDIATE_SHOW", "1")
                .env_remove("STONE_NO_WINDOW_FIT")
                .output()
                .expect("launch isolated saved-geometry fixture");
            let after: Vec<_> = keys.iter().map(std::env::var_os).collect();
            assert_eq!(
                after, before,
                "geometry child preserves caller environment after success and failure"
            );
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            if fail {
                assert!(
                    !output.status.success()
                        && stderr.contains("intentional saved-geometry fixture failure"),
                    "cleanup probe preserves its failure: {stdout}\n{stderr}"
                );
                assert!(
                    stdout.contains("cleanup: teardown=true"),
                    "native cleanup completes before the failure is rethrown: {stdout}\n{stderr}"
                );
            } else {
                assert!(
                    output.status.success(),
                    "saved-geometry fixture failed: {stdout}\n{stderr}"
                );
            }
        }
        return;
    }

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert!(stone_init());
        let title = CString::new("Test Hidden").unwrap();
        let seed = stone_create_window_ex(title.as_ptr(), 0, 0, 320, 240, -1, true, 0);
        assert!(!seed.is_null());
        stone_show_window();
        stone_show_window_if_deferred();
        let mut boot_w = 0;
        let mut boot_h = 0;
        stone_get_window_size(&mut boot_w, &mut boot_h);
        stone_set_window_size(960, 540);
        stone_set_window_position(150, 150);
        let mut expected_w = 0;
        let mut expected_h = 0;
        let mut expected_x = 0;
        let mut expected_y = 0;
        stone_get_window_size(&mut expected_w, &mut expected_h);
        stone_get_window_position(&mut expected_x, &mut expected_y);
        assert_ne!(
            (expected_w, expected_h),
            (boot_w, boot_h),
            "the production seed differs from boot placement"
        );
        stone_save_window_geometry(title.as_ptr());
        assert!(
            std::path::Path::new(&std::env::var_os("XDG_STATE_HOME").unwrap())
                .join("stone/window-geometry.json")
                .is_file(),
            "production saves geometry for the current topology"
        );
        assert!(stone_teardown());

        std::env::set_var("STONE_HEADLESS", "1");
        assert!(stone_init());
        let window = stone_create_window_ex(title.as_ptr(), 0, 0, 320, 240, -1, true, 0);
        assert!(!window.is_null());
        assert!(stone_is_window_hidden());
        if std::env::var(FAIL).as_deref() == Ok("1") {
            panic!("intentional saved-geometry fixture failure");
        }
        stone_show_window();
        assert!(!stone_is_window_hidden());
        stone_show_window_if_deferred();
        let mut w = 0;
        let mut h = 0;
        let mut x = 0;
        let mut y = 0;
        stone_get_window_size(&mut w, &mut h);
        stone_get_window_position(&mut x, &mut y);
        assert_eq!(
            (w, h),
            (expected_w, expected_h),
            "first show restores the actual saved window size"
        );
        assert_eq!(
            (x, y),
            (expected_x, expected_y),
            "first show restores the actual saved window position"
        );
    }));
    let torn_down = stone_teardown();
    println!("saved-geometry fixture cleanup: teardown={torn_down}");
    if let Err(error) = result {
        if !torn_down {
            log::error!("saved-geometry fixture cleanup failed before rethrow");
        }
        std::panic::resume_unwind(error);
    }
    assert!(
        torn_down,
        "saved-geometry fixture releases its native session"
    );
}

#[test]
#[serial]
fn hide_and_show_preserves_position_after_runtime_move() {
    if !private_display::available() {
        return;
    }
    std::env::set_var("SDL_VIDEODRIVER", "x11");
    std::env::remove_var("STONE_HEADLESS");
    std::env::set_var("STONE_IMMEDIATE_SHOW", "1");
    assert!(stone_init());

    let title = CString::new("test-move-hide-show").unwrap();
    let window = stone_create_window_ex(title.as_ptr(), 200, 200, 400, 300, -1, true, 0);
    assert!(!window.is_null());
    stone_show_window_if_deferred();

    stone_set_window_position(500, 350);
    let mut x = 0;
    let mut y = 0;
    stone_get_window_position(&mut x, &mut y);
    assert_eq!((x, y), (500, 350));

    stone_hide_window();
    assert!(stone_is_window_hidden());

    stone_show_window();
    assert!(!stone_is_window_hidden());
    stone_show_window_if_deferred();

    let mut restored_x = 0;
    let mut restored_y = 0;
    stone_get_window_position(&mut restored_x, &mut restored_y);
    assert_eq!(
        (restored_x, restored_y),
        (500, 350),
        "window position must be preserved after hide and show"
    );

    assert!(stone_teardown());
}

/// A reparenting WM (i3) destroys the frame window it put the client in when the
/// client withdraws. SDL's X11 position path reads the client's current parent
/// and then XTranslateCoordinates through it (SDL_x11window.c
/// X11_SetWindowPosition), so a move issued inside that teardown names a
/// destroyed window -- and Xlib's default error handler exits the whole process
/// with "X Error of failed request: BadWindow ... 40 (X_TranslateCoords)".
///
/// Each cycle issues the move `stone_show_window` itself starts with right after
/// the unmap, so the request lands inside the teardown; before the fix that
/// named the dead frame within the first few dozen cycles on the private i3
/// display. The move must survive the teardown and the shown window must still
/// carry the position it was given.
#[test]
#[serial]
fn hide_and_show_survives_wm_frame_teardown() {
    if !private_display::available() {
        return;
    }
    std::env::set_var("SDL_VIDEODRIVER", "x11");
    std::env::remove_var("STONE_HEADLESS");
    std::env::set_var("STONE_IMMEDIATE_SHOW", "1");
    assert!(stone_init());

    let title = CString::new("test-hide-show-teardown").unwrap();
    let window = stone_create_window_ex(title.as_ptr(), 200, 200, 400, 300, -1, true, 0);
    assert!(!window.is_null());
    stone_show_window_if_deferred();
    stone_set_window_position(500, 350);

    let handles = x11_window::current();
    let display = handles.display;
    let viewable = || unsafe {
        let mut attrs: xlib::XWindowAttributes = std::mem::zeroed();
        xlib::XGetWindowAttributes(display, handles.window, &mut attrs) != 0
            && attrs.map_state == xlib::IsViewable
    };

    const CYCLES: usize = 300;
    let mut mapped: usize = 0;
    for cycle in 0..CYCLES {
        stone_hide_window();
        assert!(stone_is_window_hidden());
        for _ in 0..3 {
            stone_set_window_position(500, 350);
        }
        stone_show_window();
        assert!(!stone_is_window_hidden());
        for _ in 0..250 {
            if viewable() {
                break;
            }
            stone_show_window_if_deferred();
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        if viewable() {
            mapped += 1;
            let mut x = 0;
            let mut y = 0;
            stone_get_window_position(&mut x, &mut y);
            assert_eq!(
                (x, y),
                (500, 350),
                "cycle {cycle} keeps the position across hide and show"
            );
        }
    }
    println!("mapped {mapped}/{CYCLES} cycles");
    assert!(
        mapped * 10 >= CYCLES * 9,
        "only {mapped}/{CYCLES} cycles reached a mapped window"
    );

    assert!(stone_teardown());
}

#[test]
#[serial]
fn hide_and_show_preserves_fullscreen_mode() {
    if !private_display::available() {
        return;
    }
    std::env::set_var("SDL_VIDEODRIVER", "x11");
    std::env::remove_var("STONE_HEADLESS");
    assert!(stone_init());

    let title = CString::new("test-fs-hide-show").unwrap();
    let window = stone_create_window_ex(title.as_ptr(), 100, 100, 320, 240, -1, true, 0);
    assert!(!window.is_null());
    stone_show_window_if_deferred();

    stone_set_fullscreen(1);
    assert_eq!(stone_get_fullscreen(), 1);

    stone_hide_window();
    assert!(stone_is_window_hidden());

    stone_show_window();
    assert!(!stone_is_window_hidden());
    stone_show_window_if_deferred();

    assert_eq!(
        stone_get_fullscreen(),
        1,
        "fullscreen mode must be preserved across hide and show"
    );

    assert!(stone_teardown());
}

#[test]
#[serial]
fn saving_geometry_while_hidden_does_not_corrupt_saved_state() {
    if !private_display::available() {
        return;
    }
    let temp_dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("XDG_STATE_HOME", temp_dir.path());
    std::env::set_var("SDL_VIDEODRIVER", "x11");
    std::env::remove_var("STONE_HEADLESS");
    assert!(stone_init());

    let title = CString::new("test-save-hidden").unwrap();
    let window = stone_create_window_ex(title.as_ptr(), 300, 200, 400, 300, -1, true, 0);
    assert!(!window.is_null());
    stone_show_window_if_deferred();

    stone_hide_window();
    assert!(stone_is_window_hidden());

    stone_save_window_geometry(title.as_ptr());

    let state_file = temp_dir.path().join("stone").join("window-geometry.json");
    if state_file.exists() {
        let content = std::fs::read_to_string(&state_file).unwrap();
        assert!(
            !content.contains("testsavehidden"),
            "hidden window must not write geometry file"
        );
    }

    assert!(stone_teardown());
}

#[test]
#[serial]
fn showing_fullscreen_session_as_windowed_preserves_actual_focus_and_pointer() {
    if !private_display::available() {
        return;
    }
    if !window_manager_is_running() {
        println!("skipping: no window manager on this display (only a WM owns X11 focus)");
        return;
    }
    std::env::set_var("SDL_VIDEODRIVER", "x11");
    std::env::set_var("STONE_HEADLESS", "1");
    std::env::remove_var("STONE_FOCUS_ON_SHOW");
    assert!(stone_init());
    let title = CString::new("test-passive-show").unwrap();
    let window = stone_create_window_ex(title.as_ptr(), 100, 100, 320, 240, -1, true, 0);
    assert!(!window.is_null());
    let handles = x11_window::current();
    let display = handles.display;
    let snapshot = || unsafe {
        xlib::XSync(display, 0);
        let mut focus = 0;
        let mut revert = 0;
        xlib::XGetInputFocus(display, &mut focus, &mut revert);
        let (mut root, mut child, mut rx, mut ry, mut wx, mut wy, mut mask) = (0, 0, 0, 0, 0, 0, 0);
        xlib::XQueryPointer(
            display,
            xlib::XDefaultRootWindow(display),
            &mut root,
            &mut child,
            &mut rx,
            &mut ry,
            &mut wx,
            &mut wy,
            &mut mask,
        );
        (focus, rx, ry)
    };
    stone_show_window();
    settle_window();
    stone_set_fullscreen(1);
    stone_hide_window();
    settle_window();
    let before = snapshot();
    stone_rs::stone_show_window_mode(0);
    settle_window();
    let after = snapshot();
    stone_hide_window();
    assert!(stone_teardown());
    std::env::remove_var("STONE_HEADLESS");
    assert_eq!(
        before, after,
        "ordinary show must preserve X11 focus and root pointer coordinates"
    );
}

#[test]
#[serial]
fn explicit_activation_works_after_headless_boot_is_shown() {
    if !private_display::available() {
        return;
    }
    if !window_manager_is_running() {
        println!("skipping: no window manager on this display (explicit activation is WM work)");
        return;
    }
    std::env::set_var("SDL_VIDEODRIVER", "x11");
    std::env::set_var("STONE_HEADLESS", "1");
    assert!(stone_init());
    let title = CString::new("test-explicit-activation").unwrap();
    let window = stone_create_window_ex(title.as_ptr(), 100, 100, 320, 240, -1, true, 0);
    assert!(!window.is_null());
    stone_show_window();
    settle_window();
    stone_rs::stone_activate_window(window);
    settle_window();
    let handles = x11_window::current();
    let mut focus = 0;
    let mut revert = 0;
    unsafe {
        xlib::XGetInputFocus(handles.display, &mut focus, &mut revert);
    }
    let expected = handles.window;
    stone_hide_window();
    assert!(stone_teardown());
    std::env::remove_var("STONE_HEADLESS");
    assert_eq!(
        focus, expected,
        "explicit activation must use live visibility, not the boot environment"
    );
}

/// Explicit activation is the window manager's job: on a bare X server (the
/// private Xvfb window tests run on) nobody grants focus and the assertion
/// would wait forever. SDL's no-WM `raise()` also sets input focus itself, so
/// "an ordinary show must not take focus" has no meaning there either.
/// `_NET_SUPPORTING_WM_CHECK` is the EWMH probe every real desktop's WM
/// publishes.
fn window_manager_is_running() -> bool {
    unsafe {
        let display = xlib::XOpenDisplay(std::ptr::null());
        if display.is_null() {
            return false;
        }
        let root = xlib::XDefaultRootWindow(display);
        // Create the atom if absent: querying property 0 is BadAtom, which aborts the process
        // through Xlib's default handler.
        let atom = xlib::XInternAtom(display, c"_NET_SUPPORTING_WM_CHECK".as_ptr(), 0);
        if atom == 0 {
            xlib::XCloseDisplay(display);
            return false;
        }
        let (mut actual_type, mut actual_format) = (0, 0);
        let (mut items, mut bytes_after) = (0, 0);
        let mut prop: *mut c_uchar = std::ptr::null_mut();
        let status = xlib::XGetWindowProperty(
            display,
            root,
            atom,
            0,
            1,
            0,
            xlib::XA_WINDOW,
            &mut actual_type,
            &mut actual_format,
            &mut items,
            &mut bytes_after,
            &mut prop,
        );
        let present =
            status == xlib::Success as c_int && actual_type == xlib::XA_WINDOW && items == 1;
        if !prop.is_null() {
            xlib::XFree(prop.cast());
        }
        xlib::XCloseDisplay(display);
        present
    }
}

fn settle_window() {
    for _ in 0..25 {
        unsafe { sdl2::sys::SDL_PumpEvents() };
        stone_show_window_if_deferred();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
