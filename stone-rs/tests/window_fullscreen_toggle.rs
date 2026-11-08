//! Tests for Alt+Enter fullscreen toggling, repeat suppression, and bounds restoration.

#![cfg(target_os = "linux")]

use serial_test::serial;
use std::ffi::CString;
use stone_rs::{
    stone_create_window_ex, stone_get_fullscreen, stone_get_window_position, stone_get_window_size,
    stone_init, stone_poll_event, stone_set_fullscreen, stone_show_window_if_deferred,
    stone_teardown, stone_toggle_fullscreen, StoneEvent,
};

#[test]
#[serial]
fn toggle_fullscreen_restores_windowed_bounds() {
    if std::env::var("DISPLAY").is_err() {
        println!("skipping: no DISPLAY");
        return;
    }
    std::env::set_var("SDL_VIDEODRIVER", "x11");
    std::env::remove_var("STONE_HEADLESS");
    std::env::set_var("STONE_IMMEDIATE_SHOW", "1");
    assert!(stone_init());

    let title = CString::new("test-toggle-fs").unwrap();
    let window = stone_create_window_ex(title.as_ptr(), 200, 150, 480, 360, -1, true, 0);
    assert!(!window.is_null());
    assert_eq!(stone_get_fullscreen(), 0);

    let mut orig_w = 0;
    let mut orig_h = 0;
    stone_get_window_size(&mut orig_w, &mut orig_h);
    let mut orig_x = 0;
    let mut orig_y = 0;
    stone_get_window_position(&mut orig_x, &mut orig_y);

    // Toggle to desktop fullscreen
    let new_mode = stone_toggle_fullscreen();
    assert_eq!(new_mode, 1);
    assert_eq!(stone_get_fullscreen(), 1);

    // Toggle back to windowed: must restore original bounds
    let restored_mode = stone_toggle_fullscreen();
    assert_eq!(restored_mode, 0);
    assert_eq!(stone_get_fullscreen(), 0);

    let mut w = 0;
    let mut h = 0;
    stone_get_window_size(&mut w, &mut h);
    let mut x = 0;
    let mut y = 0;
    stone_get_window_position(&mut x, &mut y);

    assert_eq!(
        (w, h),
        (orig_w, orig_h),
        "toggling back to windowed must restore original size"
    );
    assert_eq!(
        (x, y),
        (orig_x, orig_y),
        "toggling back to windowed must restore original position"
    );

    assert!(stone_teardown());
}

#[test]
#[serial]
fn alt_enter_key_event_toggles_fullscreen_and_suppresses_repeat() {
    if std::env::var("DISPLAY").is_err() {
        println!("skipping: no DISPLAY");
        return;
    }
    std::env::set_var("SDL_VIDEODRIVER", "x11");
    std::env::remove_var("STONE_HEADLESS");
    std::env::set_var("STONE_IMMEDIATE_SHOW", "1");
    assert!(stone_init());

    let title = CString::new("test-alt-enter-repeat").unwrap();
    let window = stone_create_window_ex(title.as_ptr(), 200, 150, 480, 360, -1, true, 0);
    assert!(!window.is_null());
    assert_eq!(stone_get_fullscreen(), 0);

    let mut event = StoneEvent::default();
    // Drain creation events
    while stone_poll_event(&mut event) {}

    // Push Alt+Enter KeyDown with repeat = false
    unsafe {
        let mut sdl_ev: sdl2::sys::SDL_Event = std::mem::zeroed();
        sdl_ev.type_ = sdl2::sys::SDL_EventType::SDL_KEYDOWN as u32;
        sdl_ev.key.type_ = sdl2::sys::SDL_EventType::SDL_KEYDOWN as u32;
        sdl_ev.key.state = sdl2::sys::SDL_PRESSED as u8;
        sdl_ev.key.repeat = 0;
        sdl_ev.key.keysym.sym = sdl2::sys::SDL_KeyCode::SDLK_RETURN as i32;
        sdl_ev.key.keysym.mod_ = sdl2::sys::SDL_Keymod::KMOD_LALT as u16;
        assert_eq!(sdl2::sys::SDL_PushEvent(&mut sdl_ev), 1);
    }

    while stone_poll_event(&mut event) {}
    // Window should now be in desktop fullscreen mode
    assert_eq!(
        stone_get_fullscreen(),
        1,
        "Alt+Enter keydown should switch to fullscreen"
    );

    // Simulate key repeat while holding Alt+Enter (repeat = 1)
    unsafe {
        let mut sdl_ev: sdl2::sys::SDL_Event = std::mem::zeroed();
        sdl_ev.type_ = sdl2::sys::SDL_EventType::SDL_KEYDOWN as u32;
        sdl_ev.key.type_ = sdl2::sys::SDL_EventType::SDL_KEYDOWN as u32;
        sdl_ev.key.state = sdl2::sys::SDL_PRESSED as u8;
        sdl_ev.key.repeat = 1; // Repeat event!
        sdl_ev.key.keysym.sym = sdl2::sys::SDL_KeyCode::SDLK_RETURN as i32;
        sdl_ev.key.keysym.mod_ = sdl2::sys::SDL_Keymod::KMOD_LALT as u16;
        assert_eq!(sdl2::sys::SDL_PushEvent(&mut sdl_ev), 1);
    }

    while stone_poll_event(&mut event) {}
    // Window should STILL be in fullscreen mode (repeat suppressed!)
    assert_eq!(
        stone_get_fullscreen(),
        1,
        "holding Alt+Enter should suppress repeat and not toggle back"
    );

    // Push Alt+Enter KeyDown again with repeat = 0 (new press)
    unsafe {
        let mut sdl_ev: sdl2::sys::SDL_Event = std::mem::zeroed();
        sdl_ev.type_ = sdl2::sys::SDL_EventType::SDL_KEYDOWN as u32;
        sdl_ev.key.type_ = sdl2::sys::SDL_EventType::SDL_KEYDOWN as u32;
        sdl_ev.key.state = sdl2::sys::SDL_PRESSED as u8;
        sdl_ev.key.repeat = 0;
        sdl_ev.key.keysym.sym = sdl2::sys::SDL_KeyCode::SDLK_RETURN as i32;
        sdl_ev.key.keysym.mod_ = sdl2::sys::SDL_Keymod::KMOD_LALT as u16;
        assert_eq!(sdl2::sys::SDL_PushEvent(&mut sdl_ev), 1);
    }

    while stone_poll_event(&mut event) {}
    // Window should now be toggled back to windowed mode
    assert_eq!(
        stone_get_fullscreen(),
        0,
        "second distinct Alt+Enter press should toggle back to windowed"
    );

    let mut w = 0;
    let mut h = 0;
    stone_get_window_size(&mut w, &mut h);
    let mut x = 0;
    let mut y = 0;
    stone_get_window_position(&mut x, &mut y);
    assert_eq!((w, h), (480, 360));

    assert!(stone_teardown());
}

fn run_frames(frames: u32) {
    let mut event = StoneEvent::default();
    for _ in 0..frames {
        while stone_poll_event(&mut event) {}
        stone_show_window_if_deferred();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Keep the WM's geometry when it takes a fullscreen window out of fullscreen.
#[test]
#[serial]
fn wm_fullscreen_exit_leaves_stone_windowed_at_the_wm_geometry() {
    assert!(
        std::env::var("DISPLAY").is_ok(),
        "run through the private-display test wrapper"
    );
    std::env::set_var("SDL_VIDEODRIVER", "x11");
    std::env::remove_var("STONE_HEADLESS");
    std::env::set_var("STONE_IMMEDIATE_SHOW", "1");
    assert!(stone_init());
    struct Teardown;
    impl Drop for Teardown {
        fn drop(&mut self) {
            stone_teardown();
        }
    }
    let _teardown = Teardown;

    let title = CString::new("test-wm-fullscreen-exit").unwrap();
    let window = stone_create_window_ex(title.as_ptr(), 200, 150, 480, 360, -1, true, 0);
    assert!(!window.is_null());
    run_frames(20);

    for mode in [1, 2] {
        stone_set_fullscreen(mode);
        run_frames(20);
        assert_eq!(stone_get_fullscreen(), mode);

        let status = std::process::Command::new("i3-msg")
            .args([
                "-q",
                "[title=\"test-wm-fullscreen-exit\"] fullscreen disable, floating enable, resize set 500 400, move position 100 100",
            ])
            .status()
            .expect("i3-msg runs on the private display");
        assert!(status.success());
        run_frames(30);

        let (mut w, mut h, mut x, mut y) = (0, 0, 0, 0);
        stone_get_window_size(&mut w, &mut h);
        stone_get_window_position(&mut x, &mut y);
        println!(
            "after WM exit: fullscreen={} size={}x{} pos=({}, {})",
            stone_get_fullscreen(),
            w,
            h,
            x,
            y
        );
        assert_eq!(
            stone_get_fullscreen(),
            0,
            "the WM took the window out of fullscreen"
        );
        assert_eq!(
            (x, y),
            (100, 100),
            "Stone must report the WM's window position"
        );
        assert_eq!(
            (w, h),
            (500, 400),
            "the window keeps the size the WM gave it"
        );

        // A later windowed request is already satisfied and must preserve the WM's geometry.
        stone_set_fullscreen(0);
        run_frames(10);
        stone_get_window_size(&mut w, &mut h);
        stone_get_window_position(&mut x, &mut y);
        assert_eq!(((w, h), (x, y)), ((500, 400), (100, 100)));

        assert_eq!(stone_toggle_fullscreen(), 1);
        run_frames(10);
        assert_eq!(stone_toggle_fullscreen(), 0);
        run_frames(10);
        stone_get_window_size(&mut w, &mut h);
        stone_get_window_position(&mut x, &mut y);
        assert_eq!(
            ((w, h), (x, y)),
            ((500, 400), (100, 100)),
            "a later app-driven toggle restores the WM's windowed bounds"
        );
    }
}
