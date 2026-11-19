use std::cell::RefCell;
use std::ffi::{c_float, c_int};

use crate::{track_draw_call, with_gl, DebugGl, CURRENT_SHADER};

struct SpriteState {
    vao: glow::VertexArray,
    vbo: glow::Buffer,
    // Mesh VAO for stone_draw_mesh. Attribs are configured per draw from the
    // layout the caller passes, pointing into the vertex stream's range for
    // that draw (stream_buffer.rs); this VAO holds none of its own.
    mesh_vao: glow::VertexArray,
    triangle_indices: Vec<u32>,
    // present: NDC top -> v=0, the one flip onto the bottom-up window. identity: NDC top ->
    // v=1, so canvas-to-canvas hops preserve content.
    fs_quad_present_vao: glow::VertexArray,
    fs_quad_present_vbo: glow::Buffer,
    fs_quad_identity_vao: glow::VertexArray,
    fs_quad_identity_vbo: glow::Buffer,
    fs_quad_scaled_vao: glow::VertexArray,
    fs_quad_scaled_vbo: glow::Buffer,
}

thread_local! {
    static SPRITE_STATE: RefCell<Option<SpriteState>> = const { RefCell::new(None) };
}

// Each upload starts a fresh GL_TRIANGLES draw. Never split a primitive.
const MAX_STREAM_DRAW_VERTICES: usize = 999_999;

fn stream_draw_chunks(total: usize) -> impl Iterator<Item = (usize, usize)> {
    (0..total)
        .step_by(MAX_STREAM_DRAW_VERTICES)
        .map(move |offset| (offset, (total - offset).min(MAX_STREAM_DRAW_VERTICES)))
}

fn build_fan_indices(
    vertices: usize,
    ranges: &[i32],
    indices: &mut Vec<u32>,
) -> Result<(), &'static str> {
    if !ranges.len().is_multiple_of(2) {
        return Err("incomplete fan range");
    }
    let mut cursor = 0;
    let mut count = vertices;
    for range in ranges.as_chunks::<2>().0 {
        if range[0] < 0 || range[1] <= 0 {
            return Err("invalid fan range");
        }
        let first = range[0] as usize;
        let segments = range[1] as usize;
        let end = first
            .checked_add(segments + 1)
            .ok_or("fan range overflow")?;
        if first < cursor || end > vertices || !(first - cursor).is_multiple_of(3) {
            return Err("fan range overlaps or leaves an incomplete triangle");
        }
        count = count
            .checked_add(segments * 2 - 1)
            .ok_or("index count overflow")?;
        cursor = end;
    }
    if !(vertices - cursor).is_multiple_of(3) {
        return Err("incomplete triangle after fan");
    }
    indices.clear();
    indices.reserve(count);
    cursor = 0;
    for range in ranges.as_chunks::<2>().0 {
        let first = range[0] as usize;
        let segments = range[1] as usize;
        indices.extend((cursor..first).map(|i| i as u32));
        for i in 0..segments {
            let next = if i + 1 == segments { 0 } else { i + 1 };
            indices.extend_from_slice(&[
                first as u32,
                (first + 1 + i) as u32,
                (first + 1 + next) as u32,
            ]);
        }
        cursor = first + segments + 1;
    }
    indices.extend((cursor..vertices).map(|i| i as u32));
    Ok(())
}

pub(crate) fn initialized() -> bool {
    SPRITE_STATE.with(|state| state.borrow().is_some())
}

// Upload one static fullscreen quad (pos(2)+uv(2), 6 verts) and return its
// VAO/VBO. The two callers differ only in the UV column (present vs identity).
// None on GL failure, with the error logged; a panic here would abort across FFI.
unsafe fn upload_fs_quad(
    gl: &DebugGl,
    data: &[f32; 24],
) -> Option<(glow::VertexArray, glow::Buffer)> {
    let vao = match gl.create_vertex_array() {
        Ok(vao) => vao,
        Err(e) => {
            ::log::error!(target: "STONE", "stone_sprite_init: failed to create fs-quad VAO: {e}");
            return None;
        }
    };
    let vbo = match gl.create_buffer() {
        Ok(vbo) => vbo,
        Err(e) => {
            ::log::error!(target: "STONE", "stone_sprite_init: failed to create fs-quad VBO: {e}");
            gl.delete_vertex_array(vao);
            return None;
        }
    };
    gl.bind_vertex_array(Some(vao));
    gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
    if gl
        .buffer_data_u8_slice(
            glow::ARRAY_BUFFER,
            bytemuck::cast_slice(data),
            glow::STATIC_DRAW,
        )
        .is_err()
    {
        gl.bind_vertex_array(None);
        gl.delete_buffer(vbo);
        gl.delete_vertex_array(vao);
        return None;
    }
    let stride = 4 * std::mem::size_of::<f32>() as i32;
    gl.enable_vertex_attrib_array(0);
    gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, stride, 0);
    gl.enable_vertex_attrib_array(1);
    gl.vertex_attrib_pointer_f32(
        1,
        2,
        glow::FLOAT,
        false,
        stride,
        2 * std::mem::size_of::<f32>() as i32,
    );
    gl.bind_vertex_array(None);
    Some((vao, vbo))
}

unsafe fn stream_vertices(gl: &DebugGl, data: &[u8]) -> Result<(), String> {
    // Replace storage so queued draws keep their vertices.
    gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, data, glow::STREAM_DRAW)
}

#[no_mangle]
pub extern "C" fn stone_sprite_init() -> bool {
    if initialized() {
        return with_gl(|_| true).unwrap_or(false);
    }
    with_gl(|gl| unsafe { init_geometry(gl) }).unwrap_or(false)
}

unsafe fn init_geometry(gl: &crate::DebugGl) -> bool {
    let vao = match gl.create_vertex_array() {
        Ok(vao) => vao,
        Err(e) => {
            ::log::error!(target: "STONE", "stone_sprite_init: failed to create VAO: {e}");
            return false;
        }
    };
    let vbo = match gl.create_buffer() {
        Ok(vbo) => vbo,
        Err(e) => {
            ::log::error!(target: "STONE", "stone_sprite_init: failed to create VBO: {e}");
            gl.delete_vertex_array(vao);
            return false;
        }
    };

    gl.bind_vertex_array(Some(vao));
    gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));

    let empty_data: [f32; 48] = [0.0; 48];
    if gl
        .buffer_data_u8_slice(
            glow::ARRAY_BUFFER,
            bytemuck::cast_slice(&empty_data),
            glow::DYNAMIC_DRAW,
        )
        .is_err()
    {
        gl.bind_vertex_array(None);
        gl.delete_buffer(vbo);
        gl.delete_vertex_array(vao);
        return false;
    }

    let stride = 8 * std::mem::size_of::<f32>() as i32;

    gl.enable_vertex_attrib_array(0);
    gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, stride, 0);

    gl.enable_vertex_attrib_array(1);
    gl.vertex_attrib_pointer_f32(
        1,
        2,
        glow::FLOAT,
        false,
        stride,
        2 * std::mem::size_of::<f32>() as i32,
    );

    gl.enable_vertex_attrib_array(2);
    gl.vertex_attrib_pointer_f32(
        2,
        4,
        glow::FLOAT,
        false,
        stride,
        4 * std::mem::size_of::<f32>() as i32,
    );

    gl.bind_vertex_array(None);

    // Mesh VAO for stone_draw_mesh. No attrib state is configured here:
    // the draw sets pointers AND enables from the layout the caller passes,
    // so a VAO pre-shaped to one layout would be a second, stale statement.
    let mesh_vao = match gl.create_vertex_array() {
        Ok(vao) => vao,
        Err(e) => {
            ::log::error!(target: "STONE", "stone_sprite_init: failed to create mesh VAO: {e}");
            gl.delete_vertex_array(vao);
            gl.delete_buffer(vbo);
            return false;
        }
    };
    let Some((fs_quad_present_vao, fs_quad_present_vbo)) =
        upload_fs_quad(gl, &fs_quad_data(true, 1.0, 1.0))
    else {
        gl.delete_vertex_array(vao);
        gl.delete_buffer(vbo);
        gl.delete_vertex_array(mesh_vao);
        return false;
    };
    let Some((fs_quad_identity_vao, fs_quad_identity_vbo)) =
        upload_fs_quad(gl, &fs_quad_data(false, 1.0, 1.0))
    else {
        gl.delete_vertex_array(vao);
        gl.delete_buffer(vbo);
        gl.delete_vertex_array(mesh_vao);
        gl.delete_vertex_array(fs_quad_present_vao);
        gl.delete_buffer(fs_quad_present_vbo);
        return false;
    };
    let Some((fs_quad_scaled_vao, fs_quad_scaled_vbo)) =
        upload_fs_quad(gl, &fs_quad_data(false, 1.0, 1.0))
    else {
        gl.delete_vertex_array(vao);
        gl.delete_buffer(vbo);
        gl.delete_vertex_array(mesh_vao);
        gl.delete_vertex_array(fs_quad_present_vao);
        gl.delete_buffer(fs_quad_present_vbo);
        gl.delete_vertex_array(fs_quad_identity_vao);
        gl.delete_buffer(fs_quad_identity_vbo);
        return false;
    };

    SPRITE_STATE.with(|s| {
        *s.borrow_mut() = Some(SpriteState {
            vao,
            vbo,
            mesh_vao,
            triangle_indices: Vec::new(),
            fs_quad_present_vao,
            fs_quad_present_vbo,
            fs_quad_scaled_vao,
            fs_quad_scaled_vbo,
            fs_quad_identity_vao,
            fs_quad_identity_vbo,
        });
    });

    gl.enable(glow::BLEND);
    gl.blend_func_separate(
        glow::SRC_ALPHA,
        glow::ONE_MINUS_SRC_ALPHA,
        glow::ONE,
        glow::ONE_MINUS_SRC_ALPHA,
    );

    true
}

unsafe fn destroy_geometry(gl: &DebugGl, state: SpriteState) {
    gl.delete_vertex_array(state.vao);
    gl.delete_buffer(state.vbo);
    gl.delete_vertex_array(state.mesh_vao);
    gl.delete_vertex_array(state.fs_quad_present_vao);
    gl.delete_buffer(state.fs_quad_present_vbo);
    gl.delete_vertex_array(state.fs_quad_scaled_vao);
    gl.delete_buffer(state.fs_quad_scaled_vbo);
    gl.delete_vertex_array(state.fs_quad_identity_vao);
    gl.delete_buffer(state.fs_quad_identity_vbo);
}

pub(crate) fn shutdown() {
    SPRITE_STATE.with(|s| {
        if let Some(state) = s.borrow_mut().take() {
            with_gl(|gl| unsafe {
                destroy_geometry(gl, state);
            });
        }
    });
}

#[no_mangle]
pub extern "C" fn stone_current_shader_bound() -> bool {
    CURRENT_SHADER.with(|c| c.borrow().is_some())
}

thread_local! {
    static FS_QUAD_UV_SCALE: RefCell<(f32, f32)> = const { RefCell::new((1.0, 1.0)) };
}

/// Set the fullscreen quad's UV scale for subsequent draws. (1, 1) means the
/// sampled target's active rectangle is its whole texture.
#[no_mangle]
pub extern "C" fn stone_set_fullscreen_uv_scale(sx: c_float, sy: c_float) {
    FS_QUAD_UV_SCALE.with(|c| *c.borrow_mut() = (sx.max(0.0), sy.max(0.0)));
}

const FS_QUAD_PRESENT: [f32; 24] = [
    -1.0, 1.0, 0.0, 0.0, 1.0, -1.0, 1.0, 1.0, -1.0, -1.0, 0.0, 1.0, -1.0, 1.0, 0.0, 0.0, 1.0, 1.0,
    1.0, 0.0, 1.0, -1.0, 1.0, 1.0,
];
const FS_QUAD_IDENTITY: [f32; 24] = [
    -1.0, 1.0, 0.0, 1.0, 1.0, -1.0, 1.0, 0.0, -1.0, -1.0, 0.0, 0.0, -1.0, 1.0, 0.0, 1.0, 1.0, 1.0,
    1.0, 1.0, 1.0, -1.0, 1.0, 0.0,
];

fn fs_quad_data(to_window: bool, sx: f32, sy: f32) -> [f32; 24] {
    let mut data = if to_window {
        FS_QUAD_PRESENT
    } else {
        FS_QUAD_IDENTITY
    };
    let mut i = 0;
    while i < 24 {
        data[i + 2] *= sx;
        data[i + 3] *= sy;
        i += 4;
    }
    data
}

/// `(u0, v0)` maps to NDC (-1,-1) and `(u1, v1)` to (+1,+1).
fn fs_rect_data(u0: f32, v0: f32, u1: f32, v1: f32) -> [f32; 24] {
    [
        -1.0, 1.0, u0, v1, 1.0, -1.0, u1, v0, -1.0, -1.0, u0, v0, -1.0, 1.0, u0, v1, 1.0, 1.0, u1,
        v1, 1.0, -1.0, u1, v0,
    ]
}

/// Issue one fullscreen rectangle with an EXPLICIT corner UV mapping.
///
/// The semantic submission's geometry verb (`submission.rock`). Unlike
/// `stone_draw_fullscreen_quad` it infers nothing from the bound target: the
/// caller resolved the mapping and this draws exactly it, so recording and
/// execution consume one value rather than two computations that agree.
///
/// Culling is disabled HERE rather than assumed: the two tiers wind their
/// triangles opposite ways, so "no culling" has to be part of the state this
/// draw states, not a property of GL's default that nothing re-establishes.
#[no_mangle]
pub extern "C" fn stone_draw_fullscreen_rect(u0: c_float, v0: c_float, u1: c_float, v1: c_float) {
    if crate::ui_submission_probe::windowless() {
        crate::ui_submission_probe::fullscreen_consumed([
            u0 as f64, v0 as f64, u1 as f64, v1 as f64,
        ]);
        return;
    }
    SPRITE_STATE.with(|s| {
        if let Some(ref state) = *s.borrow() {
            with_gl(|gl| unsafe {
                let data = fs_rect_data(u0, v0, u1, v1);
                gl.bind_buffer(glow::ARRAY_BUFFER, Some(state.fs_quad_scaled_vbo));
                if stream_vertices(gl, bytemuck::cast_slice(&data)).is_err() {
                    gl.bind_vertex_array(None);
                    return;
                }
                gl.disable(glow::CULL_FACE);
                gl.bind_vertex_array(Some(state.fs_quad_scaled_vao));
                gl.draw_arrays(glow::TRIANGLES, 0, 6);
                crate::ui_submission_probe::fullscreen_consumed([
                    u0 as f64, v0 as f64, u1 as f64, v1 as f64,
                ]);
                crate::ui_submission_probe::device_state(gl);
                if let Some(p) = CURRENT_SHADER.with(|c| *c.borrow()) {
                    crate::uniform_block::mark_program_drawn(p);
                }
                track_draw_call();
                gl.bind_vertex_array(None);
            });
        }
    });
}

#[no_mangle]
pub extern "C" fn stone_draw_fullscreen_quad() {
    SPRITE_STATE.with(|s| {
        if let Some(ref state) = *s.borrow() {
            with_gl(|gl| unsafe {
                let to_window = crate::canvas::current_fbo().is_none();
                let (sx, sy) = FS_QUAD_UV_SCALE.with(|c| *c.borrow());
                let vao = if (sx - 1.0).abs() < 1e-9 && (sy - 1.0).abs() < 1e-9 {
                    if to_window {
                        state.fs_quad_present_vao
                    } else {
                        state.fs_quad_identity_vao
                    }
                } else {
                    let data = fs_quad_data(to_window, sx, sy);
                    gl.bind_buffer(glow::ARRAY_BUFFER, Some(state.fs_quad_scaled_vbo));
                    if stream_vertices(gl, bytemuck::cast_slice(&data)).is_err() {
                        gl.bind_vertex_array(None);
                        return;
                    }
                    state.fs_quad_scaled_vao
                };
                gl.bind_vertex_array(Some(vao));
                gl.draw_arrays(glow::TRIANGLES, 0, 6);
                if let Some(p) = CURRENT_SHADER.with(|c| *c.borrow()) {
                    crate::uniform_block::mark_program_drawn(p);
                }
                track_draw_call();
                gl.bind_vertex_array(None);
            });
        }
    });
}
/// The vertex layout is the CALLER's, passed per draw as component counts in
/// location order (`attr_sizes`) -- the same contract `stone_mesh_create` /
/// `stone_mesh_set_instances` take, sharing their validator and their attrib
/// setup. Nothing here knows what the sprite vertex looks like: the Rock
/// `Vertex` value struct owns that, and its consumers derive both numbers from
/// it via `@vertex_expand`.
/// Returns -1 when no draw was submitted, 0 when the draw had no resolved
/// texture, and 1 when the texture was bound.
#[no_mangle]
pub extern "C" fn stone_draw_mesh(
    data: *const c_float,
    count: c_int,
    stride: c_int,
    attr_sizes: *const c_int,
    attr_count: c_int,
    tex_kind: c_int,
    tex: u64,
) -> c_int {
    draw_mesh(
        data,
        count,
        stride,
        attr_sizes,
        attr_count,
        tex_kind,
        tex,
        &[],
    )
}

/// Explicit compact fan topology. Custom vertex shaders that observe
/// gl_VertexID must continue to use the expanded stone_draw_mesh contract.
#[no_mangle]
pub extern "C" fn stone_draw_mesh_fans(
    data: *const c_float,
    count: c_int,
    stride: c_int,
    attr_sizes: *const c_int,
    attr_count: c_int,
    tex_kind: c_int,
    tex: u64,
    fan_ranges: *const c_int,
    fan_count: c_int,
) -> c_int {
    if fan_count <= 0 || count <= 0 || fan_count > count / 2 || fan_ranges.is_null() {
        crate::log_once!("SPRITE", "stone_draw_mesh_fans: invalid fan range payload");
        return -1;
    }
    let ranges = unsafe { std::slice::from_raw_parts(fan_ranges, fan_count as usize * 2) };
    draw_mesh(
        data, count, stride, attr_sizes, attr_count, tex_kind, tex, ranges,
    )
}

fn draw_mesh(
    data: *const c_float,
    count: c_int,
    stride: c_int,
    attr_sizes: *const c_int,
    attr_count: c_int,
    tex_kind: c_int,
    tex: u64,
    fan_ranges: &[i32],
) -> c_int {
    if data.is_null() || count <= 0 || stride <= 0 {
        return -1;
    }

    let stride_usize = stride as usize;
    let count_usize = count as usize;
    if stride_usize > 1024 || count < 0 {
        crate::log_once!(
            "SPRITE",
            "stone_draw_mesh: invalid stride ({}) or count ({})",
            stride,
            count
        );
        return -1;
    }
    let Some(sizes) =
        crate::mesh::validate_layout("stone_draw_mesh", stride, attr_sizes, attr_count)
    else {
        return -1;
    };

    if crate::ui_submission_probe::windowless() {
        if fan_ranges.is_empty() {
            return if unsafe {
                crate::ui_submission_probe::mesh_consumed(
                    data,
                    count_usize,
                    stride_usize,
                    tex_kind,
                    tex,
                    0,
                )
            } {
                0
            } else {
                -1
            };
        }
        let mut indices = Vec::new();
        if let Err(error) = build_fan_indices(count_usize, fan_ranges, &mut indices) {
            ::log::error!(target: "STONE", "stone_draw_mesh_fans: {error}");
            return -1;
        }
        return if unsafe {
            crate::ui_submission_probe::indexed_mesh_consumed(
                data,
                stride_usize,
                &indices,
                tex_kind,
                tex,
                0,
            )
        } {
            0
        } else {
            -1
        };
    }

    SPRITE_STATE.with(|s| {
        if let Some(ref mut state) = *s.borrow_mut() {
            let indexed = !fan_ranges.is_empty();
            if indexed {
                if let Err(error) = build_fan_indices(count_usize, fan_ranges, &mut state.triangle_indices) {
                    ::log::error!(target: "STONE", "stone_draw_mesh_fans: {error}");
                    return -1;
                }
            }
            return with_gl(|gl| unsafe {
                let active_shader = match CURRENT_SHADER.with(|c| *c.borrow()) {
                    Some(p) => p,
                    None => {
                        crate::log_once!("SPRITE", "stone_draw_mesh: no shader bound; skipping (every caller must bind a program first)");
                        return -1;
                    }
                };
                gl.use_program(Some(active_shader));

                gl.bind_vertex_array(Some(state.mesh_vao));

                let tex_id = if tex_kind == 1 {
                    crate::canvas::canvas_texture_id(tex).unwrap_or(0)
                } else {
                    match crate::image_texture::sample_image_texture_for_bind(tex) {
                        Some(texture) => texture,
                        None if tex == 0 => 0,
                        None => return -1,
                    }
                };
                // Unit 0 only. Which sampler reads it is the program's own
                // business: a `@textures` group's slot 0 was pointed here once at
                // link (`bind_texture_slots`), and hand-authored GLSL either binds
                // by name through `Pass.inputs` or takes GL's link-time default of
                // unit 0. Nothing here names a uniform.
                if tex_id != 0 {
                    gl.active_texture(glow::TEXTURE0);
                    if let Some(native_tex) = std::num::NonZeroU32::new(tex_id) {
                        gl.bind_texture(glow::TEXTURE_2D, Some(glow::NativeTexture(native_tex)));
                    }
                }

                let stride_bytes = stride_usize * std::mem::size_of::<f32>();
                let upload = |stream, bytes: &[u8]| {
                    crate::stream_buffer::upload(gl, stream, bytes)
                        .map_err(|error| ::log::error!(target: "STONE", "stone_draw_mesh: {error}"))
                };
                if indexed {
                    let bytes = count_usize * stride_bytes;
                    let vertex_data = std::slice::from_raw_parts(data.cast::<u8>(), bytes);
                    let Ok(vertices) = upload(crate::stream_buffer::Stream::Vertex, vertex_data) else { return -1 };
                    gl.bind_buffer(glow::ARRAY_BUFFER, Some(vertices.buffer));
                    crate::mesh::setup_attribs(gl, 0, sizes.as_slice(), stride, 0, vertices.offset);
                    crate::track_stream_upload(count_usize, bytes, vertices.replaced);
                    for (offset, count) in stream_draw_chunks(state.triangle_indices.len()) {
                        let indices = &state.triangle_indices[offset..offset + count];
                        let bytes = bytemuck::cast_slice(indices);
                        let Ok(range) = upload(crate::stream_buffer::Stream::Index, bytes) else { return -1 };
                        gl.bind_buffer(glow::ELEMENT_ARRAY_BUFFER, Some(range.buffer));
                        crate::track_stream_upload(0, bytes.len(), range.replaced);
                        if crate::mesh_capture::armed() {
                            crate::mesh_capture::record(gl, vertex_data, vertices.offset, count_usize,
                                stride_bytes, sizes.as_slice().len(), Some((bytes, range.offset)));
                        }
                        gl.draw_elements(glow::TRIANGLES, count as c_int, glow::UNSIGNED_INT, range.offset);
                        if crate::ui_submission_probe::enabled() {
                            crate::ui_submission_probe::indexed_mesh_consumed(
                                data, stride_usize, indices, tex_kind, tex, offset);
                            crate::ui_submission_probe::device_state(gl);
                        }
                        crate::uniform_block::mark_program_drawn(active_shader);
                        track_draw_call();
                    }
                } else {
                    for (vertex_offset, chunk_count) in stream_draw_chunks(count_usize) {
                        let data_size = chunk_count * stride_bytes;
                        let vertex_data = std::slice::from_raw_parts(
                            data.add(vertex_offset * stride_usize).cast::<u8>(), data_size);
                        let Ok(vertices) = upload(crate::stream_buffer::Stream::Vertex, vertex_data) else { return -1 };
                        gl.bind_buffer(glow::ARRAY_BUFFER, Some(vertices.buffer));
                        crate::mesh::setup_attribs(gl, 0, sizes.as_slice(), stride, 0, vertices.offset);
                        crate::track_stream_upload(chunk_count, data_size, vertices.replaced);
                        if crate::mesh_capture::armed() {
                            crate::mesh_capture::record(gl, vertex_data, vertices.offset, chunk_count,
                                stride_bytes, sizes.as_slice().len(), None);
                        }
                        gl.draw_arrays(glow::TRIANGLES, 0, chunk_count as c_int);
                        if crate::ui_submission_probe::enabled() {
                            crate::ui_submission_probe::mesh_consumed(
                                data.add(vertex_offset * stride_usize), chunk_count,
                                stride_usize, tex_kind, tex, vertex_offset);
                            crate::ui_submission_probe::device_state(gl);
                        }
                        crate::uniform_block::mark_program_drawn(active_shader);
                        track_draw_call();
                    }
                }

                if tex_id != 0 { 1 } else { 0 }
            }).unwrap_or(-1);
        }
        -1
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{build_fan_indices, stream_draw_chunks, MAX_STREAM_DRAW_VERTICES};

    use std::{cell::RefCell, ffi::c_void};
    #[derive(Default)]
    struct Driver {
        allocation: usize,
        fail_at: usize,
        objects: std::collections::HashSet<u32>,
        invalid_deletes: usize,
        next: u32,
        error: u32,
    }
    thread_local! { static DRIVER: RefCell<Driver> = RefCell::new(Driver::default()); }

    pub(crate) fn failing_driver(fail_at: usize) -> crate::DebugGl {
        DRIVER.with_borrow_mut(|driver| {
            *driver = Driver {
                fail_at,
                ..Driver::default()
            }
        });
        unsafe extern "system" fn get_string(_: u32) -> *const u8 {
            c"3.3".as_ptr().cast()
        }
        unsafe extern "system" fn get_integer(_: u32, value: *mut i32) {
            *value = 0;
        }
        unsafe extern "system" fn get_error() -> u32 {
            DRIVER.with_borrow_mut(|driver| std::mem::replace(&mut driver.error, 0))
        }
        unsafe extern "system" fn generate(count: i32, names: *mut u32) {
            DRIVER.with_borrow_mut(|driver| {
                for i in 0..count {
                    driver.next += 1;
                    driver.objects.insert(driver.next);
                    *names.add(i as usize) = driver.next;
                }
            });
        }
        unsafe extern "system" fn delete(count: i32, names: *const u32) {
            DRIVER.with_borrow_mut(|driver| {
                for i in 0..count {
                    if !driver.objects.remove(&*names.add(i as usize)) {
                        driver.invalid_deletes += 1;
                    }
                }
            });
        }
        unsafe extern "system" fn buffer_data(_: u32, _: isize, _: *const c_void, _: u32) {
            DRIVER.with_borrow_mut(|driver| {
                driver.allocation += 1;
                if driver.allocation == driver.fail_at {
                    driver.error = glow::OUT_OF_MEMORY;
                }
            });
        }
        unsafe extern "system" fn blend(_: u32, _: u32, _: u32, _: u32) {}
        unsafe extern "system" fn bind_vertex_array(_: u32) {}
        unsafe extern "system" fn bind_buffer(_: u32, _: u32) {}
        unsafe extern "system" fn attrib(_: u32) {}
        unsafe extern "system" fn pointer(_: u32, _: i32, _: u32, _: u8, _: i32, _: *const c_void) {
        }
        crate::DebugGl::new(unsafe {
            glow::Context::from_loader_function(|name| match name {
                "glGetString" => get_string as *const c_void,
                "glGetIntegerv" => get_integer as *const c_void,
                "glGetError" => get_error as *const c_void,
                "glGenVertexArrays" | "glGenBuffers" => generate as *const c_void,
                "glDeleteVertexArrays" | "glDeleteBuffers" => delete as *const c_void,
                "glBindVertexArray" => bind_vertex_array as *const c_void,
                "glBindBuffer" => bind_buffer as *const c_void,
                "glBufferData" => buffer_data as *const c_void,
                "glEnableVertexAttribArray" | "glEnable" => attrib as *const c_void,
                "glBlendFuncSeparate" => blend as *const c_void,
                "glVertexAttribPointer" => pointer as *const c_void,
                _ => std::ptr::null(),
            })
        })
    }

    pub(crate) fn assert_no_driver_objects() {
        DRIVER.with_borrow(|driver| {
            assert!(
                driver.objects.is_empty(),
                "startup leaked GL objects: {:?}",
                driver.objects
            );
            assert_eq!(
                driver.invalid_deletes, 0,
                "startup deleted an unowned GL object"
            );
        });
    }

    #[test]
    fn sprite_geometry_teardown_releases_every_owned_object() {
        let gl = failing_driver(0);
        assert!(unsafe { super::init_geometry(&gl) });
        let state = super::SPRITE_STATE
            .with(|state| state.borrow_mut().take())
            .unwrap();
        unsafe { super::destroy_geometry(&gl, state) };
        assert_no_driver_objects();
    }

    #[test]
    fn required_sprite_allocations_fail_without_publishing_or_leaking() {
        for fail_at in 1..=4 {
            let gl = failing_driver(fail_at);
            let result = std::panic::catch_unwind(|| {
                assert!(!unsafe { super::init_geometry(&gl) });
                assert!(!super::initialized());
                assert_no_driver_objects();
            });
            if let Some(state) = super::SPRITE_STATE.with(|state| state.borrow_mut().take()) {
                unsafe { super::destroy_geometry(&gl, state) };
            }
            if let Err(error) = result {
                std::panic::resume_unwind(error);
            }
        }
    }

    #[test]
    fn fan_indices_preserve_mixed_triangle_order_and_close_the_rim() {
        let mut indices = Vec::new();
        build_fan_indices(11, &[3, 4], &mut indices).unwrap();
        assert_eq!(
            indices,
            [0, 1, 2, 3, 4, 5, 3, 5, 6, 3, 6, 7, 3, 7, 4, 8, 9, 10]
        );
        build_fan_indices(7, &[0, 3, 4, 2], &mut indices).unwrap();
        assert_eq!(indices, [0, 1, 2, 0, 2, 3, 0, 3, 1, 4, 5, 6, 4, 6, 5]);
        for ranges in [
            &[-1, 4][..],
            &[0, 0],
            &[0, 11],
            &[1, 4],
            &[0, 3, 2, 3],
            &[0],
        ] {
            assert!(build_fan_indices(11, ranges, &mut indices).is_err());
        }
        assert!(build_fan_indices(10, &[3, 4], &mut indices).is_err());
    }

    #[test]
    fn stream_draw_chunks_cover_large_submissions_without_oversized_chunks() {
        let total = MAX_STREAM_DRAW_VERTICES * 2 + 3;
        let chunks: Vec<_> = stream_draw_chunks(total).collect();
        let mut next = 0;
        for (offset, count) in &chunks {
            assert_eq!(*offset, next);
            assert_eq!(offset % 3, 0, "a chunk starts at a triangle boundary");
            assert_eq!(count % 3, 0, "every triangle remains in one draw");
            assert!(*count <= MAX_STREAM_DRAW_VERTICES);
            next += count;
        }
        assert_eq!(chunks.iter().map(|(_, count)| count).sum::<usize>(), total);
    }
}
