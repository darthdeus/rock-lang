//! Stable C ABI surface for native renderers running in the same process as Stone.
//!
//! A second native library (such as a game's native renderer) obtains the static
//! function table via `stone_native_renderer_api(STONE_NATIVE_RENDERER_ABI_VERSION)`
//! without linking a second `stone-rs` instance.

use std::cell::{Cell, RefCell};
use std::ffi::{c_char, c_float, c_int, c_void, CStr};
use std::num::NonZeroU32;

use crate::gl_state::{PipelineGlState, DRAW_BUFFERS};
use crate::shader::MAX_TEXTURE_UNITS;
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
pub const STONE_STAGE_UNWOUND: u32 = 1 << 4;
pub const STONE_STAGE_GL_ERROR: u32 = 1 << 5;

const SAVED_UBO_SLOTS: usize = 8;

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
    // Only units some registered shader samples; the rest cannot affect a
    // later Stone draw.
    texture_units: u32,
    textures_2d: [Option<glow::Texture>; MAX_TEXTURE_UNITS],
    samplers: [Option<glow::Sampler>; MAX_TEXTURE_UNITS],
    vao: Option<glow::VertexArray>,
    draw_buffer_count: usize,
    draw_buffers: [u32; DRAW_BUFFERS],
    pipeline: PipelineGlState,
    clear_depth: f32,
    stencil_enabled: bool,
    polygon_offset_fill: bool,
    sample_alpha_to_coverage: bool,
    rasterizer_discard: bool,
    #[cfg(debug_assertions)]
    defaults: DefaultState,
}

/// Context state Stone leaves at its GL default and never saves. A stage that
/// changes it is a native-code bug; debug builds report it rather than hide it.
#[cfg(debug_assertions)]
#[derive(Clone, Copy, PartialEq, Debug)]
struct DefaultState {
    ints: [i32; DEFAULT_INTS.len()],
    enables: [bool; DEFAULT_ENABLES.len()],
    polygon_mode: [i32; 2],
    depth_range: [u32; 2],
    polygon_offset: [u32; 2],
    line_width: u32,
}

#[cfg(debug_assertions)]
const DEFAULT_INTS: [(u32, &str); 15] = [
    (glow::FRONT_FACE, "FRONT_FACE"),
    (glow::CULL_FACE_MODE, "CULL_FACE_MODE"),
    (glow::PROVOKING_VERTEX, "PROVOKING_VERTEX"),
    (glow::STENCIL_FUNC, "STENCIL_FUNC"),
    (glow::STENCIL_REF, "STENCIL_REF"),
    (glow::STENCIL_VALUE_MASK, "STENCIL_VALUE_MASK"),
    (glow::STENCIL_WRITEMASK, "STENCIL_WRITEMASK"),
    (glow::STENCIL_FAIL, "STENCIL_FAIL"),
    (glow::STENCIL_PASS_DEPTH_FAIL, "STENCIL_PASS_DEPTH_FAIL"),
    (glow::STENCIL_PASS_DEPTH_PASS, "STENCIL_PASS_DEPTH_PASS"),
    (glow::PACK_SKIP_ROWS, "PACK_SKIP_ROWS"),
    (glow::PACK_SKIP_PIXELS, "PACK_SKIP_PIXELS"),
    (glow::UNPACK_SKIP_ROWS, "UNPACK_SKIP_ROWS"),
    (glow::UNPACK_SKIP_PIXELS, "UNPACK_SKIP_PIXELS"),
    (glow::UNPACK_IMAGE_HEIGHT, "UNPACK_IMAGE_HEIGHT"),
];

#[cfg(debug_assertions)]
const DEFAULT_ENABLES: [(u32, &str); 12] = [
    (glow::DEPTH_CLAMP, "DEPTH_CLAMP"),
    (glow::PRIMITIVE_RESTART, "PRIMITIVE_RESTART"),
    (glow::COLOR_LOGIC_OP, "COLOR_LOGIC_OP"),
    (glow::PROGRAM_POINT_SIZE, "PROGRAM_POINT_SIZE"),
    (glow::CLIP_DISTANCE0, "CLIP_DISTANCE0"),
    (glow::CLIP_DISTANCE1, "CLIP_DISTANCE1"),
    (glow::CLIP_DISTANCE2, "CLIP_DISTANCE2"),
    (glow::CLIP_DISTANCE3, "CLIP_DISTANCE3"),
    (glow::CLIP_DISTANCE4, "CLIP_DISTANCE4"),
    (glow::CLIP_DISTANCE5, "CLIP_DISTANCE5"),
    (glow::CLIP_DISTANCE6, "CLIP_DISTANCE6"),
    (glow::CLIP_DISTANCE7, "CLIP_DISTANCE7"),
];

#[cfg(debug_assertions)]
unsafe fn read_default_state(gl: &DebugGl) -> DefaultState {
    let mut polygon_mode = [0i32; 2];
    gl.get_parameter_i32_slice(glow::POLYGON_MODE, &mut polygon_mode);
    let mut depth_range = [0f32; 2];
    gl.get_parameter_f32_slice(glow::DEPTH_RANGE, &mut depth_range);
    let mut factor = [0f32; 1];
    gl.get_parameter_f32_slice(glow::POLYGON_OFFSET_FACTOR, &mut factor);
    let mut units = [0f32; 1];
    gl.get_parameter_f32_slice(glow::POLYGON_OFFSET_UNITS, &mut units);
    let mut line_width = [0f32; 1];
    gl.get_parameter_f32_slice(glow::LINE_WIDTH, &mut line_width);
    DefaultState {
        ints: DEFAULT_INTS.map(|(param, _)| gl.get_parameter_i32(param)),
        enables: DEFAULT_ENABLES.map(|(cap, _)| gl.is_enabled(cap)),
        polygon_mode,
        depth_range: depth_range.map(f32::to_bits),
        polygon_offset: [factor[0].to_bits(), units[0].to_bits()],
        line_width: line_width[0].to_bits(),
    }
}

#[cfg(debug_assertions)]
fn report_default_state_changes(entry: &DefaultState, exit: &DefaultState) {
    let mut changed: Vec<&str> = Vec::new();
    for (i, (_, name)) in DEFAULT_INTS.iter().enumerate() {
        if entry.ints[i] != exit.ints[i] {
            changed.push(name);
        }
    }
    for (i, (_, name)) in DEFAULT_ENABLES.iter().enumerate() {
        if entry.enables[i] != exit.enables[i] {
            changed.push(name);
        }
    }
    if entry.polygon_mode != exit.polygon_mode {
        changed.push("POLYGON_MODE");
    }
    if entry.depth_range != exit.depth_range {
        changed.push("DEPTH_RANGE");
    }
    if entry.polygon_offset != exit.polygon_offset {
        changed.push("POLYGON_OFFSET_FACTOR/UNITS");
    }
    if entry.line_width != exit.line_width {
        changed.push("LINE_WIDTH");
    }
    if !changed.is_empty() {
        ::log::error!(
            target: "STONE",
            "native stage changed GL state Stone neither sets nor restores: {}",
            changed.join(", ")
        );
    }
}

/// Drain pending GL errors (bounded: a lost context can report one forever)
/// and report them against `site`. Runs before any checked DebugGl call, which
/// would otherwise consume the error and blame its own operation.
unsafe fn drain_gl_errors(gl: &DebugGl, site: &str) -> bool {
    let mut codes = [0u32; 8];
    let mut count = 0;
    while count < codes.len() {
        let code = gl.get_error();
        if code == glow::NO_ERROR {
            break;
        }
        codes[count] = code;
        count += 1;
    }
    if count == 0 {
        return false;
    }
    stone_gl::GL_ERROR_COUNT.fetch_add(count as u64, std::sync::atomic::Ordering::Relaxed);
    if crate::rock_log_reexport::once_should_emit_for(
        concat!(file!(), ":", line!()),
        codes[0] as u64,
    ) {
        let list: Vec<String> = codes[..count].iter().map(|c| format!("0x{c:X}")).collect();
        ::log::error!(target: "GL", "gl error {site}: {}", list.join(", "));
    }
    true
}

#[derive(Clone, Copy)]
struct GlLimits {
    ubo_bindings: usize,
    texture_units: u32,
    draw_buffers: usize,
}

thread_local! {
    static NATIVE_STAGE_STACK: RefCell<Vec<(u32, NativeStageState)>> = const { RefCell::new(Vec::new()) };
    static LAST_STAGE_TOKEN: Cell<u32> = const { Cell::new(0) };
    static GL_LIMITS: Cell<Option<GlLimits>> = const { Cell::new(None) };
}

pub(crate) fn reset_on_teardown() {
    NATIVE_STAGE_STACK.with(|stack| stack.borrow_mut().clear());
    GL_LIMITS.with(|limits| limits.set(None));
}

/// A scope still open at frame start was left by an early return or a script
/// error; its snapshot describes a frame that is gone.
pub(crate) fn begin_frame() {
    let leaked = NATIVE_STAGE_STACK.with(|stack| {
        let mut stack = stack.borrow_mut();
        let leaked = stack.len();
        stack.clear();
        leaked
    });
    if leaked > 0 {
        crate::log_once!(
            "STONE",
            "{leaked} native stage scope(s) were still open at frame start; dropped without restoring"
        );
    }
}

// Tokens are per-thread serial numbers, so a stale end cannot close a later
// scope that reuses its depth.
fn next_stage_token() -> u32 {
    LAST_STAGE_TOKEN.with(|last| {
        let token = last.get().wrapping_add(1).max(1);
        last.set(token);
        token
    })
}

unsafe fn gl_limits(gl: &DebugGl) -> GlLimits {
    GL_LIMITS.with(|cached| {
        if let Some(limits) = cached.get() {
            return limits;
        }
        let read = |param| gl.get_parameter_i32(param).max(0) as usize;
        let units = read(glow::MAX_COMBINED_TEXTURE_IMAGE_UNITS).min(MAX_TEXTURE_UNITS);
        let limits = GlLimits {
            ubo_bindings: read(glow::MAX_UNIFORM_BUFFER_BINDINGS).min(SAVED_UBO_SLOTS),
            texture_units: if units >= 32 {
                u32::MAX
            } else {
                (1u32 << units) - 1
            },
            draw_buffers: read(glow::MAX_DRAW_BUFFERS).clamp(1, DRAW_BUFFERS),
        };
        cached.set(Some(limits));
        limits
    })
}

fn set_units(mask: u32) -> impl Iterator<Item = usize> {
    (0..MAX_TEXTURE_UNITS).filter(move |unit| mask & (1 << unit) != 0)
}

pub(crate) unsafe fn read_framebuffer(gl: &DebugGl, param: u32) -> Option<glow::Framebuffer> {
    NonZeroU32::new(gl.get_parameter_i32(param) as u32).map(glow::NativeFramebuffer)
}

unsafe fn read_program(gl: &DebugGl) -> Option<glow::Program> {
    NonZeroU32::new(gl.get_parameter_i32(glow::CURRENT_PROGRAM) as u32).map(glow::NativeProgram)
}

unsafe fn read_buffer(gl: &DebugGl, param: u32) -> Option<glow::Buffer> {
    NonZeroU32::new(gl.get_parameter_i32(param) as u32).map(glow::NativeBuffer)
}

unsafe fn read_texture_2d(gl: &DebugGl) -> Option<glow::Texture> {
    NonZeroU32::new(gl.get_parameter_i32(glow::TEXTURE_BINDING_2D) as u32).map(glow::NativeTexture)
}

unsafe fn read_sampler(gl: &DebugGl) -> Option<glow::Sampler> {
    NonZeroU32::new(gl.get_parameter_i32(glow::SAMPLER_BINDING) as u32).map(glow::NativeSampler)
}

unsafe fn read_indexed_ubo(gl: &DebugGl, index: u32) -> (Option<glow::Buffer>, i32, i32) {
    let buffer =
        NonZeroU32::new(gl.get_parameter_indexed_i32(glow::UNIFORM_BUFFER_BINDING, index) as u32)
            .map(glow::NativeBuffer);
    let start = gl.get_parameter_indexed_i32(glow::UNIFORM_BUFFER_START, index);
    let size = gl.get_parameter_indexed_i32(glow::UNIFORM_BUFFER_SIZE, index);
    (buffer, start, size)
}

unsafe fn read_draw_buffers(
    gl: &DebugGl,
    limits: GlLimits,
    is_default_fbo: bool,
) -> (usize, [u32; DRAW_BUFFERS]) {
    let mut buffers = [glow::NONE; DRAW_BUFFERS];
    let count = if is_default_fbo {
        1
    } else {
        limits.draw_buffers
    };
    for (i, dst) in buffers.iter_mut().enumerate().take(count) {
        *dst = gl.get_parameter_i32(glow::DRAW_BUFFER0 + i as u32) as u32;
    }
    (count, buffers)
}

unsafe fn capture_stage_state(gl: &DebugGl) -> NativeStageState {
    let limits = gl_limits(gl);
    let gl_draw_fbo = read_framebuffer(gl, glow::DRAW_FRAMEBUFFER_BINDING);
    let gl_read_fbo = read_framebuffer(gl, glow::READ_FRAMEBUFFER_BINDING);
    let mut viewport = [0i32; 4];
    gl.get_parameter_i32_slice(glow::VIEWPORT, &mut viewport);
    let scissor_enabled = gl.is_enabled(glow::SCISSOR_TEST);
    let mut scissor_box = [0i32; 4];
    gl.get_parameter_i32_slice(glow::SCISSOR_BOX, &mut scissor_box);

    let mut ubo_slots = [(None, 0i32, 0i32); SAVED_UBO_SLOTS];
    for (i, slot) in ubo_slots.iter_mut().enumerate().take(limits.ubo_bindings) {
        *slot = read_indexed_ubo(gl, i as u32);
    }

    let active_texture = gl.get_parameter_i32(glow::ACTIVE_TEXTURE) as u32;
    let texture_units = crate::shader::texture_units_in_use() & limits.texture_units;
    let mut textures_2d = [None; MAX_TEXTURE_UNITS];
    let mut samplers = [None; MAX_TEXTURE_UNITS];
    for unit in set_units(texture_units) {
        gl.active_texture(glow::TEXTURE0 + unit as u32);
        textures_2d[unit] = read_texture_2d(gl);
        samplers[unit] = read_sampler(gl);
    }
    gl.active_texture(active_texture);

    let mut clear_depth = [0.0f32; 1];
    gl.get_parameter_f32_slice(glow::DEPTH_CLEAR_VALUE, &mut clear_depth);
    let (draw_buffer_count, draw_buffers) = read_draw_buffers(gl, limits, gl_draw_fbo.is_none());

    NativeStageState {
        canvas_handle: crate::canvas::current_canvas_handle(),
        fbo: crate::canvas::current_fbo(),
        gl_draw_fbo,
        gl_read_fbo,
        target_size: crate::canvas::current_target_size_raw(),
        viewport,
        scissor_enabled,
        scissor_box,
        cached_shader: CURRENT_SHADER.with(|c| *c.borrow()),
        gl_program: read_program(gl),
        generic_ubo: read_buffer(gl, glow::UNIFORM_BUFFER_BINDING),
        ubo_count: limits.ubo_bindings,
        ubo_slots,
        active_texture,
        texture_units,
        textures_2d,
        samplers,
        vao: NonZeroU32::new(gl.get_parameter_i32(glow::VERTEX_ARRAY_BINDING) as u32)
            .map(glow::NativeVertexArray),
        draw_buffer_count,
        draw_buffers,
        pipeline: PipelineGlState::read(gl),
        clear_depth: clear_depth[0],
        stencil_enabled: gl.is_enabled(glow::STENCIL_TEST),
        polygon_offset_fill: gl.is_enabled(glow::POLYGON_OFFSET_FILL),
        sample_alpha_to_coverage: gl.is_enabled(glow::SAMPLE_ALPHA_TO_COVERAGE),
        rasterizer_discard: gl.is_enabled(glow::RASTERIZER_DISCARD),
        #[cfg(debug_assertions)]
        defaults: read_default_state(gl),
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

    let limits = gl_limits(gl);
    let (cur_db_count, cur_dbs) = read_draw_buffers(gl, limits, saved.gl_draw_fbo.is_none());
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

    for unit in set_units(saved.texture_units) {
        gl.active_texture(glow::TEXTURE0 + unit as u32);
        if read_texture_2d(gl) != saved.textures_2d[unit] {
            gl.bind_texture(glow::TEXTURE_2D, saved.textures_2d[unit]);
        }
        if read_sampler(gl) != saved.samplers[unit] {
            gl.bind_sampler(unit as u32, saved.samplers[unit]);
        }
    }
    gl.active_texture(saved.active_texture);

    let cur_vao = NonZeroU32::new(gl.get_parameter_i32(glow::VERTEX_ARRAY_BINDING) as u32)
        .map(glow::NativeVertexArray);
    if cur_vao != saved.vao {
        gl.bind_vertex_array(saved.vao);
    }

    saved.pipeline.restore(gl);
    gl.clear_depth_f32(saved.clear_depth);
    gl.set_enabled(glow::STENCIL_TEST, saved.stencil_enabled);
    gl.set_enabled(glow::POLYGON_OFFSET_FILL, saved.polygon_offset_fill);
    gl.set_enabled(
        glow::SAMPLE_ALPHA_TO_COVERAGE,
        saved.sample_alpha_to_coverage,
    );
    gl.set_enabled(glow::RASTERIZER_DISCARD, saved.rasterizer_discard);

    // Outside its own save/restore scopes Stone keeps these at fixed values, so
    // they are reset rather than captured. Alignment 1 is correct for every
    // tightly packed transfer Stone makes, and its text and canvas paths set it
    // and leave it.
    gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
    gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, None);
    gl.pixel_store_i32(glow::PACK_ALIGNMENT, 1);
    gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
    gl.pixel_store_i32(glow::PACK_ROW_LENGTH, 0);
    gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
    // The present shader encodes sRGB itself.
    gl.disable(glow::FRAMEBUFFER_SRGB);
    #[cfg(debug_assertions)]
    report_default_state_changes(&saved.defaults, &read_default_state(gl));

    flags
}

/// Begin a native rendering stage: opens the render-thread frame scope and
/// snapshots the GL and engine-cached state so `stone_native_stage_end` can
/// restore only what changed. Returns a non-zero scope token, or 0 if no GL
/// context is live on this thread.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_native_stage_begin() -> u32 {
    let Some(state) = with_gl(|gl| unsafe {
        drain_gl_errors(gl, "pending before a native stage");
        let state = capture_stage_state(gl);
        // Native attribute calls without their own VAO then fail instead of
        // rewriting the VAO a Stone draw left bound.
        gl.bind_vertex_array(None);
        state
    }) else {
        return 0;
    };
    let token = next_stage_token();
    NATIVE_STAGE_STACK.with(|stack| stack.borrow_mut().push((token, state)));
    token
}

/// End the native rendering stage `scope`, restoring the GL and engine-cached
/// state saved when it began. Scopes opened inside it and still open are
/// closed too (`STONE_STAGE_UNWOUND`). Returns a bitmask (`STONE_STAGE_*`), or
/// 0 when `scope` is not open (never begun, already closed, or dropped at
/// frame start) or no GL context is live; nothing is restored then.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_native_stage_end(scope: u32) -> u32 {
    let found = NATIVE_STAGE_STACK.with(|stack| {
        let stack = stack.borrow();
        let index = stack
            .iter()
            .rposition(|&(token, _)| token == scope && token != 0)?;
        Some((index, stack.len() - 1 - index, stack[index].1))
    });
    let Some((index, unwound, saved)) = found else {
        crate::log_once!(
            "STONE",
            "stone_native_stage_end: scope {scope} is not open; nothing restored"
        );
        return 0;
    };
    let Some(mut flags) = with_gl(|gl| unsafe {
        let errored = drain_gl_errors(gl, "raised inside a native stage");
        let flags = restore_stage_state(gl, &saved);
        if errored {
            flags | STONE_STAGE_GL_ERROR
        } else {
            flags
        }
    }) else {
        return 0;
    };
    NATIVE_STAGE_STACK.with(|stack| stack.borrow_mut().truncate(index));
    if unwound > 0 {
        crate::log_once!(
            "STONE",
            "stone_native_stage_end: closed {unwound} inner scope(s) that were never ended"
        );
        flags |= STONE_STAGE_UNWOUND;
    }
    flags
}

/// Address of the GL function `name` in Stone's context, for a native library
/// that issues GL itself inside a stage. Null for an unknown name, off the
/// render thread, or without a context. Valid until the context generation
/// changes.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_native_gl_get_proc_address(name: *const c_char) -> *const c_void {
    if name.is_null() {
        ::log::error!(target: "STONE", "stone_native_gl_get_proc_address: null name");
        return std::ptr::null();
    }
    let Ok(name) = unsafe { CStr::from_ptr(name) }.to_str() else {
        return std::ptr::null();
    };
    if with_gl(|_| ()).is_none() {
        return std::ptr::null();
    }
    crate::with_window(|w| w.gl_proc_address(name)).unwrap_or(std::ptr::null())
}

/// True only on the render thread while a native stage / custom draw scope is active.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_native_frame_active() -> bool {
    crate::with_window(|_| ()).is_some()
        && NATIVE_STAGE_STACK.with(|stack| !stack.borrow().is_empty())
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
/// Returns 0 for invalid, wrong-kind, or destroyed resources, including those
/// released by Stone teardown. Changes on target resize, active-rect change,
/// attachment change, texture reload, or shader reload.
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
    pub target_color_view: extern "C" fn(u64) -> u64,
    pub get_live_mesh_bytes: extern "C" fn() -> u64,

    pub gl_get_proc_address: extern "C" fn(*const c_char) -> *const c_void,
    pub image_gl_texture: extern "C" fn(u64) -> u32,
    pub target_gl_texture: extern "C" fn(u64, c_int) -> u32,
    pub target_gl_depth_texture: extern "C" fn(u64) -> u32,
    pub target_gl_framebuffer: extern "C" fn(u64) -> u32,
    pub image_import_gl_texture: extern "C" fn(u32, c_int, c_int, u32) -> u64,
    pub image_import_update: extern "C" fn(u64, c_int, c_int) -> bool,
    pub image_release_import: extern "C" fn(u64) -> bool,
    pub frame_hook_register:
        extern "C" fn(u32, c_int, *const c_void, *const c_void, *mut c_void, *const c_char) -> u64,
    pub frame_hook_unregister: extern "C" fn(u64) -> bool,
    pub frame_set_world_image: extern "C" fn(u64, u32) -> bool,
    pub gpu_scope_begin: unsafe extern "C" fn(*const c_char),
    pub gpu_scope_end: extern "C" fn(),
    pub pixel_readback_new: extern "C" fn(c_int) -> u64,
    pub pixel_readback_area_new: extern "C" fn(c_int) -> u64,
    pub pixel_readback_submit: extern "C" fn(u64, u64, c_int, c_int),
    pub pixel_readback_submit_area: extern "C" fn(u64, u64, c_int, c_int, c_int),
    pub pixel_readback_poll_id: extern "C" fn(u64) -> c_int,
    pub pixel_readback_poll_rgba: extern "C" fn(u64, *mut c_float, c_int) -> c_int,
    pub pixel_readback_consume: extern "C" fn(u64) -> c_float,
    pub pixel_readback_in_flight: extern "C" fn(u64) -> c_int,
    pub pixel_readback_answer_x: extern "C" fn(u64) -> c_int,
    pub pixel_readback_answer_y: extern "C" fn(u64) -> c_int,
    pub pixel_readback_reset_contents: extern "C" fn(u64) -> bool,
    pub pixel_readback_destroy: extern "C" fn(u64),
    pub image_load: extern "C" fn(*const c_char, c_int, c_int, c_int, c_int) -> u64,
    pub image_require: extern "C" fn(u64) -> bool,
    pub image_ready: extern "C" fn(u64) -> bool,
    pub image_unload: extern "C" fn(u64),
}

// Version 1 only appends: each entry keeps its index, so its offset never moves.
macro_rules! api_v1_layout {
    ($($field:ident = $index:expr,)*) => {
        const _: () = {
            const PTR: usize = std::mem::size_of::<usize>();
            assert!(std::mem::offset_of!(StoneNativeRendererApiV1, abi_version) == 0);
            assert!(std::mem::offset_of!(StoneNativeRendererApiV1, struct_size) == 4);
            $(assert!(std::mem::offset_of!(StoneNativeRendererApiV1, $field) == 8 + $index * PTR);)*
            assert!(std::mem::size_of::<StoneNativeRendererApiV1>() == 8 + [$($index),*].len() * PTR);
        };

        /// Field names of `StoneNativeRendererApiV1` in declaration order.
        pub const STONE_NATIVE_RENDERER_API_V1_FIELDS: &[&str] =
            &["abi_version", "struct_size", $(stringify!($field)),*];
    };
}

api_v1_layout! {
    stage_begin = 0,
    stage_end = 1,
    frame_active = 2,
    frame_target = 3,
    frame_size = 4,
    context_generation = 5,
    handle_valid = 6,
    handle_generation = 7,
    find_shader = 8,
    find_texture = 9,
    target_info = 10,
    shader_info = 11,
    texture_info = 12,
    mesh_info = 13,
    target_create = 14,
    target_resize = 15,
    target_destroy = 16,
    target_attachment = 17,
    set_canvas = 18,
    set_canvas_active = 19,
    clear_canvas = 20,
    clear_canvas_int = 21,
    clear_depth = 22,
    shader_uniform_block_count = 23,
    shader_uniform_block_find_by_binding = 24,
    shader_uniform_block_find_by_name = 25,
    shader_uniform_block_info = 26,
    shader_uniform_block_fingerprint = 27,
    shader_uniform_block_member_info = 28,
    shader_uniform_block_member_name = 29,
    shader_uniform_block_member_offset = 30,
    shader_texture_slot_count = 31,
    shader_texture_slot_find = 32,
    shader_texture_slot_info = 33,
    set_shader = 34,
    upload_uniform_block = 35,
    get_white_image = 36,
    bind_image_texture_to_unit = 37,
    bind_canvas_texture_to_unit = 38,
    bind_canvas_depth_to_unit = 39,
    bind_sampler = 40,
    set_depth_mode = 41,
    set_blend_mode_i = 42,
    set_draw_buffer_mask = 43,
    set_color_mask = 44,
    set_scissor = 45,
    set_gl_viewport = 46,
    draw_mesh = 47,
    draw_fullscreen_quad = 48,
    draw_fullscreen_rect = 49,
    mesh_create = 50,
    mesh_set_instances = 51,
    mesh_update_instances = 52,
    mesh_draw = 53,
    mesh_free = 54,
    target_color_view = 55,
    get_live_mesh_bytes = 56,
    gl_get_proc_address = 57,
    image_gl_texture = 58,
    target_gl_texture = 59,
    target_gl_depth_texture = 60,
    target_gl_framebuffer = 61,
    image_import_gl_texture = 62,
    image_import_update = 63,
    image_release_import = 64,
    frame_hook_register = 65,
    frame_hook_unregister = 66,
    frame_set_world_image = 67,
    gpu_scope_begin = 68,
    gpu_scope_end = 69,
    pixel_readback_new = 70,
    pixel_readback_area_new = 71,
    pixel_readback_submit = 72,
    pixel_readback_submit_area = 73,
    pixel_readback_poll_id = 74,
    pixel_readback_poll_rgba = 75,
    pixel_readback_consume = 76,
    pixel_readback_in_flight = 77,
    pixel_readback_answer_x = 78,
    pixel_readback_answer_y = 79,
    pixel_readback_reset_contents = 80,
    pixel_readback_destroy = 81,
    image_load = 82,
    image_require = 83,
    image_ready = 84,
    image_unload = 85,
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
    target_color_view: crate::canvas::stone_target_color_view,
    get_live_mesh_bytes: crate::mesh::stone_get_live_mesh_bytes,

    gl_get_proc_address: stone_native_gl_get_proc_address,
    image_gl_texture: crate::image_texture::stone_get_image_texture,
    target_gl_texture: crate::canvas::stone_target_gl_texture,
    target_gl_depth_texture: crate::canvas::stone_target_gl_depth_texture,
    target_gl_framebuffer: crate::canvas::stone_target_gl_framebuffer,
    image_import_gl_texture: crate::image_texture::stone_image_import_gl_texture,
    image_import_update: crate::image_texture::stone_image_import_update,
    image_release_import: crate::image_texture::stone_image_release_import,
    frame_hook_register: crate::native_frame::stone_native_frame_hook_register,
    frame_hook_unregister: crate::native_frame::stone_native_frame_hook_unregister,
    frame_set_world_image: crate::native_frame::stone_native_frame_set_world_image,
    gpu_scope_begin: crate::stone_gpu_scope_begin,
    gpu_scope_end: crate::stone_gpu_scope_end,
    pixel_readback_new: crate::pixel_readback::stone_pixel_readback_new,
    pixel_readback_area_new: crate::pixel_readback::stone_pixel_readback_area_new,
    pixel_readback_submit: crate::pixel_readback::stone_pixel_readback_submit,
    pixel_readback_submit_area: crate::pixel_readback::stone_pixel_readback_submit_area,
    pixel_readback_poll_id: crate::pixel_readback::stone_pixel_readback_poll_id,
    pixel_readback_poll_rgba: crate::pixel_readback::stone_pixel_readback_poll_rgba,
    pixel_readback_consume: crate::pixel_readback::stone_pixel_readback_consume,
    pixel_readback_in_flight: crate::pixel_readback::stone_pixel_readback_in_flight,
    pixel_readback_answer_x: crate::pixel_readback::stone_pixel_readback_answer_x,
    pixel_readback_answer_y: crate::pixel_readback::stone_pixel_readback_answer_y,
    pixel_readback_reset_contents: crate::pixel_readback::stone_pixel_readback_reset_contents,
    pixel_readback_destroy: crate::pixel_readback::stone_pixel_readback_destroy,
    image_load: crate::image_texture::stone_load_image,
    image_require: crate::image_publication::stone_image_require,
    image_ready: crate::image_publication::stone_image_ready,
    image_unload: crate::image_texture::stone_unload_image,
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
