// Retained mesh objects -- the ONE GPU-side geometry primitive, with optional
// instancing. A mesh owns a VAO plus
// a STATIC_DRAW vertex VBO; an optional instance VBO continues the
// sequential attribute-location numbering with divisor 1. Contrast with
// stone_draw_mesh (sprite.rs): that path re-uploads STREAM_DRAW every call
// with the caller-supplied attribute layout.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{c_float, c_int};

use crate::handles::Class;
use crate::{track_draw_call, track_instanced_draw, with_gl, CURRENT_SHADER};

struct RetainedMesh {
    vao: glow::VertexArray,
    vbo: glow::Buffer,
    inst_vbo: Option<glow::Buffer>,
    gl_mode: u32,
    vert_count: i32,
    // Base attrib count = the first instance attrib's location.
    attr_count: i32,
    inst_count: i32,
    // Instance attrib locations in use, so detaching can put the VAO back.
    inst_attr_count: i32,
    // Bytes uploaded to each buffer, recorded at upload: the layout that
    // sizes them is not retained, so the census reads these, not a formula.
    vbo_bytes: u64,
    inst_vbo_bytes: u64,
}

thread_local! {
    static MESHES: RefCell<HashMap<u64, RetainedMesh>> = RefCell::new(HashMap::new());
}

/// Validated attr layout: sizes each 1..=4, sum == stride. None = reject
/// (logged); never falls back.
pub(crate) struct ValidatedLayout {
    sizes: [i32; 16],
    len: usize,
}

impl ValidatedLayout {
    pub(crate) fn as_slice(&self) -> &[i32] {
        &self.sizes[..self.len]
    }
}

pub(crate) fn validate_layout(
    what: &str,
    stride_floats: c_int,
    attr_sizes: *const c_int,
    attr_count: c_int,
) -> Option<ValidatedLayout> {
    if attr_sizes.is_null()
        || attr_count <= 0
        || attr_count > 16
        || stride_floats <= 0
        || stride_floats > 64
    {
        crate::log_once!(
            "MESH",
            "{}: invalid layout (attr_count {}, stride {})",
            what,
            attr_count,
            stride_floats
        );
        return None;
    }
    let sizes = unsafe { std::slice::from_raw_parts(attr_sizes, attr_count as usize) };
    if sizes.iter().any(|&s| !(1..=4).contains(&s)) {
        crate::log_once!(
            "MESH",
            "{}: attr sizes must be 1..=4, got {:?}",
            what,
            sizes
        );
        return None;
    }
    let sum: i32 = sizes.iter().sum();
    if sum != stride_floats {
        crate::log_once!(
            "MESH",
            "{}: attr sizes {:?} sum to {} but stride is {}",
            what,
            sizes,
            sum,
            stride_floats
        );
        return None;
    }
    let mut validated = ValidatedLayout {
        sizes: [0; 16],
        len: sizes.len(),
    };
    validated.sizes[..sizes.len()].copy_from_slice(sizes);
    Some(validated)
}

/// Set up float attrib pointers at sequential locations starting at
/// `first_loc` for the currently bound VBO. Returns the location after the
/// last one configured.
pub(crate) unsafe fn setup_attribs(
    gl: &crate::DebugGl,
    first_loc: u32,
    sizes: &[i32],
    stride_floats: i32,
    divisor: u32,
    base_offset: i32,
) -> u32 {
    let stride_bytes = stride_floats * std::mem::size_of::<f32>() as i32;
    let mut loc = first_loc;
    let mut offset_floats = 0i32;
    for &size in sizes {
        gl.enable_vertex_attrib_array(loc);
        gl.vertex_attrib_pointer_f32(
            loc,
            size,
            glow::FLOAT,
            false,
            stride_bytes,
            base_offset + offset_floats * std::mem::size_of::<f32>() as i32,
        );
        if divisor != 0 {
            gl.vertex_attrib_divisor(loc, divisor);
        }
        loc += 1;
        offset_floats += size;
    }
    loc
}

/// mode: 0 = triangle_fan, 1 = triangles. Returns 0 on any invalid input
/// (logged) -- reject, don't fall back.
#[no_mangle]
pub extern "C" fn stone_mesh_create(
    mode: c_int,
    verts: *const c_float,
    vert_count: c_int,
    stride_floats: c_int,
    attr_sizes: *const c_int,
    attr_count: c_int,
) -> u64 {
    let gl_mode = match mode {
        0 => glow::TRIANGLE_FAN,
        1 => glow::TRIANGLES,
        _ => {
            crate::log_once!("MESH", "stone_mesh_create: unknown mode {}", mode);
            return 0;
        }
    };
    if verts.is_null() || vert_count <= 0 || vert_count > 1_000_000 {
        crate::log_once!(
            "MESH",
            "stone_mesh_create: invalid verts ({} verts)",
            vert_count
        );
        return 0;
    }
    let Some(sizes) = validate_layout("stone_mesh_create", stride_floats, attr_sizes, attr_count)
    else {
        return 0;
    };

    with_gl(|gl| unsafe {
        let (vao, vbo) = match (gl.create_vertex_array(), gl.create_buffer()) {
            (Ok(vao), Ok(vbo)) => (vao, vbo),
            (vao, vbo) => {
                crate::log_once!(
                    "MESH",
                    "stone_mesh_create: GL object creation failed ({:?}, {:?})",
                    vao.err(),
                    vbo.err()
                );
                return 0;
            }
        };
        gl.bind_vertex_array(Some(vao));
        gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
        let byte_len = vert_count as usize * stride_floats as usize * std::mem::size_of::<f32>();
        let bytes = std::slice::from_raw_parts(verts as *const u8, byte_len);
        if gl
            .buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes, glow::STATIC_DRAW)
            .is_err()
        {
            gl.bind_vertex_array(None);
            gl.delete_buffer(vbo);
            gl.delete_vertex_array(vao);
            return 0;
        }
        crate::track_retained_upload(vert_count as usize, byte_len);
        setup_attribs(gl, 0, sizes.as_slice(), stride_floats, 0, 0);
        gl.bind_vertex_array(None);

        let id = crate::handles::mint(Class::Mesh);
        MESHES.with(|m| {
            m.borrow_mut().insert(
                id,
                RetainedMesh {
                    vao,
                    vbo,
                    inst_vbo: None,
                    gl_mode,
                    vert_count,
                    attr_count,
                    inst_count: 0,
                    inst_attr_count: 0,
                    vbo_bytes: byte_len as u64,
                    inst_vbo_bytes: 0,
                },
            )
        });
        id
    })
    .unwrap_or_else(|| {
        crate::log_once!("MESH", "stone_mesh_create: no GL context");
        0
    })
}

/// Attach per-instance data (attribs continue after the base attribs,
/// divisor 1). count 0 is valid (draws nothing). Calling twice on one mesh
/// is a reject at v0 -- no re-upload semantics exist yet.
#[no_mangle]
pub extern "C" fn stone_mesh_set_instances(
    handle: u64,
    data: *const c_float,
    count: c_int,
    stride_floats: c_int,
    attr_sizes: *const c_int,
    attr_count: c_int,
) {
    if data.is_null() || count < 0 || count > 10_000_000 {
        crate::log_once!(
            "MESH",
            "stone_mesh_set_instances: invalid data ({} instances)",
            count
        );
        return;
    }
    let Some(sizes) = validate_layout(
        "stone_mesh_set_instances",
        stride_floats,
        attr_sizes,
        attr_count,
    ) else {
        return;
    };

    MESHES.with(|m| {
        let mut meshes = m.borrow_mut();
        let Some(mesh) = meshes.get_mut(&handle) else {
            crate::log_once!(
                "MESH",
                "stone_mesh_set_instances: unknown mesh handle {}",
                handle
            );
            return;
        };
        if mesh.inst_vbo.is_some() {
            crate::log_once!(
                "MESH",
                "stone_mesh_set_instances: instances already set on mesh {} (no re-upload in v0)",
                handle
            );
            return;
        }
        if mesh.attr_count + attr_count > 16 {
            crate::log_once!(
                "MESH",
                "stone_mesh_set_instances: {} base + {} instance attribs exceeds 16 locations",
                mesh.attr_count,
                attr_count
            );
            return;
        }
        with_gl(|gl| unsafe {
            let inst_vbo = match gl.create_buffer() {
                Ok(b) => b,
                Err(e) => {
                    crate::log_once!(
                        "MESH",
                        "stone_mesh_set_instances: buffer creation failed ({})",
                        e
                    );
                    return;
                }
            };
            gl.bind_vertex_array(Some(mesh.vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(inst_vbo));
            let byte_len = count as usize * stride_floats as usize * std::mem::size_of::<f32>();
            let bytes = std::slice::from_raw_parts(data as *const u8, byte_len);
            if gl
                .buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes, glow::STATIC_DRAW)
                .is_err()
            {
                gl.bind_vertex_array(None);
                gl.delete_buffer(inst_vbo);
                return;
            }
            crate::track_retained_upload(0, byte_len);
            setup_attribs(
                gl,
                mesh.attr_count as u32,
                sizes.as_slice(),
                stride_floats,
                1,
                0,
            );
            gl.bind_vertex_array(None);
            mesh.inst_vbo = Some(inst_vbo);
            mesh.inst_count = count;
            mesh.inst_attr_count = attr_count;
            mesh.inst_vbo_bytes = byte_len as u64;
        });
    });
}

/// Draws under CURRENT_SHADER; a mesh's custom attribute layout has no
/// meaning under the default sprite shader, so no bound shader is a reject,
/// not a fallback. tex 0 = untextured (no uTexture bind). count < 0 = all
/// instances; ignored for non-instanced meshes.
///
/// Returns -1 when no draw was issued, 0 when the draw had no resolved
/// texture, and 1 when the texture was bound to unit 0 before it.
#[no_mangle]
pub extern "C" fn stone_mesh_draw(handle: u64, tex: u64, count: c_int) -> c_int {
    MESHES.with(|m| {
        let meshes = m.borrow();
        let Some(mesh) = meshes.get(&handle) else {
            crate::log_once!("MESH", "stone_mesh_draw: unknown mesh handle {}", handle);
            return -1;
        };
        with_gl(|gl| unsafe {
            let Some(program) = CURRENT_SHADER.with(|c| *c.borrow()) else {
                crate::log_once!(
                    "MESH",
                    "stone_mesh_draw: no shader bound (wrap the draw in with_shader)"
                );
                return -1;
            };
            gl.use_program(Some(program));

            let mut texture_bound = false;
            if tex != 0 {
                let tex_id = crate::image_texture::sample_image_texture_for_bind(tex);
                let Some(tex_id) = tex_id else {
                    crate::log_once!(
                        "MESH",
                        "stone_mesh_draw: image {} is absent or unpublished",
                        tex
                    );
                    return -1;
                };
                // The mesh's own texture is unit 0. The sampler is NOT pointed
                // there by name here: a `@textures` group assigns every unit at
                // link time, and re-pointing one sampler per draw would silently
                // override a slot the compiler chose. A shader without a group
                // needs no assignment either -- an unset sampler already reads
                // unit 0.
                gl.active_texture(glow::TEXTURE0);
                if let Some(native_tex) = std::num::NonZeroU32::new(tex_id) {
                    gl.bind_texture(glow::TEXTURE_2D, Some(glow::NativeTexture(native_tex)));
                    texture_bound = true;
                }
            }

            gl.bind_vertex_array(Some(mesh.vao));
            // An instanced mesh with zero live instances issues nothing, so it
            // is a skip rather than an untextured draw -- a caller auditing its
            // texture slots owes nothing for a draw that did not happen.
            let mut drew = true;
            if mesh.inst_vbo.is_some() {
                let n = if count < 0 {
                    mesh.inst_count
                } else {
                    count.min(mesh.inst_count)
                };
                if n > 0 {
                    gl.draw_arrays_instanced(mesh.gl_mode, 0, mesh.vert_count, n);
                    crate::uniform_block::mark_program_drawn(program);
                    track_instanced_draw();
                    track_draw_call();
                } else {
                    drew = false;
                }
            } else {
                gl.draw_arrays(mesh.gl_mode, 0, mesh.vert_count);
                crate::uniform_block::mark_program_drawn(program);
                track_draw_call();
            }
            gl.bind_vertex_array(None);
            if !drew {
                -1
            } else if texture_bound {
                1
            } else {
                0
            }
        })
        .unwrap_or(-1)
    })
}

/// Live retained-mesh handles (`native_ledger`).
pub(crate) fn live_mesh_ids() -> Vec<u64> {
    MESHES.with(|m| m.borrow().keys().copied().collect())
}

/// Instances attached to a mesh: -2 for an unknown handle, -1 for a mesh with
/// no instance buffer, otherwise the count.
///
/// Three states because `set_instances` accepts a count of 0, so "never
/// latched" and "latched with nothing in it" are different mesh states that a
/// two-state answer would merge.
#[no_mangle]
pub extern "C" fn stone_mesh_instance_count(handle: u64) -> c_int {
    MESHES.with(|m| match m.borrow().get(&handle) {
        Some(mesh) if mesh.inst_vbo.is_some() => mesh.inst_count,
        Some(_) => -1,
        None => -2,
    })
}

/// Asset-report census over the retained-mesh registry: population, one
/// handle per index in id order (the `stone_get_live_image_handle` shape),
/// and the uploaded bytes a handle holds. 0 for an unknown handle: it holds
/// no buffer, so there is nothing to count.
#[no_mangle]
pub extern "C" fn stone_get_live_mesh_count() -> c_int {
    MESHES.with(|m| m.borrow().len().min(c_int::MAX as usize) as c_int)
}

#[no_mangle]
pub extern "C" fn stone_get_live_mesh_handle(index: c_int) -> u64 {
    if index < 0 {
        return 0;
    }
    MESHES.with(|m| {
        let mut ids: Vec<u64> = m.borrow().keys().copied().collect();
        ids.sort_unstable();
        ids.get(index as usize).copied().unwrap_or(0)
    })
}

#[no_mangle]
pub extern "C" fn stone_mesh_bytes(handle: u64) -> u64 {
    MESHES.with(|m| {
        m.borrow()
            .get(&handle)
            .map_or(0, |mesh| mesh.vbo_bytes + mesh.inst_vbo_bytes)
    })
}

#[no_mangle]
pub extern "C" fn stone_mesh_free(handle: u64) {
    if handle == 0 {
        return;
    }
    let Some(mesh) = MESHES.with(|m| m.borrow_mut().remove(&handle)) else {
        crate::log_once!("MESH", "stone_mesh_free: unknown mesh handle {}", handle);
        return;
    };
    crate::handles::retire(Class::Mesh, handle);
    with_gl(|gl| unsafe {
        gl.delete_vertex_array(mesh.vao);
        gl.delete_buffer(mesh.vbo);
        if let Some(inst_vbo) = mesh.inst_vbo {
            gl.delete_buffer(inst_vbo);
        }
    });
}

/// Teardown hook: the GL context dies with the window, so just drop the map.
pub(crate) fn clear_all() {
    MESHES.with(|m| {
        let mut m = m.borrow_mut();
        for handle in m.keys() {
            crate::handles::retire(Class::Mesh, *handle);
        }
        m.clear();
    });
}

#[cfg(test)]
mod layout_tests {
    use super::validate_layout;

    #[test]
    fn validate_layout_does_not_allocate_for_maximum_attribute_count() {
        let attributes = [1, 2, 3, 4, 1, 2, 3, 4, 1, 2, 3, 4, 1, 2, 3, 4];
        let live_before = crate::text::retained_bytes::live();
        let layout = validate_layout("test", 40, attributes.as_ptr(), attributes.len() as i32)
            .expect("valid layout");
        let live_after = crate::text::retained_bytes::live();

        assert_eq!(
            live_after, live_before,
            "validation retains no heap allocation"
        );
        drop(layout);
    }
}
