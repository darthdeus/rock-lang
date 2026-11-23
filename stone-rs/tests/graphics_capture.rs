// SDL's Cocoa driver needs the process main thread.
fn main() {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let args: Vec<_> = std::env::args_os().skip(1).collect();
        if args.iter().any(|arg| arg == "--ignored") {
            return;
        }
        if args.iter().any(|arg| arg == "--list") {
            println!("captures_sampler_and_numerical_canvas_snapshots: test");
            return;
        }
        native::run();
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod native {
    use serde_json::Value;
    use std::ffi::{c_char, CStr, CString};
    use std::num::NonZeroU32;
    use stone_gl::DebugGl;
    use stone_rs::{stone_create_window_ex, stone_gl_error_count, stone_init, stone_teardown};

    extern "C" {
        fn stone_create_canvas(width: i32, height: i32, format: i32, with_depth: bool) -> u64;
        fn stone_set_canvas(handle: u64);
        fn stone_graphics_capture_begin(directory: *const c_char, frame: i32);
        fn stone_graphics_capture_scene_depth(handle: u64);
        fn stone_graphics_capture_finish_json() -> *const c_char;
    }

    pub fn run() {
        std::env::set_var("STONE_HEADLESS", "1");
        #[cfg(target_os = "linux")]
        std::env::set_var("SDL_VIDEODRIVER", "offscreen");
        #[cfg(target_os = "macos")]
        std::env::set_var("SDL_VIDEODRIVER", "cocoa");
        assert!(stone_init());
        let window = stone_create_window_ex(c"capture".as_ptr(), 0, 0, 32, 32, -1, true, 0);
        assert!(!window.is_null());
        let gl = unsafe {
            DebugGl::new(glow::Context::from_loader_function(|name| {
                let name = CString::new(name).unwrap();
                sdl2::sys::SDL_GL_GetProcAddress(name.as_ptr()).cast()
            }))
        };
        let dir = tempfile::tempdir().unwrap();
        let directory = CString::new(dir.path().as_os_str().as_encoded_bytes()).unwrap();
        let (target, color, depth, sampler, errors_before) = unsafe {
            let target = stone_create_canvas(3, 2, 1, true);
            assert_ne!(target, 0);
            stone_set_canvas(target);
            let attachment_texture = |attachment| {
                let id = gl.get_framebuffer_attachment_parameter_i32(
                    glow::FRAMEBUFFER,
                    attachment,
                    glow::FRAMEBUFFER_ATTACHMENT_OBJECT_NAME,
                );
                glow::NativeTexture(NonZeroU32::new(id as u32).unwrap())
            };
            let color = attachment_texture(glow::COLOR_ATTACHMENT0);
            let depth = attachment_texture(glow::DEPTH_ATTACHMENT);
            gl.clear_color(0.25, 0.5, 0.75, 1.0);
            gl.depth_mask(true);
            gl.clear_depth_f32(0.25);
            gl.clear(glow::COLOR_BUFFER_BIT | glow::DEPTH_BUFFER_BIT);
            stone_set_canvas(0);

            let vertex = stone_gl::compile_shader(
                &gl,
                glow::VERTEX_SHADER,
                "#version 330 core\nvoid main() { gl_Position = vec4(0.0); }",
            )
            .unwrap();
            let fragment = stone_gl::compile_shader(
                &gl,
                glow::FRAGMENT_SHADER,
                "#version 330 core\nuniform sampler2D source; out vec4 color;\nvoid main() { color = texture(source, vec2(0.5)); }",
            )
            .unwrap();
            let program = stone_gl::link_program(&gl, vertex, fragment).unwrap();
            gl.use_program(Some(program));
            gl.uniform_1_i32(gl.get_uniform_location(program, "source").as_ref(), 3);
            gl.bind_vertex_array(Some(gl.create_vertex_array().unwrap()));

            let sampler = gl.create_sampler().unwrap();
            gl.sampler_parameter_i32(sampler, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
            gl.bind_sampler(3, Some(sampler));
            gl.active_texture(glow::TEXTURE0 + 3);
            gl.bind_texture(glow::TEXTURE_2D, Some(color));
            gl.active_texture(glow::TEXTURE0 + 5);
            let pack = gl.create_buffer().unwrap();
            gl.bind_buffer(glow::PIXEL_PACK_BUFFER, Some(pack));
            gl.pixel_store_i32(glow::PACK_ALIGNMENT, 8);
            gl.pixel_store_i32(glow::PACK_ROW_LENGTH, 11);
            (target, color, depth, sampler, stone_gl_error_count())
        };

        unsafe { stone_graphics_capture_begin(directory.as_ptr(), 1) };
        unsafe {
            let state = || {
                [
                    glow::ACTIVE_TEXTURE,
                    glow::TEXTURE_BINDING_2D,
                    glow::PIXEL_PACK_BUFFER_BINDING,
                    glow::PACK_ALIGNMENT,
                    glow::PACK_ROW_LENGTH,
                    glow::READ_FRAMEBUFFER_BINDING,
                    glow::DRAW_FRAMEBUFFER_BINDING,
                ]
                .map(|parameter| gl.get_parameter_i32(parameter))
            };
            let before = state();
            gl.draw_arrays(glow::TRIANGLES, 0, 0);
            assert_eq!(state(), before);
            stone_graphics_capture_scene_depth(target);
            assert_eq!(state(), before);
            assert_eq!(gl.get_error(), glow::NO_ERROR);
        }
        let capture: Value = serde_json::from_slice(
            unsafe { CStr::from_ptr(stone_graphics_capture_finish_json()) }.to_bytes(),
        )
        .unwrap();

        eprintln!("graphics capture on {}", capture["gpu"]);
        assert_eq!(capture["status"], "ok");
        for (texture, channels, expected) in [
            (color, 4, &[0.25, 0.5, 0.75, 1.0][..]),
            (depth, 1, &[0.25][..]),
        ] {
            let entry = capture["snapshots"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["texture"] == texture.0.get())
                .expect("canvas texture snapshot");
            let snapshot = &entry["snapshot"];
            assert_eq!(snapshot["status"], "ok");
            assert_eq!(snapshot["channels"], channels);
            assert_eq!(snapshot["scalar"], "float32");
            let bytes = std::fs::read(snapshot["path"].as_str().unwrap()).unwrap();
            assert_eq!(bytes.len(), 3 * 2 * channels * 4);
            for (i, value) in bytes.as_chunks::<4>().0.iter().enumerate() {
                assert!((f32::from_ne_bytes(*value) - expected[i % channels]).abs() < 0.002);
            }
        }
        assert_eq!(stone_gl_error_count(), errors_before);
        let draw = &capture["operations"][0];
        assert_eq!(draw["state_restored"], true);
        let binding = draw["textures"]
            .as_array()
            .unwrap()
            .iter()
            .find(|binding| binding["unit"] == 3)
            .unwrap();
        assert_eq!(binding["sampled"], true);
        assert_eq!(binding["sampler"], sampler.0.get());
        assert_eq!(binding["min_filter"], glow::LINEAR);
        assert!(stone_teardown());
    }
}
