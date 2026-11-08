use x11::xlib;

pub struct X11Window {
    pub display: *mut xlib::Display,
    pub window: xlib::Window,
}

/// The X11 display and XID of Stone's window, read from the SDL window that
/// owns the current GL context (Stone makes it current on the creating
/// thread).
pub fn current() -> X11Window {
    use sdl2::sys;
    unsafe {
        let sdl_window = sys::SDL_GL_GetCurrentWindow();
        assert!(!sdl_window.is_null(), "Stone's GL window is current");
        let mut info: sys::SDL_SysWMinfo = std::mem::zeroed();
        sys::SDL_GetVersion(&mut info.version);
        assert!(sys::SDL_GetWindowWMInfo(sdl_window, &mut info) == sys::SDL_bool::SDL_TRUE);
        assert!(
            info.subsystem == sys::SDL_SYSWM_TYPE::SDL_SYSWM_X11,
            "the window is an X11 window"
        );
        let display = info.info.x11.display.cast::<xlib::Display>();
        assert!(!display.is_null());
        X11Window {
            display,
            window: info.info.x11.window as xlib::Window,
        }
    }
}
