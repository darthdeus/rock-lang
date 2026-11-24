use std::cell::RefCell;
use std::ffi::{c_char, c_float, c_int, CStr};

use crate::{log_once, with_gl, DebugGl, IMAGES};

/// 0 = NEAREST, 1 = LINEAR, 2 = stable bilinear mip. Keep in sync with IMAGE_FILTER_* in
/// stone/engine/lib.rock and the web `__sw_gl_filter`.
pub(crate) fn gl_filter_for(filter_mode: i32) -> Option<u32> {
    match filter_mode {
        0 => Some(glow::NEAREST),
        1 => Some(glow::LINEAR),
        2 => Some(glow::LINEAR),
        _ => None,
    }
}
pub(crate) fn gl_min_filter_for(filter_mode: i32) -> Option<u32> {
    if filter_mode == 2 {
        Some(glow::LINEAR_MIPMAP_LINEAR)
    } else {
        gl_filter_for(filter_mode)
    }
}
thread_local! {
    static SAMPLERS: RefCell<Option<[[glow::Sampler; 2]; 3]>> = const { RefCell::new(None) };
}

/// The samplers die with the GL context; the next context creates its own.
pub(crate) fn forget_samplers() {
    SAMPLERS.with(|cell| *cell.borrow_mut() = None);
}

fn samplers(gl: &DebugGl) -> Option<[[glow::Sampler; 2]; 3]> {
    SAMPLERS.with(|cell| {
        if let Some(s) = *cell.borrow() {
            return Some(s);
        }
        let anisotropy = gl.max_texture_anisotropy().map(|limit| limit.min(4.0));
        if anisotropy.is_none() {
            log_once!(
                "RENDER",
                "Anisotropic filtering unavailable; using trilinear mip sampling"
            );
        }
        let make = |min_filter: u32, mag_filter: u32, wrap: u32| -> Option<glow::Sampler> {
            let s = match unsafe { gl.create_sampler() } {
                Ok(s) => s,
                Err(e) => {
                    log_once!("RENDER", "gl.create_sampler() failed: {}", e);
                    return None;
                }
            };
            unsafe {
                gl.sampler_parameter_i32(s, glow::TEXTURE_MIN_FILTER, min_filter as i32);
                gl.sampler_parameter_i32(s, glow::TEXTURE_MAG_FILTER, mag_filter as i32);
                gl.sampler_parameter_i32(s, glow::TEXTURE_WRAP_S, wrap as i32);
                gl.sampler_parameter_i32(s, glow::TEXTURE_WRAP_T, wrap as i32);
                if min_filter == glow::LINEAR_MIPMAP_LINEAR {
                    if let Some(level) = anisotropy {
                        gl.sampler_parameter_f32(s, glow::TEXTURE_MAX_ANISOTROPY, level);
                    }
                }
            }
            Some(s)
        };
        let made = [
            [
                make(glow::NEAREST, glow::NEAREST, glow::CLAMP_TO_EDGE)?,
                make(glow::NEAREST, glow::NEAREST, glow::REPEAT)?,
            ],
            [
                make(glow::LINEAR, glow::LINEAR, glow::CLAMP_TO_EDGE)?,
                make(glow::LINEAR, glow::LINEAR, glow::REPEAT)?,
            ],
            [
                make(
                    glow::LINEAR_MIPMAP_LINEAR,
                    glow::LINEAR,
                    glow::CLAMP_TO_EDGE,
                )?,
                make(glow::LINEAR_MIPMAP_LINEAR, glow::LINEAR, glow::REPEAT)?,
            ],
        ];
        *cell.borrow_mut() = Some(made);
        Some(made)
    })
}

#[no_mangle]
pub extern "C" fn stone_sampler_bound(unit: c_int) -> bool {
    if unit < 0 {
        return false;
    }
    with_gl(|gl| unsafe {
        gl.active_texture(glow::TEXTURE0 + unit as u32);
        let bound = gl.get_parameter_i32(glow::SAMPLER_BINDING);
        gl.active_texture(glow::TEXTURE0);
        bound != 0
    })
    .unwrap_or(false)
}

/// A sampler overrides the texture's wrap, so `image` selects the wrap; pass 0 for a canvas
/// (always CLAMP_TO_EDGE). A negative `mode` releases the unit.
#[no_mangle]
pub extern "C" fn stone_bind_sampler(unit: c_int, mode: c_int, image: u64) {
    if unit < 0 {
        return;
    }
    with_gl(|gl| unsafe {
        if mode < 0 {
            gl.bind_sampler(unit as u32, None);
            return;
        }
        if mode > 2 {
            log_once!("RENDER", "stone_bind_sampler: unknown mode {}", mode);
            return;
        }
        let Some(table) = samplers(gl) else { return };
        let repeat = IMAGES
            .with(|images| images.borrow().get(&image).map(|img| img.repeat))
            .unwrap_or(false);
        gl.bind_sampler(unit as u32, Some(table[mode as usize][repeat as usize]));
    });
}

/// Indexed by the Rock BlendMode enum (blend_mode_to_int in commands.rock): the blend
/// equation and `blend_func_separate` factors, `None` for blending off. Every mode sets the
/// equation so lighten's GL_MAX cannot leak into later draws.
const BLEND_MODES: [(u32, Option<[u32; 4]>); 7] = {
    use glow::*;
    [
        (
            FUNC_ADD,
            Some([SRC_ALPHA, ONE_MINUS_SRC_ALPHA, ONE, ONE_MINUS_SRC_ALPHA]),
        ),
        (FUNC_ADD, Some([SRC_ALPHA, ONE, ONE, ONE_MINUS_SRC_ALPHA])),
        (FUNC_ADD, Some([DST_COLOR, ZERO, DST_ALPHA, ZERO])),
        (MAX, Some([ONE, ONE, ONE, ONE])),
        (FUNC_ADD, None),
        (FUNC_ADD, Some([ONE, ONE, ONE, ONE])),
        (
            FUNC_ADD,
            Some([ONE, ONE_MINUS_SRC_ALPHA, ONE, ONE_MINUS_SRC_ALPHA]),
        ),
    ]
};

#[no_mangle]
pub extern "C" fn stone_set_blend_mode_i(mode: i32) {
    let (equation, factors) = match usize::try_from(mode).ok().and_then(|m| BLEND_MODES.get(m)) {
        Some(row) => *row,
        None => {
            warn_unknown_blend_mode(mode);
            BLEND_MODES[0]
        }
    };
    with_gl(|gl| unsafe {
        gl.set_enabled(glow::BLEND, factors.is_some());
        gl.blend_equation(equation);
        if let Some([src_rgb, dst_rgb, src_alpha, dst_alpha]) = factors {
            gl.blend_func_separate(src_rgb, dst_rgb, src_alpha, dst_alpha);
        }
    });
}

#[derive(Clone, Copy)]
pub(crate) struct BlendGlState {
    enabled: bool,
    equation_rgb: u32,
    equation_alpha: u32,
    src_rgb: u32,
    src_alpha: u32,
    dst_rgb: u32,
    dst_alpha: u32,
}

impl BlendGlState {
    unsafe fn read(gl: &DebugGl) -> Self {
        Self {
            enabled: gl.is_enabled(glow::BLEND),
            equation_rgb: gl.get_parameter_i32(glow::BLEND_EQUATION_RGB) as u32,
            equation_alpha: gl.get_parameter_i32(glow::BLEND_EQUATION_ALPHA) as u32,
            src_rgb: gl.get_parameter_i32(glow::BLEND_SRC_RGB) as u32,
            src_alpha: gl.get_parameter_i32(glow::BLEND_SRC_ALPHA) as u32,
            dst_rgb: gl.get_parameter_i32(glow::BLEND_DST_RGB) as u32,
            dst_alpha: gl.get_parameter_i32(glow::BLEND_DST_ALPHA) as u32,
        }
    }

    unsafe fn restore(self, gl: &DebugGl) {
        gl.set_enabled(glow::BLEND, self.enabled);
        gl.blend_equation_separate(self.equation_rgb, self.equation_alpha);
        gl.blend_func_separate(self.src_rgb, self.dst_rgb, self.src_alpha, self.dst_alpha);
    }
}

// Pass scopes are LIFO, including nested submissions.
#[derive(Clone, Copy)]
struct PassGlState {
    blend: BlendGlState,
    depth_enabled: bool,
    depth_func: u32,
    depth_write: bool,
    color_mask: [bool; 4],
    clear_color: [f32; 4],
    cull_enabled: bool,
}

thread_local! {
    static PASS_GL_STATES: RefCell<Vec<PassGlState>> = const { RefCell::new(Vec::new()) };
}

/// Capture only the ambient pipeline state the GL semantic executor changes.
/// Target, viewport and target scissor deliberately remain the target binder's
/// contract (exit to the window), not part of this snapshot.
#[no_mangle]
pub extern "C" fn stone_gl_pass_state_begin() -> u32 {
    let Some(state) = with_gl(|gl| unsafe {
        let mut clear_color = [0.0; 4];
        gl.get_parameter_f32_slice(glow::COLOR_CLEAR_VALUE, &mut clear_color);
        PassGlState {
            blend: BlendGlState::read(gl),
            depth_enabled: gl.is_enabled(glow::DEPTH_TEST),
            depth_func: gl.get_parameter_i32(glow::DEPTH_FUNC) as u32,
            depth_write: gl.get_parameter_bool(glow::DEPTH_WRITEMASK),
            color_mask: gl.get_parameter_bool_array::<4>(glow::COLOR_WRITEMASK),
            clear_color,
            cull_enabled: gl.is_enabled(glow::CULL_FACE),
        }
    }) else {
        return 0;
    };
    PASS_GL_STATES.with(|states| {
        let mut states = states.borrow_mut();
        states.push(state);
        states.len() as u32
    })
}

/// A mismatched scope must not consume another transaction's snapshot.
#[no_mangle]
pub extern "C" fn stone_gl_pass_state_end(scope: u32) -> bool {
    PASS_GL_STATES.with(|states| {
        let mut states = states.borrow_mut();
        if scope == 0 || states.len() != scope as usize {
            return false;
        }
        let state = states[states.len() - 1];
        let restored = with_gl(|gl| unsafe {
            state.blend.restore(gl);
            gl.set_enabled(glow::DEPTH_TEST, state.depth_enabled);
            gl.depth_func(state.depth_func);
            gl.depth_mask(state.depth_write);
            gl.color_mask(
                state.color_mask[0],
                state.color_mask[1],
                state.color_mask[2],
                state.color_mask[3],
            );
            gl.clear_color(
                state.clear_color[0],
                state.clear_color[1],
                state.clear_color[2],
                state.clear_color[3],
            );
            gl.set_enabled(glow::CULL_FACE, state.cull_enabled);
        })
        .is_some();
        if restored {
            states.pop();
        }
        restored
    })
}

/// GL write state as `mask=RGBA clear=r,g,b,a scissor=on/off x,y,w,h`;
/// masked channels are `-`.
#[no_mangle]
pub extern "C" fn stone_gl_write_state() -> *const c_char {
    let read = with_gl(|gl| unsafe {
        let mut mask = [0i32; 4];
        gl.get_parameter_i32_slice(glow::COLOR_WRITEMASK, &mut mask);
        let mut clear = [0f32; 4];
        gl.get_parameter_f32_slice(glow::COLOR_CLEAR_VALUE, &mut clear);
        let mut box_ = [0i32; 4];
        gl.get_parameter_i32_slice(glow::SCISSOR_BOX, &mut box_);
        let letters = ["R", "G", "B", "A"];
        let mask_str: String = (0..4)
            .map(|i| if mask[i] != 0 { letters[i] } else { "-" })
            .collect();
        format!(
            "mask={} clear={:.3},{:.3},{:.3},{:.3} scissor={} {},{},{},{}",
            mask_str,
            clear[0],
            clear[1],
            clear[2],
            clear[3],
            if gl.is_enabled(glow::SCISSOR_TEST) {
                "on"
            } else {
                "off"
            },
            box_[0],
            box_[1],
            box_[2],
            box_[3],
        )
    });
    reply(&WRITE_STATE_REPLY, read)
}

thread_local! {
    static WRITE_STATE_REPLY: RefCell<Option<std::ffi::CString>> = const { RefCell::new(None) };
    static BLEND_STATE_REPLY: RefCell<Option<std::ffi::CString>> = const { RefCell::new(None) };
}

/// The pointer stays valid until the next reply through the same slot.
fn reply(
    slot: &'static std::thread::LocalKey<RefCell<Option<std::ffi::CString>>>,
    text: Option<String>,
) -> *const c_char {
    let Ok(c) = std::ffi::CString::new(text.unwrap_or_else(|| "no-gl".to_string())) else {
        return std::ptr::null();
    };
    slot.with(|b| b.borrow_mut().insert(c).as_ptr())
}

/// GL's blend state as `on/off eq=<e> src=<s> dst=<d>`, with raw enum values.
/// Read GL itself so disagreements with the Rock-side shadow remain visible.
#[no_mangle]
pub extern "C" fn stone_gl_blend_state() -> *const c_char {
    let s = with_gl(|gl| unsafe {
        format!(
            "{} eq={} src={} dst={}",
            if gl.is_enabled(glow::BLEND) {
                "on"
            } else {
                "off"
            },
            gl.get_parameter_i32(glow::BLEND_EQUATION_RGB),
            gl.get_parameter_i32(glow::BLEND_SRC_RGB),
            gl.get_parameter_i32(glow::BLEND_DST_RGB),
        )
    });
    reply(&BLEND_STATE_REPLY, s)
}

fn warn_unknown_blend_mode(mode: i32) {
    if crate::rock_log_reexport::once_should_emit_for(
        concat!(file!(), ":", line!()),
        mode as u32 as u64,
    ) {
        ::log::error!(target: "GFX",
            "unknown blend mode int {} -- Rock/Rust BlendMode drift; falling back to alpha (valid: 0=alpha 1=add 2=multiply 3=lighten 4=replace 5=accumulate 6=premultiplied)",
            mode);
    }
}

#[no_mangle]
pub extern "C" fn stone_set_depth_mode(compare: *const c_char, write: bool) {
    let compare_str = if compare.is_null() {
        "always"
    } else {
        let Ok(compare_str) = (unsafe { CStr::from_ptr(compare).to_str() }) else {
            ::log::error!(target: "RENDER", "stone_set_depth_mode: invalid UTF-8 comparison; depth state unchanged");
            return;
        };
        compare_str
    };
    let func = match compare_str {
        "never" => glow::NEVER,
        "less" => glow::LESS,
        "equal" => glow::EQUAL,
        "lequal" | "lessequal" => glow::LEQUAL,
        "greater" => glow::GREATER,
        "notequal" => glow::NOTEQUAL,
        "gequal" | "greaterequal" => glow::GEQUAL,
        "always" => glow::ALWAYS,
        _ => {
            ::log::error!(target: "RENDER", "stone_set_depth_mode: unknown comparison {:?}; depth state unchanged", compare_str);
            return;
        }
    };

    with_gl(|gl| unsafe {
        if func == glow::ALWAYS && !write {
            gl.disable(glow::DEPTH_TEST);
        } else {
            gl.enable(glow::DEPTH_TEST);

            gl.depth_func(func);
        }

        gl.depth_mask(write);
    });
}

/// glClear(DEPTH_BUFFER_BIT) is gated by DEPTH_WRITEMASK, which draw-list flushes leave
/// false, so a depth clear must enable writes first.
pub(crate) unsafe fn clear_depth_unmasked(gl: &DebugGl, depth: f32) {
    gl.depth_mask(true);
    gl.clear_depth_f32(depth);
    gl.clear(glow::DEPTH_BUFFER_BIT);
}

/// Read GL's depth write mask; a depth clear through a closed mask is a no-op.
#[no_mangle]
pub extern "C" fn stone_gl_depth_write_enabled() -> bool {
    with_gl(|gl| unsafe { gl.get_parameter_bool(glow::DEPTH_WRITEMASK) }).unwrap_or(false)
}

#[no_mangle]
pub extern "C" fn stone_clear_depth(depth: c_float) {
    with_gl(|gl| unsafe {
        clear_depth_unmasked(gl, depth);
    });
}

#[no_mangle]
pub extern "C" fn stone_set_color_mask(r: bool, g: bool, b: bool, a: bool) {
    with_gl(|gl| unsafe {
        gl.color_mask(r, g, b, a);
    });
}

#[no_mangle]
pub extern "C" fn stone_set_clear_color(r: c_float, g: c_float, b: c_float, a: c_float) {
    with_gl(|gl| unsafe {
        gl.clear_color(r, g, b, a);
    });
}

/// Set the scissor rectangle and test together; a stale rect confines later clears.
#[no_mangle]
pub extern "C" fn stone_set_scissor(enabled: bool, x: c_int, y: c_int, w: c_int, h: c_int) {
    with_gl(|gl| unsafe {
        gl.scissor(x, y, w, h);
        gl.set_enabled(glow::SCISSOR_TEST, enabled);
    });
}

/// Viewport coordinates are bottom-left target pixels, without scaling or Y flip.
#[no_mangle]
pub extern "C" fn stone_set_gl_viewport(x: c_int, y: c_int, w: c_int, h: c_int) {
    with_gl(|gl| unsafe {
        gl.viewport(x, y, w, h);
    });
}

/// Colour attachments a canvas FBO can address: the count GL 3.3 guarantees
/// for both GL_MAX_DRAW_BUFFERS and GL_MAX_COLOR_ATTACHMENTS.
pub const DRAW_BUFFERS: usize = 8;

#[no_mangle]
pub extern "C" fn stone_set_draw_buffer_mask(mask: c_int) {
    let bits = (mask as u32) & ((1 << DRAW_BUFFERS) - 1);
    let bits = if bits == 0 { 1 } else { bits };
    let len = 32 - bits.leading_zeros();
    let mut buffers = [glow::NONE; DRAW_BUFFERS];
    for (slot, buffer) in buffers.iter_mut().enumerate().take(len as usize) {
        if bits & (1 << slot) != 0 {
            *buffer = glow::COLOR_ATTACHMENT0 + slot as u32;
        }
    }
    with_gl(|gl| unsafe {
        gl.draw_buffers(&buffers[..len as usize]);
    });
}

#[no_mangle]
pub extern "C" fn stone_restore_after_custom_draw() {
    with_gl(|gl| unsafe {
        gl.disable(glow::CULL_FACE);
        gl.disable(glow::STENCIL_TEST);
        gl.disable(glow::POLYGON_OFFSET_FILL);
        gl.disable(glow::SAMPLE_ALPHA_TO_COVERAGE);
        gl.disable(glow::RASTERIZER_DISCARD);
        gl.color_mask(true, true, true, true);
    });
}
