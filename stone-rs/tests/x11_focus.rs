//! Tests for x11_focus::set_user_time_zero -- the no-focus-steal stamp
//! stone_create_window_ex applies to a still-hidden window before showing it.
//!
//! The dummy-driver test is hermetic (no X server needed): SDL's dummy
//! video driver has no GetWindowWMInfo hook, so SDL_GetWindowWMInfo
//! returns false with "That operation is not supported" -- the same
//! failure a degraded/SSH X session produces. sdl2 0.38's
//! raw-window-handle wrappers panic!() on that failure (which used to
//! unwind out of extern "C" stone_create_window_ex and abort the
//! process); the stamp must degrade to a warn instead.
//!
//! The X11 test needs a real server and skips with a print when $DISPLAY
//! is absent.
//!
//! Both tests are #[serial]: SDL_VIDEODRIVER is process-global env and
//! SDL only initializes one video driver per process.

#![cfg(target_os = "linux")]

use std::ffi::{c_int, c_uchar, c_ulong};

use serial_test::serial;
use stone_rs::x11_focus;
use x11::xlib;

fn make_window(driver: &str) -> (sdl2::Sdl, sdl2::video::Window) {
    std::env::set_var("SDL_VIDEODRIVER", driver);
    let sdl = sdl2::init().expect("SDL init");
    let video = sdl.video().expect("video subsystem");
    let window = video
        .window("focus-test", 64, 64)
        .hidden()
        .build()
        .expect("window build");
    (sdl, window)
}

fn wm_info(window: &sdl2::video::Window) -> (sdl2::sys::SDL_bool, sdl2::sys::SDL_SysWMinfo) {
    let mut info: sdl2::sys::SDL_SysWMinfo = unsafe { std::mem::zeroed() };
    unsafe { sdl2::sys::SDL_GetVersion(&mut info.version) };
    let ok = unsafe { sdl2::sys::SDL_GetWindowWMInfo(window.raw(), &mut info) };
    (ok, info)
}

/// The degraded-session path: a video driver without WMInfo support must
/// degrade to a warn + skipped stamp, never a panic.
#[test]
#[serial]
fn no_wminfo_degrades_gracefully() {
    let (_sdl, window) = make_window("dummy");

    // Premise: the dummy driver really lacks the WMInfo hook. If this
    // ever flips, the test no longer exercises the failure path.
    let (ok, _) = wm_info(&window);
    assert!(
        ok == sdl2::sys::SDL_bool::SDL_FALSE,
        "dummy driver unexpectedly supports WMInfo"
    );

    x11_focus::set_user_time_zero(&window);
    assert_eq!(
        x11_focus::request_deferred_map(&window),
        x11_focus::DeferredMapState::Unsupported,
    );
    assert_eq!(
        x11_focus::poll_deferred_map(&window),
        x11_focus::DeferredMapState::Unsupported,
    );
}

#[test]
#[serial]
fn deferred_map_request_does_not_wait_for_map_notify() {
    if std::env::var("DISPLAY").is_err() {
        println!("skipping: no DISPLAY");
        return;
    }
    let (_sdl, window) = make_window("x11");

    assert_eq!(
        x11_focus::request_deferred_map(&window),
        x11_focus::DeferredMapState::Pending,
    );
    assert_ne!(
        x11_focus::poll_deferred_map(&window),
        x11_focus::DeferredMapState::Unsupported,
    );
}

/// Happy path on a real X server: the stamp must leave
/// _NET_WM_USER_TIME = 0 (format-32 CARDINAL) on the window.
#[test]
#[serial]
fn stamps_user_time_zero_on_x11() {
    if std::env::var("DISPLAY").is_err() {
        println!("skipping: no DISPLAY");
        return;
    }
    let (_sdl, window) = make_window("x11");

    x11_focus::set_user_time_zero(&window);

    let (ok, info) = wm_info(&window);
    assert!(
        ok == sdl2::sys::SDL_bool::SDL_TRUE,
        "x11 driver must support WMInfo"
    );
    let dpy = unsafe { info.info.x11 }.display as *mut xlib::Display;
    let win = unsafe { info.info.x11 }.window;

    unsafe {
        let atom = xlib::XInternAtom(dpy, c"_NET_WM_USER_TIME".as_ptr(), 0);
        let mut actual_type: xlib::Atom = 0;
        let mut actual_format: c_int = 0;
        let mut nitems: c_ulong = 0;
        let mut bytes_after: c_ulong = 0;
        let mut prop: *mut c_uchar = std::ptr::null_mut();
        // Same connection the stamp went through, so ordering guarantees
        // the XChangeProperty is processed before this round-trip.
        let status = xlib::XGetWindowProperty(
            dpy,
            win,
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
        assert_eq!(
            actual_type,
            xlib::XA_CARDINAL,
            "property missing or wrong type"
        );
        assert_eq!(actual_format, 32);
        assert_eq!(nitems, 1);
        assert_eq!(*(prop as *const c_ulong), 0, "user-time must be zero");
        xlib::XFree(prop as *mut _);
    }
}
