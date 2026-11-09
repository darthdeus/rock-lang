//! Sampled draws must preserve pixel filters while improving minified materials.
#![cfg(target_os = "linux")]

use glow::HasContext;
use std::ffi::{CString, OsString};
use stone_rs::{stone_gpu_host_acquire, stone_init, stone_teardown};

extern "C" {
    fn stone_bind_sampler(unit: i32, mode: i32, image: u64);
}

struct Host(Option<OsString>);
impl Drop for Host {
    fn drop(&mut self) {
        stone_teardown();
        match &self.0 {
            Some(value) => std::env::set_var("STONE_HEADLESS", value),
            None => std::env::remove_var("STONE_HEADLESS"),
        }
    }
}

#[test]
fn only_mipmapped_samplers_use_bounded_anisotropy() {
    let _host = Host(std::env::var_os("STONE_HEADLESS"));
    std::env::set_var("STONE_HEADLESS", "1");
    assert!(stone_init());
    assert!(!stone_gpu_host_acquire().is_null());
    let gl = unsafe {
        glow::Context::from_loader_function(|name| {
            sdl2::sys::SDL_GL_GetProcAddress(CString::new(name).unwrap().as_ptr()).cast()
        })
    };
    let supported = gl
        .supported_extensions()
        .contains("GL_EXT_texture_filter_anisotropic")
        || gl
            .supported_extensions()
            .contains("GL_ARB_texture_filter_anisotropic")
        || (gl.version().major, gl.version().minor) >= (4, 6);
    unsafe {
        for mode in 0..3 {
            stone_bind_sampler(0, mode, 0);
            let id = gl.get_parameter_i32(glow::SAMPLER_BINDING) as u32;
            let sampler = glow::NativeSampler(std::num::NonZeroU32::new(id).unwrap());
            if supported {
                let expected = if mode == 2 {
                    gl.get_parameter_f32(glow::MAX_TEXTURE_MAX_ANISOTROPY)
                        .min(4.0)
                } else {
                    1.0
                };
                assert_eq!(
                    gl.get_sampler_parameter_f32(sampler, glow::TEXTURE_MAX_ANISOTROPY),
                    expected
                );
            }
            assert_eq!(gl.get_error(), glow::NO_ERROR);
        }
        stone_bind_sampler(0, -1, 0);
    }
}
