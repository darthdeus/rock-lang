//! Stable C ABI surface for native renderers running in the same process as Stone.
//!
//! A second native library (such as a game's native renderer) obtains the static
//! function table via `stone_native_renderer_api(STONE_NATIVE_RENDERER_ABI_VERSION)`
//! without linking a second `stone-rs` instance.

use std::cell::RefCell;
use std::ffi::{c_char, c_float, c_int, c_void, CStr};
use std::num::NonZeroU32;

use crate::gl_state::{BlendGlState, DRAW_BUFFERS};
use crate::{track_canvas_switch, track_shader_switch, with_gl, DebugGl, CURRENT_SHADER};

pub const STONE_NATIVE_RENDERER_ABI_VERSION: u32 = 1;

pub const STONE_RESOURCE_TARGET: c_int = 1;
pub const STONE_RESOURCE_SHADER: c_int = 2;
pub const STONE_RESOURCE_TEXTURE: c_int = 3;
pub const STONE_RESOURCE_MESH: c_int = 4;

pub const STONE_STAGE_RESTORED: u32 = 1 << 0;
pub const STONE_STAGE_TARGET_CHANGED: u32 = 1 << 1;
pub const STONE_STAGE_SHADER_CHANGED: u32 = 1 << 2;
pub const STONE_STAGE_DRAW_BUFFERS_CHANGED: u32 = 1 << 3;

const SAVED_UBO_SLOTS: usize = 8;
const SAVED_TEXTURE_UNITS: usize = 8;

#[derive(Clone, Copy)]
struct NativeStageState {
    canvas_handle: u64,
    fbo: Option<glow::Framebuffer>,
    gl_draw_fbo: Option<glow::Framebuffer>,
    gl_read_fbo: Option<glow::Framebuffer>,
    target_size: (f32, f32),
    viewport: [i32; 4],
    scissor_enabled: bool,
    scissor_box: [i32; 4],
    cached_shader: Option<glow::Program>,
    gl_program: Option<glow::Program>,
    generic_ubo: Option<glow::Buffer>,
    ubo_count: usize,
    ubo_slots: [(Option<glow::Buffer>, i32, i32); SAVED_UBO_SLOTS],
    active_texture: u32,
    tex_unit_count: usize,
    textures_2d: [Option<glow::Texture>; SAVED_TEXTURE_UNITS],
    samplers: [Option<glow::Sampler>; SAVED_TEXTURE_UNITS],
    vao: Option<glow::VertexArray>,
    draw_buffer_count: usize,
    draw_buffers: [u32; DRAW_BUFFERS],
    blend: BlendGlState,
    depth_enabled: bool,
    depth_func: u32,
    depth_write: bool,
    color_mask: [bool; 4],
    clear_color: [f32; 4],
    clear_depth: f32,
    cull_enabled: bool,
    stencil_enabled: bool,
    polygon_offset_fill: bool,
    sample_alpha_to_coverage: bool,
    rasterizer_discard: bool,
}

thread_local! {
    static NATIVE_STAGE_STACK: RefCell<Vec<NativeStageState>> = const { RefCell::new(Vec::new()) };
}

pub(crate) fn reset_on_teardown() {
    NATIVE_STAGE_STACK.with(|stack| stack.borrow_mut().clear());
}

unsafe fn read_framebuffer(gl: &DebugGl, param: u32) -> Option<glow::Framebuffer> {
    NonZeroU32::new(gl.get_parameter_i32(param) as u32).map(glow::NativeFramebuffer)
}

unsafe fn read_program(gl: &DebugGl) -> Option<glow::Program> {
    NonZeroU32::new(gl.get_parameter_i32(glow::CURRENT_PROGRAM) as u32).map(glow::NativeProgram)
}

unsafe fn read_buffer(gl: &DebugGl, param: u32) -> Option<glow::Buffer> {
    NonZeroU32::new(gl.get_parameter_i32(param) as u32).map(glow::NativeBuffer)
}

unsafe fn read_indexed_ubo(gl: &DebugGl, index: u32) -> (Option<glow::Buffer>, i32, i32) {
    let buffer =
        NonZeroU32::new(gl.get_parameter_indexed_i32(glow::UNIFORM_BUFFER_BINDING, index) as u32)
            .map(glow::NativeBuffer);
    let start = gl.get_parameter_indexed_i32(glow::UNIFORM_BUFFER_START, index);
    let size = gl.get_parameter_indexed_i32(glow::UNIFORM_BUFFER_SIZE, index);
    (buffer, start, size)
}

unsafe fn read_draw_buffers(gl: &DebugGl, is_default_fbo: bool) -> (usize, [u32; DRAW_BUFFERS]) {
    let mut buffers = [glow::NONE; DRAW_BUFFERS];
    if is_default_fbo {
        buffers[0] = gl.get_parameter_i32(glow::DRAW_BUFFER0) as u32;
        return (1, buffers);
    }
    let count = (gl.get_parameter_i32(glow::MAX_DRAW_BUFFERS) as usize).clamp(1, DRAW_BUFFERS);
    for (i, dst) in buffers.iter_mut().enumerate().take(count) {
        *dst = gl.get_parameter_i32(glow::DRAW_BUFFER0 + i as u32) as u32;
    }
    (count, buffers)
}

unsafe fn capture_stage_state(gl: &DebugGl) -> NativeStageState {
    let gl_draw_fbo = read_framebuffer(gl, glow::DRAW_FRAMEBUFFER_BINDING);
    let gl_read_fbo = read_framebuffer(gl, glow::READ_FRAMEBUFFER_BINDING);
    let mut viewport = [0i32; 4];
    gl.get_parameter_i32_slice(glow::VIEWPORT, &mut viewport);
    let scissor_enabled = gl.is_enabled(glow::SCISSOR_TEST);
    let mut scissor_box = [0i32; 4];
    gl.get_parameter_i32_slice(glow::SCISSOR_BOX, &mut scissor_box);

    let cached_shader = CURRENT_SHADER.with(|c| *c.borrow());
    let gl_program = read_program(gl);

    let generic_ubo = read_buffer(gl, glow::UNIFORM_BUFFER_BINDING);
    let max_ubo = gl
        .get_parameter_i32(glow::MAX_UNIFORM_BUFFER_BINDINGS)
        .max(0) as usize;
    let ubo_count = max_ubo.min(SAVED_UBO_SLOTS);
    let mut ubo_slots = [(None, 0i32, 0i32); SAVED_UBO_SLOTS];
    for (i, slot) in ubo_slots.iter_mut().enumerate().take(ubo_count) {
        *slot = read_indexed_ubo(gl, i as u32);
    }

    let active_texture = gl.get_parameter_i32(glow::ACTIVE_TEXTURE) as u32;
    let max_units = gl
        .get_parameter_i32(glow::MAX_COMBINED_TEXTURE_IMAGE_UNITS)
        .max(0) as usize;
    let tex_unit_count = max_units.min(SAVED_TEXTURE_UNITS);
    let mut textures_2d = [None; SAVED_TEXTURE_UNITS];
    let mut samplers = [None; SAVED_TEXTURE_UNITS];
    for i in 0..tex_unit_count {
        gl.active_texture(glow::TEXTURE0 + i as u32);
        textures_2d[i] = NonZeroU32::new(gl.get_parameter_i32(glow::TEXTURE_BINDING_2D) as u32)
            .map(glow::NativeTexture);
        samplers[i] = NonZeroU32::new(gl.get_parameter_i32(glow::SAMPLER_BINDING) as u32)
            .map(glow::NativeSampler);
    }
    gl.active_texture(active_texture);

    let vao = NonZeroU32::new(gl.get_parameter_i32(glow::VERTEX_ARRAY_BINDING) as u32)
        .map(glow::NativeVertexArray);
    let (draw_buffer_count, draw_buffers) = read_draw_buffers(gl, gl_draw_fbo.is_none());

    let mut clear_color = [0.0f32; 4];
    gl.get_parameter_f32_slice(glow::COLOR_CLEAR_VALUE, &mut clear_color);
    let mut clear_depth = [0.0f32; 1];
    gl.get_parameter_f32_slice(glow::DEPTH_CLEAR_VALUE, &mut clear_depth);

    NativeStageState {
        canvas_handle: crate::canvas::current_canvas_handle(),
        fbo: crate::canvas::current_fbo(),
        gl_draw_fbo,
        gl_read_fbo,
        target_size: crate::canvas::current_target_size_raw(),
        viewport,
        scissor_enabled,
        scissor_box,
        cached_shader,
        gl_program,
        generic_ubo,
        ubo_count,
        ubo_slots,
        active_texture,
        tex_unit_count,
        textures_2d,
        samplers,
        vao,
        draw_buffer_count,
        draw_buffers,
        blend: BlendGlState::read(gl),
        depth_enabled: gl.is_enabled(glow::DEPTH_TEST),
        depth_func: gl.get_parameter_i32(glow::DEPTH_FUNC) as u32,
        depth_write: gl.get_parameter_bool(glow::DEPTH_WRITEMASK),
        color_mask: gl.get_parameter_bool_array::<4>(glow::COLOR_WRITEMASK),
        clear_color,
        clear_depth: clear_depth[0],
        cull_enabled: gl.is_enabled(glow::CULL_FACE),
        stencil_enabled: gl.is_enabled(glow::STENCIL_TEST),
        polygon_offset_fill: gl.is_enabled(glow::POLYGON_OFFSET_FILL),
        sample_alpha_to_coverage: gl.is_enabled(glow::SAMPLE_ALPHA_TO_COVERAGE),
        rasterizer_discard: gl.is_enabled(glow::RASTERIZER_DISCARD),
    }
}

unsafe fn restore_stage_state(gl: &DebugGl, saved: &NativeStageState) -> u32 {
    let mut flags = STONE_STAGE_RESTORED;

    let cur_draw_fbo = read_framebuffer(gl, glow::DRAW_FRAMEBUFFER_BINDING);
    let cur_read_fbo = read_framebuffer(gl, glow::READ_FRAMEBUFFER_BINDING);
    let cur_handle = crate::canvas::current_canvas_handle();
    let cur_fbo = crate::canvas::current_fbo();
    if cur_handle != saved.canvas_handle
        || cur_fbo != saved.fbo
        || cur_draw_fbo != saved.gl_draw_fbo
        || cur_read_fbo != saved.gl_read_fbo
    {
        if saved.gl_draw_fbo == saved.gl_read_fbo {
            gl.bind_framebuffer(glow::FRAMEBUFFER, saved.gl_draw_fbo);
        } else {
            gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, saved.gl_draw_fbo);
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, saved.gl_read_fbo);
        }
        crate::canvas::restore_target_cache(saved.canvas_handle, saved.fbo, saved.target_size);
        track_canvas_switch();
        flags |= STONE_STAGE_TARGET_CHANGED;
    } else if crate::canvas::current_target_size_raw() != saved.target_size {
        crate::canvas::restore_target_cache(saved.canvas_handle, saved.fbo, saved.target_size);
    }

    let (cur_db_count, cur_dbs) = read_draw_buffers(gl, saved.gl_draw_fbo.is_none());
    let db_len = saved.draw_buffer_count.min(cur_db_count);
    if cur_dbs[..db_len] != saved.draw_buffers[..db_len] {
        if saved.gl_draw_fbo.is_none() {
            gl.draw_buffers(&saved.draw_buffers[..1]);
        } else {
            let len = saved.draw_buffers[..db_len]
                .iter()
                .rposition(|&b| b != glow::NONE)
                .map_or(1, |idx| idx + 1);
            gl.draw_buffers(&saved.draw_buffers[..len]);
        }
        flags |= STONE_STAGE_DRAW_BUFFERS_CHANGED;
    }

    let mut cur_vp = [0i32; 4];
    gl.get_parameter_i32_slice(glow::VIEWPORT, &mut cur_vp);
    if cur_vp != saved.viewport {
        gl.viewport(
            saved.viewport[0],
            saved.viewport[1],
            saved.viewport[2],
            saved.viewport[3],
        );
    }
    let cur_scissor_enabled = gl.is_enabled(glow::SCISSOR_TEST);
    let mut cur_scissor_box = [0i32; 4];
    gl.get_parameter_i32_slice(glow::SCISSOR_BOX, &mut cur_scissor_box);
    if cur_scissor_box != saved.scissor_box {
        gl.scissor(
            saved.scissor_box[0],
            saved.scissor_box[1],
            saved.scissor_box[2],
            saved.scissor_box[3],
        );
    }
    if cur_scissor_enabled != saved.scissor_enabled {
        gl.set_enabled(glow::SCISSOR_TEST, saved.scissor_enabled);
    }

    let cur_cached_shader = CURRENT_SHADER.with(|c| *c.borrow());
    let cur_gl_program = read_program(gl);
    if cur_cached_shader != saved.cached_shader || cur_gl_program != saved.gl_program {
        gl.use_program(saved.gl_program);
        CURRENT_SHADER.with(|c| *c.borrow_mut() = saved.cached_shader);
        track_shader_switch();
        flags |= STONE_STAGE_SHADER_CHANGED;
    }

    for i in 0..saved.ubo_count {
        let cur = read_indexed_ubo(gl, i as u32);
        let want = saved.ubo_slots[i];
        if cur != want {
            match want {
                (None, _, _) => gl.bind_buffer_base(glow::UNIFORM_BUFFER, i as u32, None),
                (Some(buf), _, 0) => gl.bind_buffer_base(glow::UNIFORM_BUFFER, i as u32, Some(buf)),
                (Some(buf), start, size) => {
                    gl.bind_buffer_range(glow::UNIFORM_BUFFER, i as u32, Some(buf), start, size);
                }
            }
        }
    }
    if read_buffer(gl, glow::UNIFORM_BUFFER_BINDING) != saved.generic_ubo {
        gl.bind_buffer(glow::UNIFORM_BUFFER, saved.generic_ubo);
    }

    for i in 0..saved.tex_unit_count {
        gl.active_texture(glow::TEXTURE0 + i as u32);
        let cur_tex = NonZeroU32::new(gl.get_parameter_i32(glow::TEXTURE_BINDING_2D) as u32)
            .map(glow::NativeTexture);
        if cur_tex != saved.textures_2d[i] {
            gl.bind_texture(glow::TEXTURE_2D, saved.textures_2d[i]);
        }
        let cur_sampler = NonZeroU32::new(gl.get_parameter_i32(glow::SAMPLER_BINDING) as u32)
            .map(glow::NativeSampler);
        if cur_sampler != saved.samplers[i] {
            gl.bind_sampler(i as u32, saved.samplers[i]);
        }
    }
    gl.active_texture(saved.active_texture);

    let cur_vao = NonZeroU32::new(gl.get_parameter_i32(glow::VERTEX_ARRAY_BINDING) as u32)
        .map(glow::NativeVertexArray);
    if cur_vao != saved.vao {
        gl.bind_vertex_array(saved.vao);
    }

    saved.blend.restore(gl);
    gl.set_enabled(glow::DEPTH_TEST, saved.depth_enabled);
    gl.depth_func(saved.depth_func);
    gl.depth_mask(saved.depth_write);
    gl.color_mask(
        saved.color_mask[0],
        saved.color_mask[1],
        saved.color_mask[2],
        saved.color_mask[3],
    );
    gl.clear_color(
        saved.clear_color[0],
        saved.clear_color[1],
        saved.clear_color[2],
        saved.clear_color[3],
    );
    gl.clear_depth_f32(saved.clear_depth);
    gl.set_enabled(glow::CULL_FACE, saved.cull_enabled);
    gl.set_enabled(glow::STENCIL_TEST, saved.stencil_enabled);
    gl.set_enabled(glow::POLYGON_OFFSET_FILL, saved.polygon_offset_fill);
    gl.set_enabled(
        glow::SAMPLE_ALPHA_TO_COVERAGE,
        saved.sample_alpha_to_coverage,
    );
    gl.set_enabled(glow::RASTERIZER_DISCARD, saved.rasterizer_discard);

    flags
}

/// Begin a native rendering stage: opens the render-thread frame scope and
/// snapshots the GL and engine-cached state so `stone_native_stage_end` can
/// restore only what changed. Returns a non-zero LIFO scope token, or 0 if no
/// GL context is live on this thread.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_native_stage_begin() -> u32 {
    let Some(state) = with_gl(|gl| unsafe { capture_stage_state(gl) }) else {
        return 0;
    };
    NATIVE_STAGE_STACK.with(|stack| {
        let mut stack = stack.borrow_mut();
        stack.push(state);
        stack.len() as u32
    })
}

/// End a native rendering stage, closing the frame scope and restoring saved
/// GL and engine-cached state. Returns a bitmask (`STONE_STAGE_*`), or 0 on
/// mismatched scope or missing GL context.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_native_stage_end(scope: u32) -> u32 {
    NATIVE_STAGE_STACK.with(|stack| {
        let mut stack = stack.borrow_mut();
        if scope == 0 || stack.len() != scope as usize {
            return 0;
        }
        let saved = stack[stack.len() - 1];
        let Some(flags) = with_gl(|gl| unsafe { restore_stage_state(gl, &saved) }) else {
            return 0;
        };
        stack.pop();
        flags
    })
}

/// True only on the render thread while a native stage / custom draw scope is active.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_native_frame_active() -> bool {
    with_gl(|_| ()).is_some() && NATIVE_STAGE_STACK.with(|stack| !stack.borrow().is_empty())
}

/// Currently bound render target handle during an active frame scope (0 = window).
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_native_frame_target() -> u64 {
    if !stone_native_frame_active() {
        return 0;
    }
    crate::canvas::current_canvas_handle()
}

/// Active width and height of the currently bound render target during an active
/// frame scope. Returns false outside an active scope or off the render thread.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_native_frame_size(width: *mut c_int, height: *mut c_int) -> bool {
    if !stone_native_frame_active() {
        return false;
    }
    let (w, h) = crate::canvas::current_target_size();
    crate::write_out(width, w.round() as c_int);
    crate::write_out(height, h.round() as c_int);
    true
}

/// Monotonic generation of the live GL context, or 0 when no context is active.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_native_context_generation() -> u64 {
    if with_gl(|_| ()).is_none() {
        return 0;
    }
    crate::handles::context_generation()
}

/// Typed handle generation for targets (1), shaders (2), textures (3) and meshes (4).
/// Returns 0 when invalid, wrong kind, destroyed, or after context loss. Changes on
/// target resize, active-rect change, attachment change, texture reload, or shader reload.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_native_handle_generation(kind: c_int, handle: u64) -> u64 {
    if handle == 0 || with_gl(|_| ()).is_none() {
        return 0;
    }
    match kind {
        STONE_RESOURCE_TARGET => crate::canvas::canvas_generation(handle),
        STONE_RESOURCE_SHADER => crate::shader::shader_generation(handle),
        STONE_RESOURCE_TEXTURE => crate::image_texture::image_generation(handle),
        STONE_RESOURCE_MESH => crate::mesh::mesh_generation(handle),
        _ => 0,
    }
}

/// Typed handle validity for targets (1), shaders (2), textures (3) and meshes (4).
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_native_handle_valid(kind: c_int, handle: u64) -> bool {
    stone_native_handle_generation(kind, handle) != 0
}

/// Resolve a registered shader handle by name, or 0 if absent.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_native_find_shader(name: *const c_char) -> u64 {
    if name.is_null() || with_gl(|_| ()).is_none() {
        return 0;
    }
    let Ok(name) = unsafe { CStr::from_ptr(name) }.to_str() else {
        return 0;
    };
    crate::shader::find_shader_by_name(name)
}

/// Resolve a loaded image texture handle by asset path, or 0 if absent.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_native_find_texture(path: *const c_char) -> u64 {
    if path.is_null() || with_gl(|_| ()).is_none() {
        return 0;
    }
    let Ok(path) = unsafe { CStr::from_ptr(path) }.to_str() else {
        return 0;
    };
    crate::image_texture::find_image_by_path(path)
}

/// Lookup target metadata; returns generation (> 0) if valid, or 0 if invalid.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_native_target_info(
    handle: u64,
    width: *mut c_int,
    height: *mut c_int,
    format: *mut c_int,
    color_attachments: *mut c_int,
    has_depth: *mut bool,
) -> u64 {
    if with_gl(|_| ()).is_none() {
        return 0;
    }
    let Some((gen_id, w, h, fmt, count, depth)) = crate::canvas::canvas_info(handle) else {
        return 0;
    };
    crate::write_out(width, w);
    crate::write_out(height, h);
    crate::write_out(format, fmt);
    crate::write_out(color_attachments, count);
    crate::write_out(has_depth, depth);
    gen_id
}

/// Lookup shader metadata; returns generation (> 0) if valid, or 0 if invalid.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_native_shader_info(
    handle: u64,
    block_count: *mut c_int,
    texture_slot_count: *mut c_int,
) -> u64 {
    if with_gl(|_| ()).is_none() {
        return 0;
    }
    let gen_id = crate::shader::shader_generation(handle);
    if gen_id == 0 {
        return 0;
    }
    let blocks = crate::uniform_block::block_count(handle).unwrap_or(0) as c_int;
    let slots = crate::shader::shader_texture_slot_count(handle).unwrap_or(0) as c_int;
    crate::write_out(block_count, blocks);
    crate::write_out(texture_slot_count, slots);
    gen_id
}

/// Lookup image texture metadata; returns generation (> 0) if valid, or 0 if invalid.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_native_texture_info(
    handle: u64,
    width: *mut c_int,
    height: *mut c_int,
    format: *mut c_int,
    ready: *mut bool,
) -> u64 {
    if with_gl(|_| ()).is_none() {
        return 0;
    }
    let Some((gen_id, w, h, fmt, is_ready)) = crate::image_texture::image_info(handle) else {
        return 0;
    };
    crate::write_out(width, w);
    crate::write_out(height, h);
    crate::write_out(format, fmt);
    crate::write_out(ready, is_ready);
    gen_id
}

/// Lookup retained mesh metadata; returns generation (> 0) if valid, or 0 if invalid.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_native_mesh_info(
    handle: u64,
    vert_count: *mut c_int,
    attr_count: *mut c_int,
    inst_count: *mut c_int,
) -> u64 {
    if with_gl(|_| ()).is_none() {
        return 0;
    }
    let Some((gen_id, verts, attrs, insts)) = crate::mesh::mesh_info(handle) else {
        return 0;
    };
    crate::write_out(vert_count, verts);
    crate::write_out(attr_count, attrs);
    crate::write_out(inst_count, insts);
    gen_id
}

fn copy_c_name(src: &str, out: *mut u8, cap: c_int) -> c_int {
    let bytes = src.as_bytes();
    if !out.is_null() && cap > 0 {
        let copy_len = bytes.len().min((cap as usize).saturating_sub(1));
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), out, copy_len);
            *out.add(copy_len) = 0;
        }
    }
    c_int::try_from(bytes.len()).unwrap_or(c_int::MAX)
}

/// Number of registered uniform blocks for `handle`, or -1 if invalid.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_shader_uniform_block_count(handle: u64) -> c_int {
    crate::uniform_block::block_count(handle).map_or(-1, |n| n as c_int)
}

/// Index of the uniform block at `binding` on `handle`, or -1 if absent.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_shader_uniform_block_find_by_binding(handle: u64, binding: c_int) -> c_int {
    crate::uniform_block::find_block_by_binding(handle, binding)
}

/// Index of the uniform block named `name` on `handle`, or -1 if absent.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_shader_uniform_block_find_by_name(
    handle: u64,
    name: *const c_char,
) -> c_int {
    if name.is_null() {
        return -1;
    }
    let Ok(name) = unsafe { CStr::from_ptr(name) }.to_str() else {
        return -1;
    };
    crate::uniform_block::find_block_by_name(handle, name)
}

/// Query uniform block metadata at `index`. Writes NUL-terminated name into
/// `name_out` when non-null and `name_cap > 0`.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_shader_uniform_block_info(
    handle: u64,
    index: c_int,
    binding_out: *mut c_int,
    size_out: *mut c_int,
    member_count_out: *mut c_int,
    name_out: *mut u8,
    name_cap: c_int,
) -> bool {
    let Ok(idx) = usize::try_from(index) else {
        return false;
    };
    let Some((binding, size, member_count, _fp, name)) =
        crate::uniform_block::block_info(handle, idx)
    else {
        return false;
    };
    crate::write_out(binding_out, binding);
    crate::write_out(size_out, size);
    crate::write_out(member_count_out, member_count);
    let _ = copy_c_name(&name, name_out, name_cap);
    true
}

/// Layout fingerprint of uniform block `index` on `handle`, or 0 if invalid.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_shader_uniform_block_fingerprint(handle: u64, index: c_int) -> u64 {
    let Ok(idx) = usize::try_from(index) else {
        return 0;
    };
    crate::uniform_block::block_info(handle, idx).map_or(0, |(_, _, _, fp, _)| fp)
}

/// Query std140 member metadata at `(block_index, member_index)`.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_shader_uniform_block_member_info(
    handle: u64,
    block_index: c_int,
    member_index: c_int,
    offset_out: *mut c_int,
    kind_out: *mut c_int,
    capacity_out: *mut c_int,
    stride_out: *mut c_int,
) -> bool {
    let (Ok(bidx), Ok(midx)) = (usize::try_from(block_index), usize::try_from(member_index)) else {
        return false;
    };
    let Some((offset, kind, capacity, stride, _)) =
        crate::uniform_block::block_member_info(handle, bidx, midx)
    else {
        return false;
    };
    crate::write_out(offset_out, offset);
    crate::write_out(kind_out, kind);
    crate::write_out(capacity_out, capacity);
    crate::write_out(stride_out, stride);
    true
}

/// Copy member name at `(block_index, member_index)` as NUL-terminated UTF-8.
/// Returns full byte length excluding NUL, or -1 if invalid.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_shader_uniform_block_member_name(
    handle: u64,
    block_index: c_int,
    member_index: c_int,
    name_out: *mut u8,
    name_cap: c_int,
) -> c_int {
    let (Ok(bidx), Ok(midx)) = (usize::try_from(block_index), usize::try_from(member_index)) else {
        return -1;
    };
    let Some((_, _, _, _, name)) = crate::uniform_block::block_member_info(handle, bidx, midx)
    else {
        return -1;
    };
    copy_c_name(&name, name_out, name_cap)
}

/// Byte offset of `member_name` in uniform block `block_index`, or -1 if absent.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_shader_uniform_block_member_offset(
    handle: u64,
    block_index: c_int,
    member_name: *const c_char,
) -> c_int {
    let Ok(bidx) = usize::try_from(block_index) else {
        return -1;
    };
    if member_name.is_null() {
        return -1;
    }
    let Ok(name) = unsafe { CStr::from_ptr(member_name) }.to_str() else {
        return -1;
    };
    crate::uniform_block::block_member_offset(handle, bidx, name)
}

/// Number of registered texture slots on `handle`, or -1 if invalid.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_shader_texture_slot_count(handle: u64) -> c_int {
    crate::shader::shader_texture_slot_count(handle).map_or(-1, |n| n as c_int)
}

/// Texture unit assigned to sampler `name` on `handle`, or -1 if absent.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_shader_texture_slot_find(handle: u64, name: *const c_char) -> c_int {
    if name.is_null() {
        return -1;
    }
    let Ok(name) = unsafe { CStr::from_ptr(name) }.to_str() else {
        return -1;
    };
    crate::shader::shader_texture_slot_find(handle, name)
}

/// Query texture slot `index` on `handle`. Writes texture unit to `unit_out`
/// and NUL-terminated sampler name into `name_out`. Returns full name length
/// excluding NUL, or -1 if invalid.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_shader_texture_slot_info(
    handle: u64,
    index: c_int,
    unit_out: *mut c_int,
    name_out: *mut u8,
    name_cap: c_int,
) -> c_int {
    let Ok(idx) = usize::try_from(index) else {
        return -1;
    };
    let Some((unit, name)) = crate::shader::shader_texture_slot_at(handle, idx) else {
        return -1;
    };
    crate::write_out(unit_out, unit);
    copy_c_name(&name, name_out, name_cap)
}

/// Stable versioned function-pointer table handed to another native library in
/// the same process via `stone_native_renderer_api(1)`.
#[repr(C)]
pub struct StoneNativeRendererApiV1 {
    pub abi_version: u32,
    pub struct_size: u32,

    pub stage_begin: extern "C" fn() -> u32,
    pub stage_end: extern "C" fn(u32) -> u32,
    pub frame_active: extern "C" fn() -> bool,
    pub frame_target: extern "C" fn() -> u64,
    pub frame_size: extern "C" fn(*mut c_int, *mut c_int) -> bool,

    pub context_generation: extern "C" fn() -> u64,
    pub handle_valid: extern "C" fn(c_int, u64) -> bool,
    pub handle_generation: extern "C" fn(c_int, u64) -> u64,
    pub find_shader: extern "C" fn(*const c_char) -> u64,
    pub find_texture: extern "C" fn(*const c_char) -> u64,
    pub target_info:
        extern "C" fn(u64, *mut c_int, *mut c_int, *mut c_int, *mut c_int, *mut bool) -> u64,
    pub shader_info: extern "C" fn(u64, *mut c_int, *mut c_int) -> u64,
    pub texture_info: extern "C" fn(u64, *mut c_int, *mut c_int, *mut c_int, *mut bool) -> u64,
    pub mesh_info: extern "C" fn(u64, *mut c_int, *mut c_int, *mut c_int) -> u64,

    pub target_create: extern "C" fn(c_int, c_int, *const c_int, c_int, bool) -> u64,
    pub target_resize: extern "C" fn(u64, c_int, c_int) -> bool,
    pub target_destroy: extern "C" fn(u64),
    pub target_attachment: extern "C" fn(u64, c_int) -> u64,
    pub set_canvas: extern "C" fn(u64),
    pub set_canvas_active: extern "C" fn(u64, c_int, c_int),
    pub clear_canvas: extern "C" fn(c_float, c_float, c_float, c_float),
    pub clear_canvas_int: extern "C" fn(c_int, c_int),
    pub clear_depth: extern "C" fn(c_float),

    pub shader_uniform_block_count: extern "C" fn(u64) -> c_int,
    pub shader_uniform_block_find_by_binding: extern "C" fn(u64, c_int) -> c_int,
    pub shader_uniform_block_find_by_name: extern "C" fn(u64, *const c_char) -> c_int,
    pub shader_uniform_block_info:
        extern "C" fn(u64, c_int, *mut c_int, *mut c_int, *mut c_int, *mut u8, c_int) -> bool,
    pub shader_uniform_block_fingerprint: extern "C" fn(u64, c_int) -> u64,
    pub shader_uniform_block_member_info:
        extern "C" fn(u64, c_int, c_int, *mut c_int, *mut c_int, *mut c_int, *mut c_int) -> bool,
    pub shader_uniform_block_member_name: extern "C" fn(u64, c_int, c_int, *mut u8, c_int) -> c_int,
    pub shader_uniform_block_member_offset: extern "C" fn(u64, c_int, *const c_char) -> c_int,
    pub shader_texture_slot_count: extern "C" fn(u64) -> c_int,
    pub shader_texture_slot_find: extern "C" fn(u64, *const c_char) -> c_int,
    pub shader_texture_slot_info: extern "C" fn(u64, c_int, *mut c_int, *mut u8, c_int) -> c_int,

    pub set_shader: extern "C" fn(u64),
    pub upload_uniform_block: extern "C" fn(u64, *const u8, c_int, u64) -> bool,
    pub get_white_image: extern "C" fn() -> u64,
    pub bind_image_texture_to_unit: extern "C" fn(u64, c_int) -> bool,
    pub bind_canvas_texture_to_unit: extern "C" fn(u64, c_int) -> bool,
    pub bind_canvas_depth_to_unit: extern "C" fn(u64, c_int) -> bool,
    pub bind_sampler: extern "C" fn(c_int, c_int, u64),
    pub set_depth_mode: extern "C" fn(*const c_char, bool),
    pub set_blend_mode_i: extern "C" fn(c_int),
    pub set_draw_buffer_mask: extern "C" fn(c_int),
    pub set_color_mask: extern "C" fn(bool, bool, bool, bool),
    pub set_scissor: extern "C" fn(bool, c_int, c_int, c_int, c_int),
    pub set_gl_viewport: extern "C" fn(c_int, c_int, c_int, c_int),
    pub draw_mesh:
        extern "C" fn(*const c_float, c_int, c_int, *const c_int, c_int, c_int, u64) -> c_int,
    pub draw_fullscreen_quad: extern "C" fn(),
    pub draw_fullscreen_rect: extern "C" fn(c_float, c_float, c_float, c_float),

    pub mesh_create: extern "C" fn(c_int, *const c_float, c_int, c_int, *const c_int, c_int) -> u64,
    pub mesh_set_instances:
        extern "C" fn(u64, *const c_float, c_int, c_int, *const c_int, c_int) -> c_int,
    pub mesh_update_instances: extern "C" fn(u64, *const c_float, c_int) -> c_int,
    pub mesh_draw: extern "C" fn(u64, u64, c_int) -> c_int,
    pub mesh_free: extern "C" fn(u64),
}

static STONE_NATIVE_RENDERER_API_V1: StoneNativeRendererApiV1 = StoneNativeRendererApiV1 {
    abi_version: STONE_NATIVE_RENDERER_ABI_VERSION,
    struct_size: std::mem::size_of::<StoneNativeRendererApiV1>() as u32,

    stage_begin: stone_native_stage_begin,
    stage_end: stone_native_stage_end,
    frame_active: stone_native_frame_active,
    frame_target: stone_native_frame_target,
    frame_size: stone_native_frame_size,

    context_generation: stone_native_context_generation,
    handle_valid: stone_native_handle_valid,
    handle_generation: stone_native_handle_generation,
    find_shader: stone_native_find_shader,
    find_texture: stone_native_find_texture,
    target_info: stone_native_target_info,
    shader_info: stone_native_shader_info,
    texture_info: stone_native_texture_info,
    mesh_info: stone_native_mesh_info,

    target_create: crate::canvas::stone_target_create,
    target_resize: crate::canvas::stone_target_resize,
    target_destroy: crate::canvas::stone_target_destroy,
    target_attachment: crate::canvas::stone_target_attachment,
    set_canvas: crate::canvas::stone_set_canvas,
    set_canvas_active: crate::canvas::stone_set_canvas_active,
    clear_canvas: crate::canvas::stone_clear_canvas,
    clear_canvas_int: crate::canvas::stone_clear_canvas_int,
    clear_depth: crate::gl_state::stone_clear_depth,

    shader_uniform_block_count: stone_shader_uniform_block_count,
    shader_uniform_block_find_by_binding: stone_shader_uniform_block_find_by_binding,
    shader_uniform_block_find_by_name: stone_shader_uniform_block_find_by_name,
    shader_uniform_block_info: stone_shader_uniform_block_info,
    shader_uniform_block_fingerprint: stone_shader_uniform_block_fingerprint,
    shader_uniform_block_member_info: stone_shader_uniform_block_member_info,
    shader_uniform_block_member_name: stone_shader_uniform_block_member_name,
    shader_uniform_block_member_offset: stone_shader_uniform_block_member_offset,
    shader_texture_slot_count: stone_shader_texture_slot_count,
    shader_texture_slot_find: stone_shader_texture_slot_find,
    shader_texture_slot_info: stone_shader_texture_slot_info,

    set_shader: crate::shader::stone_set_shader,
    upload_uniform_block: crate::uniform_block::stone_shader_upload_uniform_block,
    get_white_image: crate::image_texture::stone_get_white_image,
    bind_image_texture_to_unit: crate::image_texture::stone_bind_image_texture_to_unit,
    bind_canvas_texture_to_unit: crate::canvas::stone_bind_canvas_texture_to_unit,
    bind_canvas_depth_to_unit: crate::canvas::stone_bind_canvas_depth_to_unit,
    bind_sampler: crate::gl_state::stone_bind_sampler,
    set_depth_mode: crate::gl_state::stone_set_depth_mode,
    set_blend_mode_i: crate::gl_state::stone_set_blend_mode_i,
    set_draw_buffer_mask: crate::gl_state::stone_set_draw_buffer_mask,
    set_color_mask: crate::gl_state::stone_set_color_mask,
    set_scissor: crate::gl_state::stone_set_scissor,
    set_gl_viewport: crate::gl_state::stone_set_gl_viewport,
    draw_mesh: crate::sprite::stone_draw_mesh,
    draw_fullscreen_quad: crate::sprite::stone_draw_fullscreen_quad,
    draw_fullscreen_rect: crate::sprite::stone_draw_fullscreen_rect,

    mesh_create: crate::mesh::stone_mesh_create,
    mesh_set_instances: crate::mesh::stone_mesh_set_instances,
    mesh_update_instances: crate::mesh::stone_mesh_update_instances,
    mesh_draw: crate::mesh::stone_mesh_draw,
    mesh_free: crate::mesh::stone_mesh_free,
};

/// Return the static function table for `abi_version`, or null if unsupported.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_native_renderer_api(abi_version: u32) -> *const c_void {
    match abi_version {
        STONE_NATIVE_RENDERER_ABI_VERSION => {
            (&STONE_NATIVE_RENDERER_API_V1 as *const StoneNativeRendererApiV1).cast()
        }
        _ => std::ptr::null(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_table_version_and_size_match_struct_layout() {
        assert!(stone_native_renderer_api(0).is_null());
        assert!(stone_native_renderer_api(STONE_NATIVE_RENDERER_ABI_VERSION + 1).is_null());
        let ptr = stone_native_renderer_api(STONE_NATIVE_RENDERER_ABI_VERSION);
        assert!(!ptr.is_null());
        let api = unsafe { &*ptr.cast::<StoneNativeRendererApiV1>() };
        assert_eq!(api.abi_version, STONE_NATIVE_RENDERER_ABI_VERSION);
        assert_eq!(
            api.struct_size as usize,
            std::mem::size_of::<StoneNativeRendererApiV1>()
        );
    }

    #[test]
    fn copy_c_name_truncates_safely_and_nul_terminates() {
        let mut buf = [0xAAu8; 4];
        assert_eq!(copy_c_name("uProjection", buf.as_mut_ptr(), 4), 11);
        assert_eq!(&buf, b"uPr\0");
        assert_eq!(copy_c_name("uView", std::ptr::null_mut(), 0), 5);
    }
}
