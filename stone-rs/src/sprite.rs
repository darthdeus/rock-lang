use std::cell::RefCell;
use std::ffi::{c_float, c_int};

use crate::{track_draw_call, with_gl, DebugGl, CURRENT_SHADER, SHADERS};

struct SpriteState {
    vao: glow::VertexArray,
    vbo: glow::Buffer,
    // Mesh VAO for stone_draw_mesh. Attribs are configured per draw from the
    // layout the caller passes, pointing into the vertex stream's range for
    // that draw (stream_buffer.rs); this VAO holds none of its own.
    mesh_vao: glow::VertexArray,
    triangle_indices: Vec<u32>,
    // Fullscreen-quad VAOs/VBOs (4-float vertex format: pos(2)+uv(2)).
    // Static: uploaded once at init, never modified. Two orientations,
    // selected per draw by the bound target (stone_draw_fullscreen_quad):
    //   present  -- NDC-top -> UV v=0. The one constant flip that lands an
    //               image-convention canvas upright on the bottom-up window.
    //   identity -- NDC-top -> UV v=1. A 1:1 copy (input row N -> output row
    //               N), so a canvas->canvas hop preserves content -- no
    //               per-hop flip, no parity to track.
    // Decoupled from the sprite VAO so stride changes there can't silently
    // corrupt fullscreen draws.
    fs_quad_present_vao: glow::VertexArray,
    fs_quad_present_vbo: glow::Buffer,
    fs_quad_identity_vao: glow::VertexArray,
    fs_quad_identity_vbo: glow::Buffer,
    // Storage replaced per draw for explicit or scaled UV mappings.
    fs_quad_scaled_vao: glow::VertexArray,
    fs_quad_scaled_vbo: glow::Buffer,
}

// Resolve a registry program from a Rock shader handle (get_shader_handle);
// None (unknown / 0 handle) is the caller's fail-closed signal. The immediate
// and present sprite paths draw through the Rock-authored sprite_basic_shader,
// resolved fresh by handle each draw (same pattern as text.rs); no cached GL
// program, so a shader hot-reload re-points with zero staleness.
fn resolve_program(shader: u32) -> Option<glow::Program> {
    SHADERS.with(|s| s.borrow().get(&(shader as u64)).map(|sh| sh.program))
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

// Fan ranges describe physical center/rim records among ordinary triangles.
// Validate the complete topology before a backend-visible effect, then preserve
// triangle and corner order while sharing the exact submitted vertex records.
fn build_fan_indices(
    vertices: usize,
    ranges: &[i32],
    indices: &mut Vec<u32>,
) -> Result<(), &'static str> {
    if ranges.len() % 2 != 0 {
        return Err("incomplete fan range");
    }
    let mut cursor = 0;
    let mut count = vertices;
    for range in ranges.chunks_exact(2) {
        if range[0] < 0 || range[1] <= 0 {
            return Err("invalid fan range");
        }
        let first = range[0] as usize;
        let segments = range[1] as usize;
        let end = first
            .checked_add(segments + 1)
            .ok_or("fan range overflow")?;
        if first < cursor || end > vertices || (first - cursor) % 3 != 0 {
            return Err("fan range overlaps or leaves an incomplete triangle");
        }
        count = count
            .checked_add(segments * 2 - 1)
            .ok_or("index count overflow")?;
        cursor = end;
    }
    if (vertices - cursor) % 3 != 0 {
        return Err("incomplete triangle after fan");
    }
    indices.clear();
    indices.reserve(count);
    cursor = 0;
    for range in ranges.chunks_exact(2) {
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
    // The caller binds its VBO. Replace storage so queued draws keep their vertices.
    gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, data, glow::STREAM_DRAW)
}

#[no_mangle]
pub extern "C" fn stone_sprite_init() -> bool {
    // Idempotent: the GPU host and explicit window setup both initialize, and
    // a second init after a scene pass must succeed without reallocating.
    if initialized() {
        return with_gl(|_| true).unwrap_or(false);
    }
    with_gl(|gl| unsafe { init_geometry(gl) }).unwrap_or(false)
}

unsafe fn init_geometry(gl: &crate::DebugGl) -> bool {
    // The sprite program is authored in Rock (sprite_basic_shader.rock) and
    // compiled through the registry; this owns only the VAO/VBO geometry.

    // Create VAO and VBO for sprite rendering (8-float format)
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

    // Allocate buffer (6 vertices * 8 floats per vertex = 48 floats)
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

    // Set up vertex attributes: pos(2) + uv(2) + color(4) = 8 floats
    let stride = 8 * std::mem::size_of::<f32>() as i32;

    // Position (location 0)
    gl.enable_vertex_attrib_array(0);
    gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, stride, 0);

    // TexCoord (location 1)
    gl.enable_vertex_attrib_array(1);
    gl.vertex_attrib_pointer_f32(
        1,
        2,
        glow::FLOAT,
        false,
        stride,
        2 * std::mem::size_of::<f32>() as i32,
    );

    // Color (location 2)
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
    // Two static fullscreen quads (pos(2)+uv(2), 6 verts each). They
    // differ only in the UV.y column; stone_draw_fullscreen_quad picks by
    // the bound target. See the SpriteState field comment for the split.
    //
    // present: NDC-top -> UV v=0. The window is bottom-up and canvases
    // store image-convention content, so a present needs exactly this one
    // flip.
    let Some((fs_quad_present_vao, fs_quad_present_vbo)) =
        upload_fs_quad(gl, &fs_quad_data(true, 1.0, 1.0))
    else {
        gl.delete_vertex_array(vao);
        gl.delete_buffer(vbo);
        gl.delete_vertex_array(mesh_vao);
        return false;
    };
    // identity: NDC-top -> UV v=1 (V column flipped from present). A 1:1
    // copy -- canvas->canvas hops preserve stored orientation.
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

    // Enable blending by default
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

#[no_mangle]
pub extern "C" fn stone_sprite_shutdown() {
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

#[no_mangle]
pub extern "C" fn stone_sprite_draw_texture(
    shader: u32,
    tex: u32,
    tw: c_int,
    th: c_int,
    x: c_float,
    y: c_float,
    w: c_float,
    h: c_float,
    sx: c_float,
    sy: c_float,
    sw: c_float,
    sh: c_float,
    rot: c_float,
    scale_x: c_float,
    scale_y: c_float,
    r: c_float,
    g: c_float,
    b: c_float,
    a: c_float,
) {
    SPRITE_STATE.with(|s| {
        if let Some(ref state) = *s.borrow() {
            with_gl(|gl| unsafe {
                // Use whatever shader is currently bound, or fall back to the
                // basic sprite program the caller named (get_shader_handle(
                // sprite_basic_shader)). Standalone callers (draw_sampled_texture, the
                // screen present) have no shader bound. Fail closed if neither
                // resolves.
                let current_shader = CURRENT_SHADER.with(|c| *c.borrow());
                let active_shader = match current_shader.or_else(|| resolve_program(shader)) {
                    Some(p) => p,
                    None => {
                        crate::log_once!("SPRITE", "stone_sprite_draw_texture: no shader bound and handle {} not in registry; skipping", shader);
                        return;
                    }
                };
                gl.use_program(Some(active_shader));

                // Calculate UV coordinates
                let u0 = sx / tw as f32;
                let v0 = sy / th as f32;
                let u1 = (sx + sw) / tw as f32;
                let v1 = (sy + sh) / th as f32;

                // Calculate scaled dimensions
                let scaled_w = w * scale_x;
                let scaled_h = h * scale_y;

                // Generate vertices with rotation
                let cos_r = rot.cos();
                let sin_r = rot.sin();

                // Vertices before rotation (centered at origin for rotation)
                let corners = [
                    (0.0, 0.0),
                    (scaled_w, 0.0),
                    (scaled_w, scaled_h),
                    (0.0, scaled_h),
                ];

                // Apply rotation and translation
                let mut rotated = [(0.0f32, 0.0f32); 4];
                for (i, (cx, cy)) in corners.iter().enumerate() {
                    let rx = cx * cos_r - cy * sin_r;
                    let ry = cx * sin_r + cy * cos_r;
                    rotated[i] = (x + rx, y + ry);
                }

                // Build vertex data: 6 vertices (2 triangles) * 8 floats each
                let vertices: [f32; 48] = [
                    // Triangle 1
                    rotated[0].0, rotated[0].1, u0, v0, r, g, b, a,
                    rotated[1].0, rotated[1].1, u1, v0, r, g, b, a,
                    rotated[2].0, rotated[2].1, u1, v1, r, g, b, a,
                    // Triangle 2
                    rotated[0].0, rotated[0].1, u0, v0, r, g, b, a,
                    rotated[2].0, rotated[2].1, u1, v1, r, g, b, a,
                    rotated[3].0, rotated[3].1, u0, v1, r, g, b, a,
                ];

                // Bind sprite VAO. In core profile, draw_arrays requires a VAO
                // to be current, and the caller binds none of its own.
                gl.bind_vertex_array(Some(state.vao));

                gl.bind_buffer(glow::ARRAY_BUFFER, Some(state.vbo));
                if stream_vertices(gl, bytemuck::cast_slice(&vertices)).is_err() {
                    gl.bind_vertex_array(None);
                    return;
                }

                // Bind texture
                gl.active_texture(glow::TEXTURE0);
                if let Some(native_tex) = std::num::NonZeroU32::new(tex) {
                    gl.bind_texture(glow::TEXTURE_2D, Some(glow::NativeTexture(native_tex)));
                }

                // Draw
                gl.draw_arrays(glow::TRIANGLES, 0, 6);
                crate::uniform_block::mark_program_drawn(active_shader);
                track_draw_call();
            });
        }
    });
}

thread_local! {
    // The active-local UV transform the pass executor installs before a
    // fullscreen draw: the sampled target's active rectangle as a fraction of
    // its allocated capacity. Scaling the quad's UV column (rather than a
    // uniform) keeps every fragment shader reading TexCoord as "the live
    // image", including the ones with their own vertex stage.
    static FS_QUAD_UV_SCALE: RefCell<(f32, f32)> = const { RefCell::new((1.0, 1.0)) };
}

/// Set the fullscreen quad's UV scale for subsequent draws. (1, 1) means the
/// sampled target's active rectangle is its whole texture.
#[no_mangle]
pub extern "C" fn stone_set_fullscreen_uv_scale(sx: c_float, sy: c_float) {
    FS_QUAD_UV_SCALE.with(|c| *c.borrow_mut() = (sx.max(0.0), sy.max(0.0)));
}

// The two static quads' vertex data, scaled into the dynamic quad when the UV
// scale is not identity. They differ only in the V column; see the SpriteState
// field comment for the split.
const FS_QUAD_PRESENT: [f32; 24] = [
    -1.0, 1.0, 0.0, 0.0, 1.0, -1.0, 1.0, 1.0, -1.0, -1.0, 0.0, 1.0, -1.0, 1.0, 0.0, 0.0, 1.0, 1.0,
    1.0, 0.0, 1.0, -1.0, 1.0, 1.0,
];
const FS_QUAD_IDENTITY: [f32; 24] = [
    -1.0, 1.0, 0.0, 1.0, 1.0, -1.0, 1.0, 0.0, -1.0, -1.0, 0.0, 0.0, -1.0, 1.0, 0.0, 1.0, 1.0, 1.0,
    1.0, 1.0, 1.0, -1.0, 1.0, 0.0,
];

/// The vertices `stone_draw_fullscreen_quad` submits for a target and UV scale.
/// The two static quads are uploaded from it at init and the scaled
/// respecification below writes it, so this file states that geometry once.
///
/// Serves the IMPERATIVE fullscreen path only (`pass.rock`, the debug
/// overlays), which infers orientation from the bound target. The semantic
/// submission goes through `stone_draw_fullscreen_rect` instead, where the
/// caller states the corner mapping and nothing is inferred.
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

/// THIS backend's tessellation of the fullscreen rectangle, given the corner UV
/// mapping the submission resolved: `(u0, v0)` at NDC (-1,-1), `(u1, v1)` at
/// NDC (+1,+1).
///
/// The mapping is the semantics; the order, the diagonal and the winding are
/// this backend's own choice, and the WebGL tier deliberately picks a different
/// one (`__sw_fsq_verts`). Both cover the same rectangle with the same texture
/// mapping, which is the only thing a fragment-only shader can observe -- its
/// vertex stage is compiler-generated and reads no vertex order
/// (`a1_vertex_stage_is_compiler_owned_and_order_blind`).
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
    // Draw a fullscreen quad in NDC using the currently-bound shader.
    // Vertex data is static and lives in fs_quad_vbo (uploaded once at init).
    // The shader is expected to bypass or receive identity matrices; post-process
    // callers set their own uProjection/uView to identity if needed.
    SPRITE_STATE.with(|s| {
        if let Some(ref state) = *s.borrow() {
            with_gl(|gl| unsafe {
                // Pick by the bound target, never by the call site: the window
                // gets the present quad (the one constant flip), a canvas gets
                // the identity quad (1:1, preserves content). No pass chooses
                // an orientation, so the mirrored-window class can't recur.
                let to_window = crate::canvas::current_fbo().is_none();
                let (sx, sy) = FS_QUAD_UV_SCALE.with(|c| *c.borrow());
                let vao = if (sx - 1.0).abs() < 1e-9 && (sy - 1.0).abs() < 1e-9 {
                    if to_window {
                        state.fs_quad_present_vao
                    } else {
                        state.fs_quad_identity_vao
                    }
                } else {
                    // Keep the UV transform in geometry so custom vertex
                    // stages receive it too.
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
                // The fullscreen quad draws through whatever is bound.
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

    // Prevent integer overflow in size calculation
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
                // Every mesh draw runs under a bound program: the world/UI flush
                // binds sprite_batch (or a pipeline), the immediate shape path
                // binds sprite_basic + a pixel ortho, custom draws bind their own.
                // Fail closed if nothing is bound -- a silent default-program
                // fallback here is what masked the custom_draw shader desync
                // (world_shader_after_custom_draw).
                let active_shader = match CURRENT_SHADER.with(|c| *c.borrow()) {
                    Some(p) => p,
                    None => {
                        crate::log_once!("SPRITE", "stone_draw_mesh: no shader bound; skipping (every caller must bind a program first)");
                        return -1;
                    }
                };
                gl.use_program(Some(active_shader));

                // Bind mesh VAO
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

// =============================================================================
// Blend/Depth Mode
