// The global GL state machine: the state every draw path mutates and none owns.
// Split from `lib.rs`, which keeps context bring-up (SDL attributes, context
// creation, the glow loader, swap and swap interval) -- a second graphics
// backend needs both halves, and they sit on opposite sides of the seam.

use std::cell::RefCell;
use std::ffi::{c_char, c_float, c_int, CStr};

use crate::{log_once, with_gl, DebugGl, IMAGES};

/// Block until all issued GL commands complete (glFinish). Used right after
/// the swap to pull the vsync stall to a known point so the frame-timing
/// spans measure real CPU work, not the driver's vblank busy-wait.
#[no_mangle]
pub extern "C" fn stone_gl_finish() {
    with_gl(|gl| unsafe { gl.finish() });
}

/// Submit all issued GL commands without waiting for them to complete
/// (glFlush). Paired with `stone_gl_finish` this separates two costs that both
/// surface at the next frame's first GL call: driver-side command submission
/// (flush pays it, finish also pays it) and waiting for the GPU or the display
/// (only finish pays that). See `STONE_PRESENT_SYNC`.
#[no_mangle]
pub extern "C" fn stone_gl_flush() {
    with_gl(|gl| unsafe { gl.flush() });
}
// =============================================================================
// OpenGL / Canvas Functions
// =============================================================================

#[no_mangle]
pub extern "C" fn stone_clear(r: c_float, g: c_float, b: c_float, a: c_float) {
    with_gl(|gl| unsafe {
        gl.clear_color(r, g, b, a);
        gl.clear(glow::COLOR_BUFFER_BIT);
    });
}
/// 0 = NEAREST, 1 = LINEAR, 2 = stable bilinear mip sampling. `None` for anything else: a texture filter has no
/// sensible default to fall back to, and picking one silently is invisible
/// until the texture magnifies. Keep in sync with the IMAGE_FILTER_* constants
/// in stone/engine/lib.rock and with the web twin `__sw_gl_filter`.
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

/// Whether a sampler object is currently bound on texture `unit`. The engine's
/// contract is that no unit carries one between draws: the paths that bind a
/// texture without going through bind_sampled
/// (rect batches and immediate draws, on unit 0) would otherwise inherit
/// whatever the last sampled draw left behind. Pinned by the sampler_release
/// sim -- a leaked sampler is invisible to a rendering oracle until some
/// unrelated draw happens to sample that unit.
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

/// How many texture units `glActiveTexture` can select, i.e. every unit that
/// can carry sampler state. The denominator for a sampler-leak scan: a slot
/// count only covers the units a compiler `@textures` group owns, while
/// `Pass.inputs` is caller-unit-addressed by design, so a hand-authored-GLSL
/// pass may pick any legal unit. 0 when there is no GL context, which fails the
/// scan's denominator assertion rather than emptying its loop silently.
#[no_mangle]
pub extern "C" fn stone_max_texture_units() -> c_int {
    with_gl(|gl| unsafe { gl.get_parameter_i32(glow::MAX_COMBINED_TEXTURE_IMAGE_UNITS) })
        .unwrap_or(0)
}

/// Bind the sampler for `mode` (0 = nearest, 1 = linear, 2 = stable bilinear mip) on texture `unit`.
/// `image` selects the wrap half of the pair: a sampler overrides the texture's
/// own wrap, so binding one that disagrees would silently clamp a REPEAT
/// texture. Pass 0 for a canvas -- canvas textures are always CLAMP_TO_EDGE.
/// A negative `mode` releases that unit back to texture-object filtering, which
/// the paths that bind a texture without going through here still rely on.
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
// =============================================================================
// Blend/Depth Mode
// =============================================================================

// Blend mode as an int, mirroring the Rock BlendMode enum
// (blend_mode_to_int in commands.rock). An out-of-range int is
// unreachable from typed Rock; the fallback logs once and stays alpha
// rather than silently mis-rendering (the "lighten" bug class).
#[no_mangle]
pub extern "C" fn stone_set_blend_mode_i(mode: i32) {
    // Every arm resets the blend equation: "lighten" sets GL_MAX, and
    // without an explicit FUNC_ADD elsewhere the MAX equation would
    // leak into every draw after a lighten bracket.
    with_gl(|gl| unsafe {
        match mode {
            0 => {
                // Alpha
                gl.enable(glow::BLEND);
                gl.blend_equation(glow::FUNC_ADD);
                gl.blend_func_separate(
                    glow::SRC_ALPHA,
                    glow::ONE_MINUS_SRC_ALPHA,
                    glow::ONE,
                    glow::ONE_MINUS_SRC_ALPHA,
                );
            }
            1 => {
                // Additive
                gl.enable(glow::BLEND);
                gl.blend_equation(glow::FUNC_ADD);
                gl.blend_func_separate(
                    glow::SRC_ALPHA,
                    glow::ONE,
                    glow::ONE,
                    glow::ONE_MINUS_SRC_ALPHA,
                );
            }
            2 => {
                // Multiply
                gl.enable(glow::BLEND);
                gl.blend_equation(glow::FUNC_ADD);
                gl.blend_func_separate(glow::DST_COLOR, glow::ZERO, glow::DST_ALPHA, glow::ZERO);
            }
            3 => {
                // Lighten -- per-channel max; GL_MAX ignores the factors.
                gl.enable(glow::BLEND);
                gl.blend_equation(glow::MAX);
                gl.blend_func_separate(glow::ONE, glow::ONE, glow::ONE, glow::ONE);
            }
            4 => {
                // Replace
                gl.disable(glow::BLEND);
                gl.blend_equation(glow::FUNC_ADD);
            }
            5 => {
                // Accumulate -- alpha IGNORED, unlike Additive above.
                gl.enable(glow::BLEND);
                gl.blend_equation(glow::FUNC_ADD);
                gl.blend_func_separate(glow::ONE, glow::ONE, glow::ONE, glow::ONE);
            }
            6 => {
                // Premultiplied: the source RGB is already associated with coverage.
                gl.enable(glow::BLEND);
                gl.blend_equation(glow::FUNC_ADD);
                gl.blend_func_separate(
                    glow::ONE,
                    glow::ONE_MINUS_SRC_ALPHA,
                    glow::ONE,
                    glow::ONE_MINUS_SRC_ALPHA,
                );
            }
            other => {
                warn_unknown_blend_mode(other);
                gl.enable(glow::BLEND);
                gl.blend_equation(glow::FUNC_ADD);
                gl.blend_func_separate(
                    glow::SRC_ALPHA,
                    glow::ONE_MINUS_SRC_ALPHA,
                    glow::ONE,
                    glow::ONE_MINUS_SRC_ALPHA,
                );
            }
        }
    });
}

/// The GL blend state as one value: what `stone_set_blend_mode_i` writes, and
/// the whole of what a successor entry inherits.
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
        if self.enabled {
            gl.enable(glow::BLEND);
        } else {
            gl.disable(glow::BLEND);
        }
        gl.blend_equation_separate(self.equation_rgb, self.equation_alpha);
        gl.blend_func_separate(self.src_rgb, self.dst_rgb, self.src_alpha, self.dst_alpha);
    }
}

// Pass scopes are LIFO, including a pass submitted while another is executing.
// Reuse capacity: snapshots allocate neither per draw nor after the stack warms.
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

/// Restore the matching scope. A mismatch is a caller error, not permission to
/// consume another transaction's snapshot. The Rock owner surfaces refusal.
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
            if state.depth_enabled {
                gl.enable(glow::DEPTH_TEST);
            } else {
                gl.disable(glow::DEPTH_TEST);
            }
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
            if state.cull_enabled {
                gl.enable(glow::CULL_FACE);
            } else {
                gl.disable(glow::CULL_FACE);
            }
        })
        .is_some();
        if restored {
            states.pop();
        }
        restored
    })
}

// =============================================================================
// GL write state probe: colour mask, clear colour, scissor
// =============================================================================

/// GL's current write state as `mask=RGBA clear=r,g,b,a scissor=on/off x,y,w,h`,
/// with a masked-off channel shown as `-`. The fixture's oracle, and GL's own
/// answer rather than a mirror.
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
    match read {
        Some(s) => write_state_reply(s),
        None => write_state_reply("no-gl".to_string()),
    }
}

thread_local! {
    static WRITE_STATE_REPLY: RefCell<Option<std::ffi::CString>> = const { RefCell::new(None) };
}

fn write_state_reply(s: String) -> *const c_char {
    WRITE_STATE_REPLY.with(|b| {
        let Ok(c) = std::ffi::CString::new(s) else {
            return std::ptr::null();
        };
        let ptr = c.as_ptr();
        *b.borrow_mut() = Some(c);
        ptr
    })
}

// =============================================================================
// Sampler bindings on units >= 2
// =============================================================================

/// The first texture unit the sampler probe reports.
///
/// Units 0 and 1 are LAUNDERED every frame -- the present pass rewrites unit 0
/// and the tonemap rewrites unit 1 -- so a violation on either is erased before
/// any end-of-frame reading can attribute it. Units 2 and up have no such
/// writer, so they are where a leaked binding both survives and can be seen.
const SAMPLER_FIRST_TRACKED_UNIT: u32 = 2;

/// How many units the probe covers.
fn tracked_sampler_units(gl: &stone_gl::DebugGl) -> u32 {
    let max = unsafe { gl.get_parameter_i32(glow::MAX_COMBINED_TEXTURE_IMAGE_UNITS) };
    u32::try_from(max)
        .unwrap_or(0)
        .saturating_sub(SAMPLER_FIRST_TRACKED_UNIT)
}

/// The sampler GL has bound on `unit`, or -1 for a unit outside the range the
/// probe covers. 0 means no sampler, which is a different state from "not tracked".
#[no_mangle]
pub extern "C" fn stone_gl_sampler_on_unit(unit: c_int) -> c_int {
    let Ok(unit) = u32::try_from(unit) else {
        return -1;
    };
    if unit < SAMPLER_FIRST_TRACKED_UNIT {
        return -1;
    }
    with_gl(|gl| unsafe {
        if unit >= SAMPLER_FIRST_TRACKED_UNIT + tracked_sampler_units(gl) {
            return -1;
        }
        gl.active_texture(glow::TEXTURE0 + unit);
        let bound = gl.get_parameter_i32(glow::SAMPLER_BINDING);
        gl.active_texture(glow::TEXTURE0);
        bound
    })
    .unwrap_or(-1)
}

thread_local! {
    static BLEND_STATE_REPLY: RefCell<Option<std::ffi::CString>> = const { RefCell::new(None) };
}

/// The blend state as GL currently has it, `on/off eq=<e> src=<s> dst=<d>` with
/// raw GL enum numbers. A diagnostic reader beside `stone_gl_depth_write_enabled`.
///
/// GL's own answer, not the Rock-side `CURRENT_BLEND` shadow -- the two
/// disagreeing is the whole subject, so a reader that consulted the shadow could
/// not see the case it exists for.
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
    })
    .unwrap_or_else(|| "no-gl".to_string());
    BLEND_STATE_REPLY.with(|b| {
        let Ok(c) = std::ffi::CString::new(s) else {
            return std::ptr::null();
        };
        let ptr = c.as_ptr();
        *b.borrow_mut() = Some(c);
        ptr
    })
}

// Once per unknown mode int (can only fire on a Rock/Rust enum-int
// drift, not from typed callers). set_blend_mode runs per frame, so a
// silent fallback would hide the drift indefinitely.
fn warn_unknown_blend_mode(mode: i32) {
    // Through the one dedup store, keyed by the MODE: a site-only key would
    // report the first drifted int and swallow every other one.
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

/// THE depth clear. `glClear(DEPTH_BUFFER_BIT)` is gated by DEPTH_WRITEMASK,
/// and `unset_depth_mode()` (`stone_set_depth_mode("always", false)`) ends every
/// draw-list flush with it false -- so a depth clear issued with inherited state
/// silently does nothing. Enabling writes first is not this call's private
/// convention; it is what makes a depth clear a clear at all, which is why the
/// canvas reset goes through here rather than reimplementing the pair.
pub(crate) unsafe fn clear_depth_unmasked(gl: &DebugGl, depth: f32) {
    gl.depth_mask(true);
    gl.clear_depth_f32(depth);
    gl.clear(glow::DEPTH_BUFFER_BIT);
}

/// DEPTH_WRITEMASK as GL currently has it. A diagnostic reader: a clear through
/// a closed mask is a no-op, so code that opens it to do its own clearing has to
/// be able to prove it put it back.
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

/// Set GL's clear colour. The other two thirds of the `gl_write_state` ambient
/// row already have setters (`stone_set_color_mask`, and the scissor below);
/// this completes the trio so a fixture can move each independently and show
/// the reader responds to all three rather than to one of them.
#[no_mangle]
pub extern "C" fn stone_set_clear_color(r: c_float, g: c_float, b: c_float, a: c_float) {
    with_gl(|gl| unsafe {
        gl.clear_color(r, g, b, a);
    });
}

/// Set GL's scissor rect, and enable or disable the test.
///
/// One call for both halves because they cross together: a rect with the test
/// off changes nothing a successor can see, and the test on with a stale rect
/// is the case that silently confines every later clear.
#[no_mangle]
pub extern "C" fn stone_set_scissor(enabled: bool, x: c_int, y: c_int, w: c_int, h: c_int) {
    with_gl(|gl| unsafe {
        gl.scissor(x, y, w, h);
        if enabled {
            gl.enable(glow::SCISSOR_TEST);
        } else {
            gl.disable(glow::SCISSOR_TEST);
        }
    });
}

/// Set the viewport in the bound target's own pixels, bottom-left origin --
/// GL's convention, with no logical/backing scaling and no Y flip.
///
/// Distinct from `stone_set_viewport_rect`, which takes a top-left LOGICAL rect
/// and scales it by the drawable's density: this one is the semantic
/// submission's `GpuRect`, which is already stated in bottom-left drawable
/// pixels (the same convention `scene_output_viewport` is in), so converting it
/// would be undoing the caller's own arithmetic.
#[no_mangle]
pub extern "C" fn stone_set_gl_viewport(x: c_int, y: c_int, w: c_int, h: c_int) {
    with_gl(|gl| unsafe {
        gl.viewport(x, y, w, h);
    });
}

/// Set the CURRENTLY-BOUND FBO's draw buffers from an attachment BITMASK: bit
/// `i` enables COLOR_ATTACHMENT`i`, a clear bit below the highest set one
/// writes GL_NONE at that slot. draw-buffer state is per-FBO. R6 uses this to
/// confine MRT to the color pass: `0b11` ([ATTACHMENT0, ATTACHMENT1]) during
/// the pass so both color and id are written+cleared, `0b1` afterward so any
/// later render into the same canvas (e.g. a scene/composite pass) writes only
/// color and can't clobber the id attachment.
///
/// The gaps are what a prefix count could not express. A world-damage mark
/// blends re-authored albedo into the scene color and wet roughness into the
/// material channels while leaving the picking ID, the surface normal and the
/// surface owner exactly as the floor/wall producer wrote them: `0b00101`.
/// A fragment output whose slot is GL_NONE is discarded, so the shader still
/// declares every output up to the highest one it writes.
///
/// Attachments above the fifth are not addressable; bits outside [0, 5) are
/// ignored, and an empty mask falls back to `0b1`.
#[no_mangle]
pub extern "C" fn stone_set_draw_buffer_mask(mask: c_int) {
    const ATTACHMENTS: [u32; 5] = [
        glow::COLOR_ATTACHMENT0,
        glow::COLOR_ATTACHMENT1,
        glow::COLOR_ATTACHMENT2,
        glow::COLOR_ATTACHMENT3,
        glow::COLOR_ATTACHMENT4,
    ];
    let bits = (mask as u32) & 0b11111;
    let bits = if bits == 0 { 1 } else { bits };
    let len = 32 - bits.leading_zeros();
    let mut buffers = [glow::NONE; 5];
    for slot in 0..len as usize {
        if bits & (1 << slot) != 0 {
            buffers[slot] = ATTACHMENTS[slot];
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
