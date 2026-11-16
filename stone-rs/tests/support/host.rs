use std::ffi::CString;
use std::ffi::OsString;
use stone_rs::{stone_gpu_host_acquire, stone_init, stone_teardown};

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
            log::error!("GL host fixture native cleanup failed");
            if !std::thread::panicking() {
                panic!("GL host fixture native cleanup failed");
            }
        }
    }
}

pub fn with_host(body: impl FnOnce(&glow::Context)) {
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
