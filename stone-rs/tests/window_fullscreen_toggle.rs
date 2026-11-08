//! Tests for Alt+Enter fullscreen toggling, repeat suppression, and bounds restoration.

#![cfg(target_os = "linux")]

use serial_test::serial;
use std::ffi::CString;
use stone_rs::{
    stone_create_window_ex, stone_get_fullscreen, stone_get_window_position, stone_get_window_size,
    stone_init, stone_poll_event, stone_teardown, stone_toggle_fullscreen, StoneEvent,
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
