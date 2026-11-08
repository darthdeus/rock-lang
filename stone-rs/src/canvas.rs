use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{c_char, c_float, c_int, CStr};

use crate::handles::Class;
use crate::{track_canvas_switch, with_gl, WINDOW};

struct Canvas {
    fbo: glow::Framebuffer,
    texture: glow::Texture,
    // A TEXTURE, not a renderbuffer: the depth-visualize overlay samples the
    // world pass's own depth, and a renderbuffer cannot be sampled at all.
    // Depth readback (glReadPixels DEPTH_COMPONENT) works against either.
    depth_texture: Option<glow::Texture>,
    // Allocated capacity. A scene target is sized for its raster profile's
    // bounded worst case and never resized by camera zoom.
    width: i32,
    height: i32,
    // The live rectangle inside that capacity, anchored at the GL origin.
    // Binding viewports and scissors to it, so every draw, clear and
    // projection sees the active image rather than the backing texture.
    // Equal to (width, height) unless a raster profile says otherwise.
    active_w: i32,
    active_h: i32,
    // The canvas format code (see `resolve_texture_format`), kept so a resize
    // can re-specify the texture the way creation did.
    format: c_int,
    // Whether anything has BOUND this canvas as a render target since it was
    // created. `tex_image_2d(.., None)` leaves contents unspecified, so
    // creation clears explicitly and this bit is what says the clear is still
    // the whole truth.
    //
    // A conservative over-approximation on purpose: a bind is not proof of a
    // draw, but the absence of one IS proof of no draw, and the consumer
    // (`sim_native.rock`) fails closed on the bit being set. Erring toward
    // "painted" costs a worker; erring the other way would silently zero a
    // canvas whose boot projection was not blank.
    bound_as_target: bool,
}

thread_local! {
    static CANVASES: RefCell<HashMap<u64, Canvas>> = RefCell::new(HashMap::new());
    static CURRENT_FBO: RefCell<Option<glow::Framebuffer>> = const { RefCell::new(None) };
    // Pixel dimensions of the currently-bound render target, so the sprite
    // projection matches the active framebuffer (window-size or an internal-res
    // canvas) instead of always assuming the window. (0,0) = unset -> fall back
    // to the window in `current_target_size`.
    static CURRENT_TARGET_SIZE: RefCell<(f32, f32)> = const { RefCell::new((0.0, 0.0)) };
    // Integer-scale letterbox present: while set, binding the default
    // framebuffer confines the viewport to this rect instead of the full
    // drawable, so the fullscreen composite quads land the centered
    // sub-rect and the remainder stays the cleared black bars. Stored as
    // (x, y, w, h) in LOGICAL window px with a TOP-LEFT origin (the
    // coordinate system all Rock-side math uses); converted to GL
    // bottom-origin drawable px at bind time.
    static PRESENT_VIEWPORT: RefCell<Option<(i32, i32, i32, i32)>> = const { RefCell::new(None) };
    // The framebuffer that presents to the window: 0 (None) on a visible
    // window, the owned `HeadlessScreen` after a headless boot.
    static DEFAULT_FBO: RefCell<Option<glow::Framebuffer>> = const { RefCell::new(None) };
}

pub(crate) fn set_default_framebuffer(fbo: Option<glow::Framebuffer>) {
    // A new context must not inherit the previous context's owned screen.
    HEADLESS_SCREEN.with(|h| *h.borrow_mut() = None);
    DEFAULT_FBO.with(|d| *d.borrow_mut() = fbo);
}

/// Map a CURRENT_FBO-style value (None == screen) to the framebuffer to
/// actually bind for it.
pub(crate) fn resolve_target(fbo: Option<glow::Framebuffer>) -> Option<glow::Framebuffer> {
    fbo.or_else(|| DEFAULT_FBO.with(|d| *d.borrow()))
}

/// The screen of a headless window: a framebuffer this crate owns, sized to the
/// drawable. A headless window is never mapped, and a never-mapped X11 window's
/// default framebuffer keeps its CREATION size through a resize (NVIDIA,
/// measured): a sim worker that grew its window for an entry drew and read back
/// nothing past boot's size. Colour and depth match the window's own default
/// framebuffer (RGB8, no alpha, depth 24).
struct HeadlessScreen {
    present_fbo: Option<glow::Framebuffer>,
    color: glow::Renderbuffer,
    depth: glow::Renderbuffer,
    size: (i32, i32),
}

thread_local! {
    static HEADLESS_SCREEN: RefCell<Option<HeadlessScreen>> = const { RefCell::new(None) };
}

/// Create the headless screen at the drawable's size and make it the target
/// every bind to the screen resolves to. Leaves it bound.
pub(crate) fn create_headless_screen(
    gl: &crate::DebugGl,
    width: i32,
    height: i32,
) -> Result<(), String> {
    let fbo = gl.create_framebuffer()?;
    let color = gl.create_renderbuffer()?;
    let depth = gl.create_renderbuffer()?;
    unsafe {
        // A renderbuffer name is not an object until first bound, and
        // attaching one that is not is an INVALID_OPERATION.
        gl.bind_renderbuffer(glow::RENDERBUFFER, Some(color));
        gl.bind_renderbuffer(glow::RENDERBUFFER, Some(depth));
        gl.bind_renderbuffer(glow::RENDERBUFFER, None);
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
        gl.framebuffer_renderbuffer(
            glow::FRAMEBUFFER,
            glow::COLOR_ATTACHMENT0,
            glow::RENDERBUFFER,
            Some(color),
        );
        gl.framebuffer_renderbuffer(
            glow::FRAMEBUFFER,
            glow::DEPTH_ATTACHMENT,
            glow::RENDERBUFFER,
            Some(depth),
        );
    }
    let present_fbo = DEFAULT_FBO.with(|d| *d.borrow());
    set_default_framebuffer(Some(fbo));
    HEADLESS_SCREEN.with(|h| {
        *h.borrow_mut() = Some(HeadlessScreen {
            present_fbo,
            color,
            depth,
            size: (0, 0),
        })
    });
    headless_screen_follow(gl, width, height)
}

/// Copy the owned screen to the native drawable without changing what screen
/// binds and captures read. The caller admits only visible windows.
pub(crate) fn present_headless_screen(window: &sdl2::video::Window) {
    HEADLESS_SCREEN.with(|h| {
        let slot = h.borrow();
        let Some(screen) = slot.as_ref() else { return };
        let (width, height) = window.drawable_size();
        with_gl(|gl| unsafe {
            let read = gl.get_parameter_i32(glow::READ_FRAMEBUFFER_BINDING);
            let draw = gl.get_parameter_i32(glow::DRAW_FRAMEBUFFER_BINDING);
            let scissor = gl.is_enabled(glow::SCISSOR_TEST);
            gl.disable(glow::SCISSOR_TEST);
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, resolve_target(None));
            gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, screen.present_fbo);
            gl.blit_framebuffer(
                0,
                0,
                screen.size.0,
                screen.size.1,
                0,
                0,
                width as i32,
                height as i32,
                glow::COLOR_BUFFER_BIT,
                glow::NEAREST,
            );
            gl.bind_framebuffer(
                glow::READ_FRAMEBUFFER,
                std::num::NonZeroU32::new(read as u32).map(glow::NativeFramebuffer),
            );
            gl.bind_framebuffer(
                glow::DRAW_FRAMEBUFFER,
                std::num::NonZeroU32::new(draw as u32).map(glow::NativeFramebuffer),
            );
            if scissor {
                gl.enable(glow::SCISSOR_TEST);
            }
        });
    });
}

/// Re-specify the headless screen's storage at `width` x `height` when that is
/// not its size, and establish blank contents as a fresh window's would be. The
/// screen framebuffer must be bound. No-op without a headless screen.
fn headless_screen_follow(gl: &crate::DebugGl, width: i32, height: i32) -> Result<(), String> {
    HEADLESS_SCREEN.with(|h| {
        let mut slot = h.borrow_mut();
        let Some(screen) = slot.as_mut() else {
            return Ok(());
        };
        if screen.size == (width, height) {
            return Ok(());
        }
        unsafe {
            gl.bind_renderbuffer(glow::RENDERBUFFER, Some(screen.color));
            gl.renderbuffer_storage(glow::RENDERBUFFER, glow::RGB8, width.max(1), height.max(1));
            gl.bind_renderbuffer(glow::RENDERBUFFER, Some(screen.depth));
            gl.renderbuffer_storage(
                glow::RENDERBUFFER,
                glow::DEPTH_COMPONENT24,
                width.max(1),
                height.max(1),
            );
            gl.bind_renderbuffer(glow::RENDERBUFFER, None);
            let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
            if status != glow::FRAMEBUFFER_COMPLETE {
                return Err(format!(
                    "headless screen {width}x{height} is incomplete: 0x{status:X}"
                ));
            }
            establish_blank_contents(gl, false, true);
        }
        screen.size = (width, height);
        Ok(())
    })
}

// Dimensions the sprite ortho projection should use: the bound canvas, or the
// window when rendering to the default framebuffer / before any bind.
pub(crate) fn current_target_size() -> (f32, f32) {
    let s = CURRENT_TARGET_SIZE.with(|c| *c.borrow());
    if s.0 > 0.0 && s.1 > 0.0 {
        return s;
    }
    crate::with_window(|win| {
        let (w, h) = win.window.size();
        (w as f32, h as f32)
    })
    .unwrap_or((800.0, 600.0))
}

// Rock builds the camera matrices and owns EngineU's bytes, but the ortho is
// derived from whatever target is bound AT THE MOMENT OF USE, and that is
// tracked here (the canvas bind updates it). Two scalar getters rather than a
// Vec2 return: an FFI struct-by-value return can abort a trace.
#[no_mangle]
pub extern "C" fn stone_current_target_width() -> c_float {
    current_target_size().0
}

#[no_mangle]
pub extern "C" fn stone_current_target_height() -> c_float {
    current_target_size().1
}

#[no_mangle]
pub extern "C" fn stone_clear_canvas(r: c_float, g: c_float, b: c_float, a: c_float) {
    with_gl(|gl| unsafe {
        gl.clear_color(r, g, b, a);
        gl.clear(glow::COLOR_BUFFER_BIT);
    });
}

/// Set (or clear) the letterbox present viewport applied whenever the
/// default framebuffer is bound. Rect is (x, y, w, h) in logical window px,
/// top-left origin. The caller re-binds canvas 0 to apply a change.
#[no_mangle]
pub extern "C" fn stone_set_present_viewport(active: bool, x: c_int, y: c_int, w: c_int, h: c_int) {
    PRESENT_VIEWPORT.with(|p| {
        *p.borrow_mut() = if active { Some((x, y, w, h)) } else { None };
    });
}

thread_local! {
    static PRESENT_VIEWPORT_REPLY: RefCell<Option<std::ffi::CString>> = const { RefCell::new(None) };
    static GL_VIEWPORT_REPLY: RefCell<Option<std::ffi::CString>> = const { RefCell::new(None) };
}

/// The letterbox present rect as `x,y,w,h`, or `none` when there is none. A
/// diagnostic reader; the rect has no other way out of this crate.
#[no_mangle]
pub extern "C" fn stone_present_viewport() -> *const c_char {
    let s = match PRESENT_VIEWPORT.with(|p| *p.borrow()) {
        Some((x, y, w, h)) => format!("{x},{y},{w},{h}"),
        None => "none".to_string(),
    };
    PRESENT_VIEWPORT_REPLY.with(|b| {
        let Ok(c) = std::ffi::CString::new(s) else {
            return std::ptr::null();
        };
        let ptr = c.as_ptr();
        *b.borrow_mut() = Some(c);
        ptr
    })
}

/// GL's own viewport as `x,y,w,h`. The CONSEQUENCE of the rect above: binding
/// canvas 0 applies one or the other, so this is what a successor's draws
/// actually land in.
#[no_mangle]
pub extern "C" fn stone_gl_viewport() -> *const c_char {
    let s = with_gl(|gl| unsafe {
        let mut v = [0i32; 4];
        gl.get_parameter_i32_slice(glow::VIEWPORT, &mut v);
        format!("{},{},{},{}", v[0], v[1], v[2], v[3])
    })
    .unwrap_or_else(|| "no-gl".to_string());
    GL_VIEWPORT_REPLY.with(|b| {
        let Ok(c) = std::ffi::CString::new(s) else {
            return std::ptr::null();
        };
        let ptr = c.as_ptr();
        *b.borrow_mut() = Some(c);
        ptr
    })
}

/// Set a canvas's active rectangle: the live image inside its allocated
/// capacity, anchored at the GL origin. Clamped into [1, capacity]. Takes
/// effect at the next bind of that canvas.
#[no_mangle]
pub extern "C" fn stone_set_canvas_active(handle: u64, w: c_int, h: c_int) {
    CANVASES.with(|canvases| {
        if let Some(canvas) = canvases.borrow_mut().get_mut(&handle) {
            canvas.active_w = w.clamp(1, canvas.width);
            canvas.active_h = h.clamp(1, canvas.height);
        }
    });
}

/// Restrict rendering to a sub-rect of the CURRENTLY BOUND target, in that
/// target's logical top-left-origin pixels. A fullscreen-quad pass drawn under
/// one lands in the rect instead of over everything, which is what a corner
/// preview is -- no sprite-convention twin of the pass shader needed.
///
/// There is deliberately NO reset call: `stone_set_canvas` already restores the
/// bound target's full viewport, so re-binding the target is the one reset path
/// and there is no second one to keep in sync with it.
#[no_mangle]
pub extern "C" fn stone_set_viewport_rect(x: c_int, y: c_int, w: c_int, h: c_int) {
    if w <= 0 || h <= 0 {
        crate::log_once!("CANVAS", "stone_set_viewport_rect: empty rect {}x{}", w, h);
        return;
    }
    let (lw, lh) = current_target_size();
    // The default framebuffer is the only target whose logical size and pixel
    // size can differ (hidpi); a canvas is its own pixels.
    let (sx, sy, ph) = if CURRENT_FBO.with(|c| c.borrow().is_none()) {
        crate::with_window(|win| {
            let (dw, dh) = win.window.drawable_size();
            (dw as f32 / lw.max(1.0), dh as f32 / lh.max(1.0), dh as f32)
        })
        .unwrap_or((1.0, 1.0, lh))
    } else {
        (1.0, 1.0, lh)
    };

    with_gl(|gl| unsafe {
        let gw = (w as f32 * sx).round() as i32;
        let gh = (h as f32 * sy).round() as i32;
        let gx = (x as f32 * sx).round() as i32;
        let gy = ph as i32 - (y as f32 * sy).round() as i32 - gh;
        gl.viewport(gx, gy, gw, gh);
    });
}

#[no_mangle]
pub extern "C" fn stone_set_canvas(handle: u64) {
    if handle == 0 {
        // Bind to default framebuffer (screen)
        with_gl(|gl| unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, resolve_target(None));
            gl.disable(glow::SCISSOR_TEST);
        });
        CURRENT_FBO.with(|c| *c.borrow_mut() = None);

        // Restore viewport: the full drawable, or the letterbox present
        // rect while one is active.
        WINDOW.with(|w| {
            if let Some(window) = w.borrow().as_ref() {
                let (dw, dh) = window.window.drawable_size();
                let (lw, lh) = window.window.size();
                let pv = PRESENT_VIEWPORT.with(|p| *p.borrow());
                with_gl(|gl| unsafe {
                    // Every frame binds the screen before drawing to it, so this
                    // is where a resize since the last frame reaches it.
                    if let Err(e) = headless_screen_follow(gl, dw as i32, dh as i32) {
                        crate::log_once!("CANVAS", "{e}");
                    }
                    if let Some((x, y, w, h)) = pv {
                        // Logical window top-left -> GL bottom-origin
                        // drawable px (hidpi scale between the two).
                        let sx = dw as f32 / lw as f32;
                        let sy = dh as f32 / lh as f32;
                        let gw = (w as f32 * sx).round() as i32;
                        let gh = (h as f32 * sy).round() as i32;
                        let gx = (x as f32 * sx).round() as i32;
                        let gy = dh as i32 - (y as f32 * sy).round() as i32 - gh;
                        gl.viewport(gx, gy, gw, gh);
                    } else {
                        gl.viewport(0, 0, dw as i32, dh as i32);
                    }
                });
                CURRENT_TARGET_SIZE.with(|c| *c.borrow_mut() = (lw as f32, lh as f32));
            }
        });
        track_canvas_switch();
        return;
    }

    CANVASES.with(|canvases| {
        if let Some(canvas) = canvases.borrow_mut().get_mut(&handle) {
            // Binding as the draw target is the last point before any draw,
            // clear or blit can touch this canvas, so it is where "no longer
            // provably blank" is recorded (`bound_as_target`).
            canvas.bound_as_target = true;
            with_gl(|gl| unsafe {
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(canvas.fbo));
                gl.viewport(0, 0, canvas.active_w, canvas.active_h);
                // glClear ignores the viewport but respects the scissor, so
                // the scissor is what keeps a clear (and any overdraw) off the
                // inactive margin.
                if canvas.active_w < canvas.width || canvas.active_h < canvas.height {
                    gl.enable(glow::SCISSOR_TEST);
                    gl.scissor(0, 0, canvas.active_w, canvas.active_h);
                } else {
                    gl.disable(glow::SCISSOR_TEST);
                }
            });
            CURRENT_FBO.with(|c| *c.borrow_mut() = Some(canvas.fbo));
            CURRENT_TARGET_SIZE
                .with(|c| *c.borrow_mut() = (canvas.active_w as f32, canvas.active_h as f32));
        }
    });

    track_canvas_switch();
}

/// How one canvas format code resolves on THIS context.
///
/// One function because creation and resize must agree exactly: a resize that
/// re-specified the texture with a different internal format would leave the
/// FBO, the readback path and the shaders reading a surface none of them expect,
/// and nothing in the handle sets would say so.
struct TextureFormat {
    internal_format: u32,
    tex_format: u32,
    data_type: u32,
    int_format: bool,
}

// STONE_CANVAS_RGBA8 = 0, STONE_CANVAS_RGBA16F = 1, STONE_CANVAS_RGBA32F = 2,
// STONE_CANVAS_R32I = 3 (integer entity-id target).
fn resolve_texture_format(format: c_int) -> TextureFormat {
    let (internal_format, tex_format, data_type) = match format {
        1 => (glow::RGBA16F, glow::RGBA, glow::HALF_FLOAT),
        2 => (glow::RGBA32F, glow::RGBA, glow::FLOAT),
        3 => (glow::R32I, glow::RED_INTEGER, glow::INT),
        _ => (glow::RGBA8, glow::RGBA, glow::UNSIGNED_BYTE),
    };
    TextureFormat {
        internal_format,
        tex_format,
        data_type,
        int_format: is_int_format(format),
    }
}

fn is_float_format(format: c_int) -> bool {
    matches!(format, 1 | 2)
}

// R32I id target: readbacks must use RGBA_INTEGER/INT, and clears must go
// through glClearBufferiv (glClear on integer buffers is undefined).
fn is_int_format(format: c_int) -> bool {
    format == 3
}

/// Write creation's contents into the CURRENTLY BOUND framebuffer.
///
/// `tex_image_2d(.., None)` allocates without specifying contents, so "a new
/// canvas reads zero" was a property of this driver rather than of the engine.
/// Clearing is what makes creation's state defined, on every driver.
///
/// Shared by creation and by a resize, which re-specifies the same texture and
/// therefore leaves contents unspecified in exactly the same way. One function
/// so the state a resized canvas starts from is the state a fresh one does.
///
/// Every state the clear needs is SAVED AND RESTORED, because either caller can
/// run MID-RENDER -- a lazily allocated target, a pyramid rebuilt on a resize --
/// and leaving the masks open or the scissor off would silently reconfigure
/// whatever was drawing.
unsafe fn establish_blank_contents(gl: &crate::DebugGl, int_format: bool, has_depth: bool) {
    let saved_scissor = gl.is_enabled(glow::SCISSOR_TEST);
    let saved_color_mask = gl.get_parameter_bool_array::<4>(glow::COLOR_WRITEMASK);
    let saved_depth_mask = gl.get_parameter_bool(glow::DEPTH_WRITEMASK);
    let mut saved_clear_color = [0.0f32; 4];
    gl.get_parameter_f32_slice(glow::COLOR_CLEAR_VALUE, &mut saved_clear_color);
    let mut saved_clear_depth = [0.0f32; 1];
    gl.get_parameter_f32_slice(glow::DEPTH_CLEAR_VALUE, &mut saved_clear_depth);

    gl.disable(glow::SCISSOR_TEST);
    gl.color_mask(true, true, true, true);
    if int_format {
        gl.clear_buffer_i32_slice(glow::COLOR, 0, &[0, 0, 0, 0]);
    } else {
        gl.clear_color(0.0, 0.0, 0.0, 0.0);
        gl.clear(glow::COLOR_BUFFER_BIT);
    }
    if has_depth {
        crate::gl_state::clear_depth_unmasked(gl, 1.0);
    }

    if saved_scissor {
        gl.enable(glow::SCISSOR_TEST);
    }
    gl.color_mask(
        saved_color_mask[0],
        saved_color_mask[1],
        saved_color_mask[2],
        saved_color_mask[3],
    );
    gl.depth_mask(saved_depth_mask);
    gl.clear_color(
        saved_clear_color[0],
        saved_clear_color[1],
        saved_clear_color[2],
        saved_clear_color[3],
    );
    gl.clear_depth_f32(saved_clear_depth[0]);
}

#[no_mangle]
pub extern "C" fn stone_create_canvas(
    width: c_int,
    height: c_int,
    format: c_int,
    with_depth: bool,
) -> u64 {
    with_gl(|gl| unsafe {
        // Create FBO
        let fbo = match gl.create_framebuffer() {
            Ok(fbo) => fbo,
            Err(e) => {
                crate::log_once!("CANVAS", "Failed to create framebuffer ({}x{}): {}", width, height, e);
                return 0;
            }
        };
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));

        // Create texture
        let texture = match gl.create_texture() {
            Ok(tex) => tex,
            Err(e) => {
                crate::log_once!("CANVAS", "Failed to create canvas texture ({}x{}): {}", width, height, e);
                gl.delete_framebuffer(fbo);
                return 0;
            }
        };
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));

        let TextureFormat {
            internal_format,
            tex_format,
            data_type,
            int_format,
        } = resolve_texture_format(format);

        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            internal_format as i32,
            width,
            height,
            0,
            tex_format,
            data_type,
            glow::PixelUnpackData::Slice(None::<&[u8]>),
        );

        // The texture object's own filter is never observed by a sampled bind:
        // every reader of a canvas declares its sampling on the Pass input.
        // Integer textures MUST be NEAREST anyway -- a linear-filtered integer
        // texture is incomplete and samples zero.
        let filter = glow::NEAREST as i32;
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, filter);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, filter);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);

        // Attach texture to FBO
        gl.framebuffer_texture_2d(
            glow::FRAMEBUFFER,
            glow::COLOR_ATTACHMENT0,
            glow::TEXTURE_2D,
            Some(texture),
            0,
        );

        // Create the depth attachment as a sampleable texture if requested.
        let depth_texture = if with_depth {
            let dt = match gl.create_texture() {
                Ok(t) => Some(t),
                Err(e) => {
                    crate::log_once!("CANVAS", "Failed to create depth texture ({}x{}): {} (depth testing disabled for this canvas)", width, height, e);
                    None
                }
            };
            if let Some(dt) = dt {
                gl.bind_texture(glow::TEXTURE_2D, Some(dt));
                gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    glow::DEPTH_COMPONENT24 as i32,
                    width,
                    height,
                    0,
                    glow::DEPTH_COMPONENT,
                    glow::UNSIGNED_INT,
                    glow::PixelUnpackData::Slice(None),
                );
                // A depth texture is not filterable and has no mips, so the
                // default MIPMAP min filter would make it INCOMPLETE and every
                // sample would read black -- indistinguishable from "near".
                gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::NEAREST as i32);
                gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::NEAREST as i32);
                gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
                gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
                // Sample the stored value, not a shadow comparison -- NONE is
                // the default, stated because the two read completely
                // differently and nothing else here would say which we want.
                gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_COMPARE_MODE, glow::NONE as i32);
                gl.bind_texture(glow::TEXTURE_2D, None);
                gl.framebuffer_texture_2d(
                    glow::FRAMEBUFFER,
                    glow::DEPTH_ATTACHMENT,
                    glow::TEXTURE_2D,
                    Some(dt),
                    0,
                );
            }
            dt
        } else {
            None
        };

        // Check FBO completeness
        let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
        if status != glow::FRAMEBUFFER_COMPLETE {
            crate::log_once!("CANVAS", "Framebuffer not complete: 0x{:X} ({}x{})", status, width, height);
            gl.delete_texture(texture);
            if let Some(dt) = depth_texture {
                gl.delete_texture(dt);
            }
            gl.delete_framebuffer(fbo);
            gl.bind_framebuffer(glow::FRAMEBUFFER, resolve_target(None));
            return 0;
        }

        // ESTABLISH blank contents; see `establish_blank_contents`. The FBO is
        // still bound from the completeness check above.
        establish_blank_contents(gl, int_format, depth_texture.is_some());

        // Restore previous FBO
        let current = CURRENT_FBO.with(|c| *c.borrow());
        gl.bind_framebuffer(glow::FRAMEBUFFER, resolve_target(current));

        // Store canvas
        let id = crate::handles::mint(Class::Canvas);
        CANVASES.with(|canvases| {
            canvases.borrow_mut().insert(
                id,
                Canvas {
                    fbo,
                    texture,
                    depth_texture,
                    width,
                    height,
                    active_w: width,
                    active_h: height,
                    format,
                    bound_as_target: false,
                },
            );
        });

        id
    })
    .unwrap_or(0)
}

/// Clear one integer draw buffer of the CURRENTLY BOUND framebuffer to
/// `value` via glClearBufferiv -- glClear on an integer color buffer is
/// undefined per spec, so R32I id targets clear through this instead.
/// `draw_buffer` is the draw-buffer index (0 for a bound id canvas's own
/// FBO, 1 for the id attachment during the MRT color pass).
#[no_mangle]
pub extern "C" fn stone_clear_canvas_int(draw_buffer: c_int, value: c_int) {
    with_gl(|gl| unsafe {
        gl.clear_buffer_i32_slice(glow::COLOR, draw_buffer as u32, &[value, 0, 0, 0]);
    });
}

/// Whether this canvas has been bound as a render target since creation, i.e.
/// whether its contents are still provably the blank ones creation established.
/// False for a handle the registry does not know -- callers pair this with a
/// liveness answer rather than reading it as "safe".
#[no_mangle]
pub extern "C" fn stone_canvas_bound_as_target(handle: u64) -> bool {
    CANVASES.with(|c| c.borrow().get(&handle).is_some_and(|c| c.bound_as_target))
}

/// Whether this canvas's ACTIVE RECT is the one creation established.
///
/// State the pixel half cannot see: `stone_set_canvas_active` narrows the
/// sub-rect every later bind draws and reads into, and it sets no
/// `bound_as_target` flag, so a narrowed canvas answers "never painted" while
/// its geometry has moved. Creation's value is the canvas's full size, which is a known state --
/// so unlike the pixels this needs no snapshot.
#[no_mangle]
pub extern "C" fn stone_canvas_active_is_creation(handle: u64) -> bool {
    CANVASES.with(|c| {
        c.borrow()
            .get(&handle)
            .is_some_and(|c| c.active_w == c.width && c.active_h == c.height)
    })
}

/// The canvas's active width, or -1 for an unknown handle. The fixture's
/// oracle; the engine reads the field directly.
#[no_mangle]
pub extern "C" fn stone_canvas_active_w(handle: u64) -> c_int {
    CANVASES.with(|c| c.borrow().get(&handle).map_or(-1, |c| c.active_w))
}

/// The canvas's active height, or -1 for an unknown handle. Both axes are
/// published because `stone_set_canvas_active` moves them independently, so a
/// restore that put one back would be invisible to a reader of the other.
#[no_mangle]
pub extern "C" fn stone_canvas_active_h(handle: u64) -> c_int {
    CANVASES.with(|c| c.borrow().get(&handle).map_or(-1, |c| c.active_h))
}

/// The canvas's ALLOCATED width, or -1 for an unknown handle.
///
/// Distinct from the active width above, and needed for its own reason: a
/// scene-target reallocation now moves the STORAGE under a stable handle, so
/// the Rock-side `Canvas.width` and the texture behind it can disagree with
/// nothing in the handle sets to say so. This is the reader that can tell.
#[no_mangle]
pub extern "C" fn stone_canvas_capacity_w(handle: u64) -> c_int {
    CANVASES.with(|c| c.borrow().get(&handle).map_or(-1, |c| c.width))
}

/// The canvas's allocated height, or -1 for an unknown handle. Both axes, for
/// the reason both active axes are published: a restore that put one back would
/// be invisible to a reader of the other.
#[no_mangle]
pub extern "C" fn stone_canvas_capacity_h(handle: u64) -> c_int {
    CANVASES.with(|c| c.borrow().get(&handle).map_or(-1, |c| c.height))
}

/// Live canvas handles (`native_ledger`).
pub(crate) fn live_canvas_ids() -> Vec<u64> {
    CANVASES.with(|c| c.borrow().keys().copied().collect())
}

/// Asset-report census over the canvas registry: population and one handle
/// per index in id order (the `stone_get_live_image_handle` shape).
#[no_mangle]
pub extern "C" fn stone_get_live_canvas_count() -> c_int {
    get_canvas_count().min(c_int::MAX as usize) as c_int
}

#[no_mangle]
pub extern "C" fn stone_get_live_canvas_handle(index: c_int) -> u64 {
    if index < 0 {
        return 0;
    }
    CANVASES.with(|c| {
        let mut ids: Vec<u64> = c.borrow().keys().copied().collect();
        ids.sort_unstable();
        ids.get(index as usize).copied().unwrap_or(0)
    })
}

/// One canvas's storage: allocated width/height out, and the EFFECTIVE color
/// format code (`STONE_CANVAS_*`) as the return. Effective, not requested: a
/// float request this context downgraded reports RGBA8, which is what the
/// texture holds. -1 for an unknown handle, with the outs untouched.
#[no_mangle]
pub extern "C" fn stone_canvas_report(handle: u64, width: *mut c_int, height: *mut c_int) -> c_int {
    if handle == 0 || width.is_null() || height.is_null() {
        return -1;
    }
    // Directly, not through `lookup_canvas`: `CanvasInfo` does not carry the
    // requested format code, which is what distinguishes 16F from 32F.
    let found = CANVASES.with(|c| {
        c.borrow()
            .get(&handle)
            .map(|canvas| (canvas.width, canvas.height, canvas.format))
    });
    let Some((w, h, format)) = found else {
        return -1;
    };
    unsafe {
        *width = w;
        *height = h;
    }
    format
}

/// Return one canvas's CONTENTS to what creation leaves behind, keeping its
/// logical identity: same handle, same FBO, same textures, same size. This is
/// the sim worker's boundary operation for a canvas that SURVIVES an entry
/// (`stone/engine/sim_native.rock`).
///
/// The live-handle sets cannot see this case at all. A boot canvas the entry
/// merely PAINTED is neither an extra nor a missing -- it is present, at its
/// boot id, holding the predecessor's pixels, which is how a scene composited
/// over a non-blank target reached the next entry.
///
/// The clear is chosen by the canvas's own recorded format, not by one generic
/// `glClear`: `glClear` on an INTEGER color buffer is undefined per spec, so an
/// R32I id target goes through `glClearBufferiv` exactly as its own per-frame
/// reset does. Returns false for a handle the registry does not know, so a
/// caller cannot read "nothing to do" as success.
#[no_mangle]
pub extern "C" fn stone_canvas_reset_contents(handle: u64) -> bool {
    if handle == 0 {
        return false;
    }
    let Some(canvas) = lookup_canvas(handle) else {
        return false;
    };
    // The ACTIVE RECT is contents too, and creation's value is known -- the
    // canvas's full size -- so it resets rather than needing a snapshot. It is
    // restored here rather than in an ambient row because it belongs to THIS
    // handle: one process-wide row over N canvases could not say which canvas
    // moved. Its exposed window is narrow but real -- `set_scene_active_rects`
    // re-establishes it on the successor's first `frame_core_resolve_view` --
    // so what crosses is every sim statement before that frame.
    CANVASES.with(|canvases| {
        if let Some(c) = canvases.borrow_mut().get_mut(&handle) {
            c.active_w = c.width;
            c.active_h = c.height;
        }
    });
    with_gl(|gl| unsafe {
        let previous = CURRENT_FBO.with(|c| *c.borrow());
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(canvas.fbo));

        // A `glClear` is gated by the write state bound at the time, and the
        // boundary inherits the PREDECESSOR's: a masked-off channel, or a
        // scissor rect left from its last pass, turns this clear into a no-op
        // or a partial one while it still reports success. These three are set
        // to GL's own defaults, which is what the equivalent moment in a fresh
        // process has, so establishing them moves the worker toward the
        // fresh-process contract rather than away from it.
        gl.disable(glow::SCISSOR_TEST);
        gl.color_mask(true, true, true, true);

        if canvas.int_format {
            // Not maskable by glColorMask in the same way and undefined through
            // glClear, so an id target clears through its own path.
            gl.clear_buffer_i32_slice(glow::COLOR, 0, &[0, 0, 0, 0]);
        } else {
            gl.clear_color(0.0, 0.0, 0.0, 0.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
        }
        // A depth attachment is part of the same projection: stale depth
        // rejects the successor's fragments even over a blank colour target.
        // GL's post-creation value is 1.0 (the far plane).
        if canvas.has_depth {
            crate::gl_state::clear_depth_unmasked(gl, 1.0);
        }
        gl.bind_framebuffer(glow::FRAMEBUFFER, resolve_target(previous));
    })
    .is_some()
}

/// Re-specify one canvas's storage at `width` x `height`, keeping its handle,
/// its FBO, its texture objects and its format.
///
/// The scene target set is sized for a raster profile and a drawable, and both
/// move at runtime: a window resize, a render-resolution tier, a PixelPerfect
/// profile. Rebuilding the set for that destroyed nineteen canvases and minted
/// nineteen new handles -- `handles::mint` is monotonic, so a recreated canvas
/// is a DIFFERENT id, and every holder of the old one is left with a dead handle.
///
/// Re-specifying keeps the identity. `glTexImage2D` on an already-attached
/// texture replaces its storage in place; the framebuffer attachment names the
/// texture OBJECT, so it survives, as does every extra colour attachment bound
/// onto this FBO (`stone_canvas_attach_color_target`). Contents do not: the new
/// storage is unspecified until `establish_blank_contents` writes creation's.
///
/// Returns false, having logged, for an unknown handle, a non-positive extent,
/// or a re-specified FBO the driver reports incomplete -- and in that last case
/// the OLD storage is put back, so a refused resize leaves a canvas its caller
/// can still render through rather than a broken one.
///
/// `bound_as_target` is deliberately untouched. It answers "are this canvas's
/// contents still provably creation's", the boundary reads it only at BOOT
/// capture, and clearing it here would launder a paint that happened before the
/// resize.
#[no_mangle]
pub extern "C" fn stone_canvas_resize(handle: u64, width: c_int, height: c_int) -> bool {
    if handle == 0 || width < 1 || height < 1 {
        crate::log_once!(
            "CANVAS",
            "stone_canvas_resize: refused {}x{} for canvas {}",
            width,
            height,
            handle
        );
        return false;
    }
    let Some((fbo, texture, depth_texture, format, old_w, old_h)) = CANVASES.with(|canvases| {
        canvases.borrow().get(&handle).map(|c| {
            (
                c.fbo,
                c.texture,
                c.depth_texture,
                c.format,
                c.width,
                c.height,
            )
        })
    }) else {
        crate::log_once!(
            "CANVAS",
            "stone_canvas_resize: unknown canvas handle {}",
            handle
        );
        return false;
    };
    if old_w == width && old_h == height {
        return true;
    }

    let resized = with_gl(|gl| unsafe {
        let spec = resolve_texture_format(format);
        let previous = CURRENT_FBO.with(|c| *c.borrow());
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
        respecify_storage(gl, &spec, texture, depth_texture, width, height);

        let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
        let complete = status == glow::FRAMEBUFFER_COMPLETE;
        if complete {
            establish_blank_contents(gl, spec.int_format, depth_texture.is_some());
        } else {
            crate::log_once!(
                "CANVAS",
                "stone_canvas_resize: canvas {} is incomplete at {}x{} (0x{:X}); keeping {}x{}",
                handle,
                width,
                height,
                status,
                old_w,
                old_h
            );
            respecify_storage(gl, &spec, texture, depth_texture, old_w, old_h);
            establish_blank_contents(gl, spec.int_format, depth_texture.is_some());
        }
        gl.bind_framebuffer(glow::FRAMEBUFFER, resolve_target(previous));
        complete
    });
    let Some(complete) = resized else {
        crate::log_once!(
            "CANVAS",
            "stone_canvas_resize: no GL context for canvas {}",
            handle
        );
        return false;
    };
    if !complete {
        return false;
    }

    CANVASES.with(|canvases| {
        if let Some(c) = canvases.borrow_mut().get_mut(&handle) {
            c.width = width;
            c.height = height;
            // The active rectangle is expressed inside the capacity, so a new
            // capacity has no old rectangle to keep: leaving a narrowed one
            // would confine every later draw to a sub-rect of a surface that
            // just changed shape. Callers re-declare it (`set_scene_active_rects`)
            // exactly as they do after a create.
            c.active_w = width;
            c.active_h = height;
        }
    });
    true
}

/// Replace the storage behind one canvas's colour texture, and its depth
/// texture when it has one. The FBO must be bound by the caller; attachments
/// name the texture objects and so survive untouched.
unsafe fn respecify_storage(
    gl: &crate::DebugGl,
    spec: &TextureFormat,
    texture: glow::Texture,
    depth_texture: Option<glow::Texture>,
    width: c_int,
    height: c_int,
) {
    gl.bind_texture(glow::TEXTURE_2D, Some(texture));
    gl.tex_image_2d(
        glow::TEXTURE_2D,
        0,
        spec.internal_format as i32,
        width,
        height,
        0,
        spec.tex_format,
        spec.data_type,
        glow::PixelUnpackData::Slice(None::<&[u8]>),
    );
    if let Some(dt) = depth_texture {
        gl.bind_texture(glow::TEXTURE_2D, Some(dt));
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::DEPTH_COMPONENT24 as i32,
            width,
            height,
            0,
            glow::DEPTH_COMPONENT,
            glow::UNSIGNED_INT,
            glow::PixelUnpackData::Slice(None),
        );
    }
    gl.bind_texture(glow::TEXTURE_2D, None);
}

#[no_mangle]
pub extern "C" fn stone_destroy_canvas(handle: u64) {
    if handle == 0 {
        return;
    }

    CANVASES.with(|canvases| {
        if let Some(canvas) = canvases.borrow_mut().remove(&handle) {
            crate::handles::retire(Class::Canvas, handle);
            with_gl(|gl| unsafe {
                gl.delete_texture(canvas.texture);
                if let Some(dt) = canvas.depth_texture {
                    gl.delete_texture(dt);
                }
                gl.delete_framebuffer(canvas.fbo);
            });
        }
    });
}

pub(crate) fn destroy_all() {
    let handles: Vec<u64> = CANVASES.with(|c| c.borrow().keys().copied().collect());
    for handle in handles {
        stone_destroy_canvas(handle);
    }
}

/// Bind a canvas's colour attachment to a texture unit, resolved here from the
/// handle. The rendering path binds through this rather than fetching the
/// texture name and handing it back (`stone_bind_texture_to_unit`): a GL name
/// is the one thing about a canvas that cannot survive a change of graphics
/// backend, and the registry lookup was already happening on this side of the
/// ABI.
#[no_mangle]
pub extern "C" fn stone_bind_canvas_texture_to_unit(handle: u64, unit: c_int) -> bool {
    if handle == 0 {
        return false;
    }

    let tex = CANVASES.with(|canvases| canvases.borrow().get(&handle).map(|c| c.texture.0.get()));
    match tex {
        Some(t) => crate::shader::stone_bind_texture_to_unit(t, unit) != 0,
        None => false,
    }
}

pub(crate) fn canvas_texture_id(handle: u64) -> Option<u32> {
    if handle == 0 {
        return None;
    }
    CANVASES.with(|canvases| canvases.borrow().get(&handle).map(|c| c.texture.0.get()))
}

/// The depth sibling. False when the canvas has no depth attachment, so a
/// caller asking for one it does not have is refused rather than left with
/// whatever the unit held.
#[no_mangle]
pub extern "C" fn stone_bind_canvas_depth_to_unit(handle: u64, unit: c_int) -> bool {
    if handle == 0 {
        return false;
    }

    let tex = CANVASES.with(|canvases| {
        canvases
            .borrow()
            .get(&handle)
            .and_then(|c| c.depth_texture)
            .map(|t| t.0.get())
    });
    match tex {
        Some(t) => crate::shader::stone_bind_texture_to_unit(t, unit) != 0,
        None => false,
    }
}

/// Whether the registry still knows this handle. The binders answer false for a
/// dead canvas too, but they answer it at BIND time -- a consumer that wants to
/// refuse a pass before it clears its output needs to ask first.
#[no_mangle]
pub extern "C" fn stone_canvas_exists(handle: u64) -> bool {
    if handle == 0 {
        return false;
    }
    CANVASES.with(|canvases| canvases.borrow().contains_key(&handle))
}

/// Whether a canvas has a depth attachment. Asked directly because the caller's
/// question is about the attachment, not about a texture name -- comparing one
/// to zero answered it only as long as attachments had integer names.
#[no_mangle]
pub extern "C" fn stone_canvas_has_depth(handle: u64) -> bool {
    if handle == 0 {
        return false;
    }

    CANVASES.with(|canvases| {
        canvases
            .borrow()
            .get(&handle)
            .map(|c| c.depth_texture.is_some())
            .unwrap_or(false)
    })
}

/// The colour attachment's GL name. NOT for the rendering path, which binds
/// through `stone_bind_canvas_texture_to_unit` -- this exists for cimgui, whose
/// renderer is GL and whose `ImTextureRef_TextureID` takes a GL texture.
#[no_mangle]
pub extern "C" fn stone_get_canvas_texture(handle: u64) -> u32 {
    if handle == 0 {
        return 0;
    }

    CANVASES.with(|canvases| {
        canvases
            .borrow()
            .get(&handle)
            .map(|c| c.texture.0.get())
            .unwrap_or(0)
    })
}

/// The DEPTH attachment's GL name, or 0. Same standing as the accessor above:
/// the debug overlay's imgui path, not the rendering path. A separate accessor
/// rather than a second `Canvas` value -- the depth attachment is not a render
/// target and has no FBO of its own, so anything shaped like a Canvas would be
/// a lie about what can be done with it.
#[no_mangle]
pub extern "C" fn stone_get_canvas_depth_texture(handle: u64) -> u32 {
    if handle == 0 {
        return 0;
    }

    CANVASES.with(|canvases| {
        canvases
            .borrow()
            .get(&handle)
            .and_then(|c| c.depth_texture)
            .map(|t| t.0.get())
            .unwrap_or(0)
    })
}

/// MRT: attach the `id`-canvas's color texture as COLOR_ATTACHMENT1 of the
/// `color`-canvas's FBO. Leaves the FBO's draw-buffer list at its default
/// ([ATTACHMENT0]) -- the color pass toggles [0,1]/[0] per frame
/// (stone_set_draw_buffer_mask) so a later render into this canvas can't write the
/// id attachment. The renderer arc (R6) folds entity-id output into the main
/// color pass this way: one geometry traversal writes color to attachment 0
/// and entity-id to attachment 1 (the id-canvas's texture), deleting the
/// separate whole-scene entity_id pass. The id canvas keeps its own FBO for
/// pixel readback and its texture for the detect shader -- both read the SAME
/// texture object this attaches here, so they need no change.
///
/// Returns false (and logs) if either handle is unknown or the resulting FBO is
/// incomplete. Idempotent: re-attaching the same pair is a no-op-equivalent
/// rebind (used after a resolution-change canvas recreate).
#[no_mangle]
pub extern "C" fn stone_canvas_attach_id_target(color_handle: u64, id_handle: u64) -> bool {
    stone_canvas_attach_color_target(color_handle, id_handle, 1)
}

#[no_mangle]
pub extern "C" fn stone_canvas_attach_color_target(
    color_handle: u64,
    id_handle: u64,
    slot: i32,
) -> bool {
    if !(1..=4).contains(&slot) {
        crate::log_once!(
            "CANVAS",
            "stone_canvas_attach_color_target: unsupported slot {slot}"
        );
        return false;
    }
    if color_handle == 0 || id_handle == 0 {
        return false;
    }

    let resolved = CANVASES.with(|canvases| {
        let c = canvases.borrow();
        match (c.get(&color_handle), c.get(&id_handle)) {
            (Some(color), Some(id)) => Some((
                color.fbo,
                (color.width, color.height),
                id.texture,
                (id.width, id.height),
            )),
            _ => None,
        }
    });

    let (color_fbo, color_wh, id_texture, id_wh) = match resolved {
        Some(v) => v,
        None => {
            crate::log_once!(
                "CANVAS",
                "stone_canvas_attach_color_target: unknown handle(s) color={color_handle} id={id_handle}"
            );
            return false;
        }
    };

    if color_wh != id_wh {
        crate::log_once!(
            "CANVAS",
            "stone_canvas_attach_color_target: size mismatch color {:?} vs id {:?} -- MRT attachments must match",
            color_wh,
            id_wh
        );
        return false;
    }

    with_gl(|gl| unsafe {
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(color_fbo));
        gl.framebuffer_texture_2d(
            glow::FRAMEBUFFER,
            glow::COLOR_ATTACHMENT0 + slot as u32,
            glow::TEXTURE_2D,
            Some(id_texture),
            0,
        );
        // Draw-buffer state stays the FBO default ([ATTACHMENT0]); the color
        // pass toggles [0,1]/[0] per frame (stone_set_draw_buffer_mask) so a later
        // render into this canvas can't write the id attachment. Completeness
        // is independent of the draw-buffer list.
        let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
        let ok = status == glow::FRAMEBUFFER_COMPLETE;
        if !ok {
            crate::log_once!(
                "CANVAS",
                "stone_canvas_attach_color_target: FBO incomplete 0x{:X} after MRT attach",
                status
            );
            // Roll back the attachment we just made: leaving COLOR_ATTACHMENT1
            // bound to the id texture keeps the color FBO incomplete regardless
            // of the draw-buffer list, so the whole world stops rendering. The
            // caller degrades to single-buffer only when this FBO is complete.
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0 + slot as u32,
                glow::TEXTURE_2D,
                None,
                0,
            );
        }

        // Restore whatever FBO was bound.
        let current = CURRENT_FBO.with(|c| *c.borrow());
        gl.bind_framebuffer(glow::FRAMEBUFFER, resolve_target(current));
        ok
    })
    .unwrap_or(false)
}

/// Read a single channel from a canvas at (x, y) in screen coordinates (top-left origin).
/// Returns 0.0 if the canvas doesn't exist, coordinates are out of bounds, or the channel is invalid.
#[no_mangle]
pub extern "C" fn stone_read_canvas_pixel_channel(
    handle: u64,
    x: c_int,
    y: c_int,
    channel: c_int,
) -> c_float {
    if handle == 0 {
        return 0.0;
    }
    if !(0..=3).contains(&channel) {
        return 0.0;
    }

    let Some(canvas) = lookup_canvas(handle) else {
        return 0.0;
    };
    if x < 0 || y < 0 || x >= canvas.active_w || y >= canvas.active_h {
        return 0.0;
    }
    let mut pixel = [0.0; 4];
    if read_canvas_region_rgba32f(&canvas, x, y, 1, 1, &mut pixel) {
        pixel[channel as usize]
    } else {
        0.0
    }
}

/// Read the red channel from a canvas at (x, y) in screen coordinates.
/// For the entity_id canvas (RGBA32F), this is the entity ID.
#[no_mangle]
pub extern "C" fn stone_read_canvas_pixel(handle: u64, x: c_int, y: c_int) -> c_float {
    stone_read_canvas_pixel_channel(handle, x, y, 0)
}

/// Read an RGBA8 rect from the WINDOW (default framebuffer), (x, y) top-left
/// drawable coordinates, rows written top-down into `out` (w*h*4 bytes).
/// This sees the fully presented frame -- present blit, UI pool, everything --
/// which no canvas readback can. Only meaningful BEFORE the swap: the
/// post-swap back buffer is undefined, so callers go through the runner's
/// pre-swap window-probe capture, not a direct mid-frame call.
#[no_mangle]
pub extern "C" fn stone_read_window_rect(x: c_int, y: c_int, w: c_int, h: c_int, out: *mut u8) {
    if out.is_null() || w <= 0 || h <= 0 {
        return;
    }
    let size = crate::with_window(|win| {
        let (dw, dh) = win.window.drawable_size();
        (dw as i32, dh as i32)
    });
    let (dw, dh) = match size {
        Some(s) => s,
        None => return,
    };
    if x < 0 || y < 0 || x + w > dw || y + h > dh {
        return;
    }
    with_gl(|gl| unsafe {
        let prev_fbo = CURRENT_FBO.with(|c| *c.borrow());
        gl.bind_framebuffer(glow::FRAMEBUFFER, resolve_target(None));
        gl.pixel_store_i32(glow::PACK_ALIGNMENT, 1);
        let len = (w * h * 4) as usize;
        let slice = std::slice::from_raw_parts_mut(out, len);
        // One read, bottom-up (GL origin), then flip rows to top-down in place.
        let gl_y = dh - y - h;
        gl.read_pixels(
            x,
            gl_y,
            w,
            h,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelPackData::Slice(Some(slice)),
        );
        let row = (w * 4) as usize;
        for r in 0..(h as usize / 2) {
            let (a, b) = slice.split_at_mut(len - (r + 1) * row);
            a[r * row..r * row + row].swap_with_slice(&mut b[..row]);
        }
        gl.bind_framebuffer(glow::FRAMEBUFFER, resolve_target(prev_fbo));
    });
}

/// Read the depth buffer value at (x, y) from a canvas with a depth attachment.
/// Returns depth as a float in [0, 1]. Returns 1.0 (far plane) if no depth attachment.
#[no_mangle]
pub extern "C" fn stone_read_depth_pixel(handle: u64, x: c_int, y: c_int) -> c_float {
    if handle == 0 {
        return 1.0;
    }

    CANVASES.with(|canvases| {
        let canvases = canvases.borrow();
        let canvas = match canvases.get(&handle) {
            Some(c) => c,
            None => return 1.0,
        };

        if canvas.depth_texture.is_none() {
            return 1.0;
        }

        if x < 0 || y < 0 || x >= canvas.active_w || y >= canvas.active_h {
            return 1.0;
        }

        let gl_y = canvas.active_h - 1 - y;

        with_gl(|gl| unsafe {
            let prev_fbo = CURRENT_FBO.with(|c| *c.borrow());
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(canvas.fbo));

            let mut depth = [0.0f32; 1];
            gl.read_pixels(
                x,
                gl_y,
                1,
                1,
                glow::DEPTH_COMPONENT,
                glow::FLOAT,
                glow::PixelPackData::Slice(Some(bytemuck::cast_slice_mut(&mut depth))),
            );

            gl.bind_framebuffer(glow::FRAMEBUFFER, resolve_target(prev_fbo));
            depth[0]
        })
        .unwrap_or(1.0)
    })
}

/// Read the entire color buffer of an RGBA8 canvas into a caller-supplied
/// byte buffer. Pixels are returned top-to-bottom (already vertically
/// flipped from the GL bottom-left origin). Returns the number of bytes
/// written, or -1 on any failure (unknown handle, capacity too small,
/// readback error). Required size = canvas_w * canvas_h * 4.
#[no_mangle]
pub extern "C" fn stone_canvas_read_rgba8(handle: u64, out: *mut u8, capacity: c_int) -> c_int {
    if handle == 0 || out.is_null() || capacity <= 0 {
        return -1;
    }
    let Some(info) = lookup_canvas(handle) else {
        return -1;
    };
    let (fbo, w, h) = (info.fbo, info.width, info.height);
    let needed = (w as usize) * (h as usize) * 4;
    if (capacity as usize) < needed {
        return -1;
    }
    let pixels = with_gl(|gl| unsafe {
        let prev_fbo = CURRENT_FBO.with(|c| *c.borrow());
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
        let mut buf = vec![0u8; needed];
        gl.read_pixels(
            0,
            0,
            w,
            h,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelPackData::Slice(Some(buf.as_mut_slice())),
        );
        gl.bind_framebuffer(glow::FRAMEBUFFER, resolve_target(prev_fbo));
        buf
    });
    let pixels = match pixels {
        Some(p) => p,
        None => return -1,
    };
    let row = (w as usize) * 4;
    let height = h as usize;
    unsafe {
        for y in 0..height {
            let src = (height - 1 - y) * row;
            let dst = y * row;
            std::ptr::copy_nonoverlapping(pixels.as_ptr().add(src), out.add(dst), row);
        }
    }
    needed as c_int
}

fn read_canvas_region_rgba32f(
    canvas: &CanvasInfo,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    out: &mut [f32],
) -> bool {
    with_gl(|gl| unsafe {
        let previous = CURRENT_FBO.with(|c| *c.borrow());
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(canvas.fbo));
        let gl_y = canvas.active_h - y - h;
        if canvas.int_format {
            let mut small = [0i32; 4];
            let mut large = Vec::new();
            let pixels = if out.len() <= small.len() {
                &mut small[..out.len()]
            } else {
                large.resize(out.len(), 0);
                large.as_mut_slice()
            };
            gl.read_pixels(
                x,
                gl_y,
                w,
                h,
                glow::RGBA_INTEGER,
                glow::INT,
                glow::PixelPackData::Slice(Some(bytemuck::cast_slice_mut(pixels))),
            );
            for (dst, src) in out.iter_mut().zip(pixels.iter()) {
                *dst = *src as f32;
            }
        } else if canvas.float_format {
            gl.read_pixels(
                x,
                gl_y,
                w,
                h,
                glow::RGBA,
                glow::FLOAT,
                glow::PixelPackData::Slice(Some(bytemuck::cast_slice_mut(out))),
            );
        } else {
            let mut small = [0u8; 4];
            let mut large = Vec::new();
            let pixels = if out.len() <= small.len() {
                &mut small[..out.len()]
            } else {
                large.resize(out.len(), 0);
                large.as_mut_slice()
            };
            gl.read_pixels(
                x,
                gl_y,
                w,
                h,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut *pixels)),
            );
            for (dst, src) in out.iter_mut().zip(pixels.iter()) {
                *dst = *src as f32 / 255.0;
            }
        }
        gl.bind_framebuffer(glow::FRAMEBUFFER, resolve_target(previous));
        let row = w as usize * 4;
        for y in 0..h as usize / 2 {
            let opposite = h as usize - 1 - y;
            let (top, bottom) = out.split_at_mut(opposite * row);
            top[y * row..(y + 1) * row].swap_with_slice(&mut bottom[..row]);
        }
        true
    })
    .unwrap_or(false)
}

/// Read the active image as top-to-bottom RGBA floats; capacity and result count components.
#[no_mangle]
pub extern "C" fn stone_canvas_read_rgba32f(handle: u64, out: *mut f32, capacity: c_int) -> c_int {
    let Some(canvas) = lookup_canvas(handle) else {
        return -1;
    };
    stone_canvas_read_region_rgba32f(
        handle,
        0,
        0,
        canvas.active_w,
        canvas.active_h,
        out,
        capacity,
    )
}

/// Read the rect (x, y, w, h) of the active image, top-left origin, as
/// top-to-bottom RGBA floats in one readback. The rect must lie inside the
/// active extent; capacity and result count components.
#[no_mangle]
pub extern "C" fn stone_canvas_read_region_rgba32f(
    handle: u64,
    x: c_int,
    y: c_int,
    w: c_int,
    h: c_int,
    out: *mut f32,
    capacity: c_int,
) -> c_int {
    if out.is_null() || capacity <= 0 || x < 0 || y < 0 || w <= 0 || h <= 0 {
        return -1;
    }
    let Some(canvas) = lookup_canvas(handle) else {
        return -1;
    };
    if x > canvas.active_w - w || y > canvas.active_h - h {
        return -1;
    }
    let Some(needed) = (w as usize)
        .checked_mul(h as usize)
        .and_then(|n| n.checked_mul(4))
        .filter(|&n| n <= c_int::MAX as usize)
    else {
        return -1;
    };
    if (capacity as usize) < needed {
        return -1;
    }
    let pixels = unsafe { std::slice::from_raw_parts_mut(out, needed) };
    if read_canvas_region_rgba32f(&canvas, x, y, w, h, pixels) {
        needed as c_int
    } else {
        -1
    }
}

fn read_canvas_region_i32(
    canvas: &CanvasInfo,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    out: &mut [i32],
) -> bool {
    with_gl(|gl| unsafe {
        let previous = CURRENT_FBO.with(|c| *c.borrow());
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(canvas.fbo));
        gl.pixel_store_i32(glow::PACK_ALIGNMENT, 1);
        let gl_y = canvas.active_h - y - h;
        gl.read_pixels(
            x,
            gl_y,
            w,
            h,
            glow::RED_INTEGER,
            glow::INT,
            glow::PixelPackData::Slice(Some(bytemuck::cast_slice_mut(out))),
        );
        gl.bind_framebuffer(glow::FRAMEBUFFER, resolve_target(previous));
        let row = w as usize;
        for y in 0..h as usize / 2 {
            let opposite = h as usize - 1 - y;
            let (top, bottom) = out.split_at_mut(opposite * row);
            top[y * row..(y + 1) * row].swap_with_slice(&mut bottom[..row]);
        }
        true
    })
    .unwrap_or(false)
}

/// Read the rect (x, y, w, h) of the active image of an R32I canvas,
/// top-left origin, as top-to-bottom signed 32-bit integers in one readback.
/// One channel: the id. The rect must lie inside the active extent; capacity
/// and result count integers. Returns -1 for a non-integer canvas: ids never
/// travel the float readback path, which rounds above 2^24.
#[no_mangle]
pub extern "C" fn stone_canvas_read_region_i32(
    handle: u64,
    x: c_int,
    y: c_int,
    w: c_int,
    h: c_int,
    out: *mut i32,
    capacity: c_int,
) -> c_int {
    if out.is_null() || capacity <= 0 || x < 0 || y < 0 || w <= 0 || h <= 0 {
        return -1;
    }
    let Some(canvas) = lookup_canvas(handle) else {
        return -1;
    };
    if !canvas.int_format {
        return -1;
    }
    if x > canvas.active_w - w || y > canvas.active_h - h {
        return -1;
    }
    let Some(needed) = (w as usize)
        .checked_mul(h as usize)
        .filter(|&n| n <= c_int::MAX as usize)
    else {
        return -1;
    };
    if (capacity as usize) < needed {
        return -1;
    }
    let pixels = unsafe { std::slice::from_raw_parts_mut(out, needed) };
    if read_canvas_region_i32(&canvas, x, y, w, h, pixels) {
        needed as c_int
    } else {
        -1
    }
}

/// Upload top-to-bottom RGBA8 pixels into an existing RGBA8 canvas. The
/// caller owns the input buffer; GL keeps the canvas contents.
#[no_mangle]
pub unsafe extern "C" fn stone_canvas_upload_rgba8(
    handle: u64,
    pixels: *const u8,
    byte_len: c_int,
) -> bool {
    if handle == 0 || pixels.is_null() || byte_len <= 0 {
        return false;
    }
    let Some(info) = lookup_canvas(handle) else {
        return false;
    };
    if info.float_format || info.int_format {
        return false;
    }
    let needed = (info.width as usize)
        .checked_mul(info.height as usize)
        .and_then(|pixels| pixels.checked_mul(4));
    let Some(needed) = needed else { return false };
    if byte_len as usize != needed {
        return false;
    }
    let input = std::slice::from_raw_parts(pixels, needed);
    let row = info.width as usize * 4;
    let height = info.height as usize;
    let mut flipped = vec![0u8; needed];
    for y in 0..height {
        let src = y * row;
        let dst = (height - 1 - y) * row;
        flipped[dst..dst + row].copy_from_slice(&input[src..src + row]);
    }
    with_gl(|gl| unsafe {
        gl.bind_texture(glow::TEXTURE_2D, Some(info.texture));
        gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
        gl.tex_sub_image_2d(
            glow::TEXTURE_2D,
            0,
            0,
            0,
            info.width,
            info.height,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(Some(&flipped)),
        );
        gl.bind_texture(glow::TEXTURE_2D, None);
    })
    .is_some()
}

/// Upload RGBA32F data in native texture row order (first row at v=0).
/// Intended for shader data tables; no color conversion or row-flip allocation.
#[no_mangle]
pub unsafe extern "C" fn stone_canvas_upload_rgba32f(
    handle: u64,
    pixels: *const f32,
    count: c_int,
) -> bool {
    if pixels.is_null() || count <= 0 {
        return false;
    }
    let valid = CANVASES.with(|c| c.borrow().get(&handle).is_some_and(|c| c.format == 2));
    if !valid {
        return false;
    }
    let Some(info) = lookup_canvas(handle) else {
        return false;
    };
    let needed = (info.width as usize)
        .checked_mul(info.height as usize)
        .and_then(|n| n.checked_mul(4));
    if needed != Some(count as usize) {
        return false;
    }
    let Some(bytes) = (count as usize).checked_mul(std::mem::size_of::<f32>()) else {
        return false;
    };
    let input = std::slice::from_raw_parts(pixels.cast::<u8>(), bytes);
    let uploaded = with_gl(|gl| unsafe {
        gl.bind_texture(glow::TEXTURE_2D, Some(info.texture));
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::RGBA32F as i32,
            info.width,
            info.height,
            0,
            glow::RGBA,
            glow::FLOAT,
            glow::PixelUnpackData::Slice(Some(input)),
        );
        gl.bind_texture(glow::TEXTURE_2D, None);
    })
    .is_some();
    if uploaded {
        CANVASES.with(|c| {
            if let Some(canvas) = c.borrow_mut().get_mut(&handle) {
                canvas.bound_as_target = true;
            }
        });
    }
    uploaded
}

/// Save a caller-supplied RGBA8 buffer as a PNG. Pixels are top-to-bottom
/// (no flip). Returns false on null path or save failure.
#[no_mangle]
pub extern "C" fn stone_save_png_rgba8(
    path: *const c_char,
    width: c_int,
    height: c_int,
    pixels: *const u8,
) -> bool {
    if path.is_null() || pixels.is_null() || width <= 0 || height <= 0 {
        return false;
    }
    let path_str = match unsafe { CStr::from_ptr(path) }.to_str() {
        Ok(s) if !s.is_empty() => s,
        _ => return false,
    };
    let len = (width as usize) * (height as usize) * 4;
    let slice = unsafe { std::slice::from_raw_parts(pixels, len) };
    match image::save_buffer(
        path_str,
        slice,
        width as u32,
        height as u32,
        image::ExtendedColorType::Rgba8,
    ) {
        Ok(_) => true,
        Err(e) => {
            eprintln!("stone_save_png_rgba8: failed to save {}: {}", path_str, e);
            false
        }
    }
}

/// Read the entire color buffer of an RGBA8 canvas and save it as a PNG.
/// No resizing, no filtering — output is exactly canvas WxH. Returns false
/// on unknown handle, null path, or save failure (logged to stderr).
#[no_mangle]
pub extern "C" fn stone_canvas_save_png(handle: u64, path: *const c_char) -> bool {
    if handle == 0 || path.is_null() {
        return false;
    }
    let path_str = match unsafe { CStr::from_ptr(path) }.to_str() {
        Ok(s) if !s.is_empty() => s,
        _ => return false,
    };

    let Some(info) = lookup_canvas(handle) else {
        eprintln!("stone_canvas_save_png: unknown canvas handle {}", handle);
        return false;
    };
    let (fbo, w, h) = (info.fbo, info.width, info.height);
    let width = w as usize;
    let height = h as usize;

    let pixels = with_gl(|gl| unsafe {
        let prev_fbo = CURRENT_FBO.with(|c| *c.borrow());
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
        let mut buf = vec![0u8; width * height * 4];
        gl.read_pixels(
            0,
            0,
            w,
            h,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelPackData::Slice(Some(buf.as_mut_slice())),
        );
        gl.bind_framebuffer(glow::FRAMEBUFFER, resolve_target(prev_fbo));
        buf
    });
    let pixels = match pixels {
        Some(p) => p,
        None => return false,
    };

    match stone_capture::save_rgba_png(
        path_str,
        w as u32,
        h as u32,
        stone_capture::RowOrigin::BottomUp,
        pixels,
        0,
    ) {
        Ok(_) => true,
        Err(e) => {
            ::log::error!(target: "SCREENSHOT", "stone_canvas_save_png: {e}");
            false
        }
    }
}

pub(crate) fn get_canvas_count() -> usize {
    CANVASES.with(|cvs| cvs.borrow().len())
}

/// Estimated GPU bytes held by live canvas attachments. Color formats have a
/// fixed texel size; DEPTH_COMPONENT24 packing is driver-defined, so its
/// storage is estimated at four bytes per pixel.
pub(crate) fn live_canvas_bytes() -> u64 {
    CANVASES.with(|canvases| {
        canvases
            .borrow()
            .values()
            .map(|canvas| {
                let color_bpp = match canvas.format {
                    1 => 8u64,
                    2 => 16u64,
                    _ => 4u64,
                };
                let pixels = canvas.width.max(0) as u64 * canvas.height.max(0) as u64;
                pixels * (color_bpp + if canvas.depth_texture.is_some() { 4 } else { 0 })
            })
            .sum()
    })
}

#[derive(Clone, Copy)]
pub(crate) struct CanvasInfo {
    pub fbo: glow::Framebuffer,
    pub texture: glow::Texture,
    pub width: i32,
    pub height: i32,
    pub active_w: i32,
    pub active_h: i32,
    pub has_depth: bool,
    pub float_format: bool,
    pub int_format: bool,
}

/// Look up a canvas handle's FBO, dimensions, and attachment facts.
/// Returns None if the handle is unknown. Used by pixel_readback for async
/// single-pixel reads.
pub(crate) fn lookup_canvas(handle: u64) -> Option<CanvasInfo> {
    CANVASES.with(|cvs| {
        cvs.borrow().get(&handle).map(|c| CanvasInfo {
            fbo: c.fbo,
            texture: c.texture,
            width: c.width,
            height: c.height,
            active_w: c.active_w,
            active_h: c.active_h,
            has_depth: c.depth_texture.is_some(),
            float_format: is_float_format(c.format),
            int_format: is_int_format(c.format),
        })
    })
}

pub(crate) fn current_fbo() -> Option<glow::Framebuffer> {
    CURRENT_FBO.with(|c| *c.borrow())
}

pub(crate) fn capture_texture_is_owned(texture: u32) -> bool {
    CANVASES.with(|c| {
        c.borrow().values().any(|c| {
            c.texture.0.get() == texture || c.depth_texture.is_some_and(|t| t.0.get() == texture)
        })
    })
}

pub(crate) fn capture_resources() -> serde_json::Value {
    CANVASES.with(|c| c.borrow().iter().map(|(handle, c)| serde_json::json!({
        "handle": handle, "texture": c.texture.0.get(), "framebuffer": c.fbo.0.get(),
        "depth_texture": c.depth_texture.map(|t| t.0.get()), "format": c.format,
        "width": c.width, "height": c.height, "active_width": c.active_w, "active_height": c.active_h,
    })).collect())
}

pub(crate) fn capture_texture_owner(texture: u32) -> serde_json::Value {
    CANVASES.with(|c| c.borrow().iter().find_map(|(handle, c)| {
        let depth = c.depth_texture.is_some_and(|t| t.0.get() == texture);
        (c.texture.0.get() == texture || depth).then(|| serde_json::json!({
            "canvas_handle": handle, "depth": depth, "active_width": c.active_w, "active_height": c.active_h,
        }))
    })).unwrap_or(serde_json::Value::Null)
}

pub(crate) fn capture_depth_texture(handle: u64) -> Option<glow::Texture> {
    CANVASES.with(|c| c.borrow().get(&handle).and_then(|c| c.depth_texture))
}
