use std::ffi::{c_char, c_int, CStr};

use crate::handles::Class;
use crate::{
    compile_shader, link_program, track_shader_switch, with_gl, DebugGl, CURRENT_SHADER, SHADERS,
};

pub(crate) struct Shader {
    pub program: glow::Program,
    pub _vertex_shader: glow::Shader,
    pub _fragment_shader: glow::Shader,
}

/// Compile, link and register one shader. Every GL object created here is released
/// unless registration succeeds, so a failing stage never leaks the stages before it.
fn build_and_register(gl: &DebugGl, vert_src: &str, frag_src: &str) -> Option<u64> {
    unsafe {
        let vertex_shader = compile_shader(gl, glow::VERTEX_SHADER, vert_src)?;
        let Some(fragment_shader) = compile_shader(gl, glow::FRAGMENT_SHADER, frag_src) else {
            gl.delete_shader(vertex_shader);
            return None;
        };
        let Some(program) = link_program(gl, vertex_shader, fragment_shader) else {
            gl.delete_shader(vertex_shader);
            gl.delete_shader(fragment_shader);
            return None;
        };

        let id = crate::handles::mint(Class::Shader);
        SHADERS.with(|s| {
            s.borrow_mut().insert(
                id,
                Shader {
                    program,
                    _vertex_shader: vertex_shader,
                    _fragment_shader: fragment_shader,
                },
            );
        });
        Some(id)
    }
}

#[no_mangle]
pub extern "C" fn stone_create_shader_from_combined(src: *const c_char) -> u64 {
    if src.is_null() {
        return 0;
    }

    let combined = unsafe { CStr::from_ptr(src).to_string_lossy() };

    // Find where to insert #define after #version line
    let insert_point = if let Some(version_pos) = combined.find("#version") {
        combined[version_pos..]
            .find('\n')
            .map(|p| version_pos + p + 1)
            .unwrap_or(0)
    } else {
        0
    };

    let prefix = &combined[..insert_point];
    let suffix = &combined[insert_point..];

    let vert_src = format!("{}#define VERTEX\n{}", prefix, suffix);
    let frag_src = format!("{}#define PIXEL\n{}", prefix, suffix);

    with_gl(|gl| build_and_register(gl, &vert_src, &frag_src))
        .flatten()
        .unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn stone_destroy_shader(handle: u64) {
    if handle == 0 {
        return;
    }

    SHADERS.with(|s| {
        if let Some(shader) = s.borrow_mut().remove(&handle) {
            crate::handles::retire(Class::Shader, handle);
            with_gl(|gl| unsafe {
                crate::uniform_block::destroy_blocks_for_shader(handle, shader.program);
                // This owns the program's WHOLE resource set, not just the GL
                // objects created here. A rejected registration candidate is
                // destroyed mid-registration, so it may already have bound
                // blocks and allocated their buffers; leaving those behind
                // leaked one buffer per block per reload, and left a
                // program -> handle entry that a reused GL program id would
                // inherit, misattributing draws in the unfilled-block gate.
                gl.delete_program(shader.program);
                gl.delete_shader(shader._vertex_shader);
                gl.delete_shader(shader._fragment_shader);
                // The draw paths read CURRENT_SHADER for uniform lookups, so a
                // destroyed program left named there is a deleted program the
                // next draw would look up. Clearing only the matching one keeps
                // an unrelated bound program bound.
                CURRENT_SHADER.with(|c| {
                    let mut c = c.borrow_mut();
                    if *c == Some(shader.program) {
                        *c = None;
                    }
                });
            });
        }
    });
}

pub(crate) fn destroy_all() {
    let handles: Vec<u64> = SHADERS.with(|s| s.borrow().keys().copied().collect());
    for handle in handles {
        stone_destroy_shader(handle);
    }
}

#[no_mangle]
pub extern "C" fn stone_set_shader(handle: u64) {
    if handle == 0 {
        with_gl(|gl| unsafe {
            gl.use_program(None);
        });
        CURRENT_SHADER.with(|c| *c.borrow_mut() = None);
        track_shader_switch();
        return;
    }

    SHADERS.with(|s| {
        if let Some(shader) = s.borrow().get(&handle) {
            with_gl(|gl| unsafe {
                gl.use_program(Some(shader.program));
            });
            CURRENT_SHADER.with(|c| *c.borrow_mut() = Some(shader.program));
            track_shader_switch();
        }
    });
    // The uniform binding index is global state, so it follows the program
    // rather than being claimed once at registration.
    with_gl(|gl| crate::uniform_block::bind_block_for_program(gl, handle));
}

fn get_shader_program(handle: u64) -> Option<glow::Program> {
    SHADERS.with(|s| s.borrow().get(&handle).map(|shader| shader.program))
}

/// Point sampler `n` at texture `unit`: the link-time `@textures` slot binding
/// (`bind_texture_slots`) is the only by-name uniform write.
#[no_mangle]
pub extern "C" fn stone_shader_set_texture(h: u64, n: *const c_char, unit: c_int) {
    if h == 0 || n.is_null() {
        return;
    }
    let name = unsafe { CStr::from_ptr(n).to_string_lossy() };
    let Some(program) = get_shader_program(h) else {
        return;
    };
    with_gl(|gl| unsafe {
        if let Some(loc) = gl.get_uniform_location(program, &name) {
            gl.use_program(Some(program));
            gl.uniform_1_i32(Some(&loc), unit);
        }
    });
}

#[no_mangle]
pub extern "C" fn stone_bind_texture_to_unit(tex: u32, unit: c_int) -> c_int {
    if tex == 0 || unit < 0 {
        return 0;
    }
    with_gl(|gl| unsafe {
        gl.active_texture(glow::TEXTURE0 + unit as u32);
        if let Some(native_tex) = std::num::NonZeroU32::new(tex) {
            gl.bind_texture(glow::TEXTURE_2D, Some(glow::NativeTexture(native_tex)));
        }
        gl.active_texture(glow::TEXTURE0);
        1
    })
    .unwrap_or(0)
}
