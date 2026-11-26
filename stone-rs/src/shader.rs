use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{c_char, c_int, CStr};

use crate::handles::Class;
use crate::{
    compile_shader, link_program, track_shader_switch, with_gl, DebugGl, CURRENT_SHADER, SHADERS,
};

pub(crate) struct Shader {
    pub program: glow::Program,
    pub _vertex_shader: glow::Shader,
    pub _fragment_shader: glow::Shader,
    pub generation: u64,
    pub name: Option<String>,
    pub texture_slots: Vec<(i32, String)>,
}

thread_local! {
    static SHADER_BY_NAME: RefCell<HashMap<String, u64>> = RefCell::new(HashMap::new());
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
                    generation: crate::handles::next_generation(),
                    name: None,
                    texture_slots: Vec::new(),
                },
            );
        });
        Some(id)
    }
}

/// rock-ffi stone-rendering rename:src=combined_src
#[no_mangle]
pub extern "C" fn stone_create_shader_from_combined(src: *const c_char) -> u64 {
    if src.is_null() {
        return 0;
    }

    let combined = unsafe { CStr::from_ptr(src).to_string_lossy() };

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

/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_destroy_shader(handle: u64) {
    if handle == 0 {
        return;
    }

    SHADERS.with(|s| {
        if let Some(shader) = s.borrow_mut().remove(&handle) {
            if let Some(name) = shader.name.as_deref() {
                SHADER_BY_NAME.with(|by_name| {
                    let mut by_name = by_name.borrow_mut();
                    if by_name.get(name) == Some(&handle) {
                        by_name.remove(name);
                    }
                });
            }
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
    SHADER_BY_NAME.with(|by_name| by_name.borrow_mut().clear());
}

/// rock-ffi stone-rendering
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
    // Uniform binding indices are global, so blocks re-bind whenever the program changes.
    with_gl(|gl| crate::uniform_block::bind_block_for_program(gl, handle));
}

fn get_shader_program(handle: u64) -> Option<glow::Program> {
    SHADERS.with(|s| s.borrow().get(&handle).map(|shader| shader.program))
}

/// Point sampler `n` at texture `unit`: the link-time `@textures` slot binding
/// (`bind_texture_slots`) is the only by-name uniform write.
/// rock-ffi stone-rendering private rename:h=handle rename:n=name rename:unit=texture_unit
#[no_mangle]
pub extern "C" fn stone_shader_set_texture(h: u64, n: *const c_char, unit: c_int) {
    if h == 0 || n.is_null() {
        return;
    }
    let name = unsafe { CStr::from_ptr(n).to_string_lossy() }.into_owned();
    let Some(program) = get_shader_program(h) else {
        return;
    };
    SHADERS.with(|s| {
        if let Some(shader) = s.borrow_mut().get_mut(&h) {
            if let Some(slot) = shader
                .texture_slots
                .iter_mut()
                .find(|(_, existing)| existing == &name)
            {
                slot.0 = unit;
            } else {
                shader.texture_slots.push((unit, name.clone()));
            }
        }
    });
    with_gl(|gl| unsafe {
        if let Some(loc) = gl.get_uniform_location(program, &name) {
            let previous =
                std::num::NonZeroU32::new(gl.get_parameter_i32(glow::CURRENT_PROGRAM) as u32)
                    .map(glow::NativeProgram);
            gl.use_program(Some(program));
            gl.uniform_1_i32(Some(&loc), unit);
            gl.use_program(previous);
        }
    });
}

/// Associate a live shader handle with its registry name so native libraries
/// can resolve it with `stone_native_find_shader`.
/// rock-ffi stone-rendering private
#[no_mangle]
pub extern "C" fn stone_shader_register_name(handle: u64, name: *const c_char) -> bool {
    if handle == 0 || name.is_null() {
        return false;
    }
    let Ok(name) = unsafe { CStr::from_ptr(name) }.to_str() else {
        return false;
    };
    if name.is_empty() {
        return false;
    }
    register_shader_name(handle, name, true)
}

pub(crate) fn register_shader_name_if_unset(handle: u64, name: &str) {
    if handle != 0 && !name.is_empty() {
        let _ = register_shader_name(handle, name, false);
    }
}

fn register_shader_name(handle: u64, name: &str, overwrite: bool) -> bool {
    SHADERS.with(|s| {
        let mut map = s.borrow_mut();
        let Some(shader) = map.get_mut(&handle) else {
            return false;
        };
        if !overwrite && shader.name.is_some() {
            return true;
        }
        if let Some(prev) = shader.name.as_deref() {
            if prev != name {
                SHADER_BY_NAME.with(|by_name| {
                    let mut by_name = by_name.borrow_mut();
                    if by_name.get(prev) == Some(&handle) {
                        by_name.remove(prev);
                    }
                });
            }
        }
        shader.name = Some(name.to_string());
        SHADER_BY_NAME.with(|by_name| {
            by_name.borrow_mut().insert(name.to_string(), handle);
        });
        true
    })
}

pub(crate) fn find_shader_by_name(name: &str) -> u64 {
    let Some(handle) = SHADER_BY_NAME.with(|by_name| by_name.borrow().get(name).copied()) else {
        return 0;
    };
    SHADERS.with(|s| {
        if s.borrow().contains_key(&handle) {
            handle
        } else {
            0
        }
    })
}

pub(crate) fn shader_generation(handle: u64) -> u64 {
    if handle == 0 {
        return 0;
    }
    SHADERS.with(|s| {
        s.borrow()
            .get(&handle)
            .map_or(0, |shader| shader.generation)
    })
}

pub(crate) fn shader_texture_slot_count(handle: u64) -> Option<usize> {
    if handle == 0 {
        return None;
    }
    SHADERS.with(|s| {
        s.borrow()
            .get(&handle)
            .map(|shader| shader.texture_slots.len())
    })
}

pub(crate) fn shader_texture_slot_find(handle: u64, name: &str) -> i32 {
    if handle == 0 {
        return -1;
    }
    SHADERS.with(|s| {
        s.borrow()
            .get(&handle)
            .and_then(|shader| {
                shader
                    .texture_slots
                    .iter()
                    .find(|(_, slot_name)| slot_name == name)
                    .map(|(unit, _)| *unit)
            })
            .unwrap_or(-1)
    })
}

pub(crate) fn shader_texture_slot_at(handle: u64, index: usize) -> Option<(i32, String)> {
    if handle == 0 {
        return None;
    }
    SHADERS.with(|s| {
        s.borrow()
            .get(&handle)
            .and_then(|shader| shader.texture_slots.get(index).cloned())
    })
}

pub(crate) fn stone_bind_texture_to_unit(tex: u32, unit: c_int) -> c_int {
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
