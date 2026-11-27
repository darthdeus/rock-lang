use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{c_char, c_float, c_int, CStr};

use crate::gl_state::DRAW_BUFFERS;
use crate::handles::Class;
use crate::{track_canvas_switch, with_gl, with_window};

#[derive(Clone, Copy)]
pub(crate) struct Canvas {
    pub fbo: glow::Framebuffer,
    pub texture: glow::Texture,
    // A texture, not a renderbuffer: the depth overlay samples it.
    depth_texture: Option<glow::Texture>,
    pub width: i32,
    pub height: i32,
    // Active image within the allocation, anchored at the GL origin.
    pub active_w: i32,
    pub active_h: i32,
    format: c_int,
    bound_as_target: bool,
    pub generation: u64,
    // Secondary color attachment handles (slots 1..DRAW_BUFFERS) owned by this
    // target and resized/destroyed atomically with it.
    owned_attachments: [u64; DRAW_BUFFERS],
    // All secondary color attachment handles currently attached to this FBO
    // (owned or attached via stone_canvas_attach_color_target).
    attached_slots: [u64; DRAW_BUFFERS],
    // The target whose owned attachment this is (0 = none). Only the owner
    // resizes, re-attaches or destroys it.
    owner: u64,
    color_source: u64,
}

impl Canvas {
    pub fn has_depth(&self) -> bool {
        self.depth_texture.is_some()
    }

    pub fn float_format(&self) -> bool {
        matches!(self.format, 1 | 2)
    }

    // R32I: readbacks use RGBA_INTEGER/INT; clears use glClearBufferiv (glClear is undefined).
    pub fn int_format(&self) -> bool {
        self.format == 3
    }

    /// Re-specify storage at `width` x `height` (contents become creation's)
    /// and leave the FBO bound. Attachments name the texture objects and survive.
    unsafe fn respecify(&self, gl: &crate::DebugGl, width: i32, height: i32) -> u32 {
        let (internal_format, pixel_format, pixel_type, _) = color_format(self.format);
        let color = (self.texture, internal_format, pixel_format, pixel_type);
        let depth = self.depth_texture.map(|depth| {
            (
                depth,
                glow::DEPTH_COMPONENT24,
                glow::DEPTH_COMPONENT,
                glow::UNSIGNED_INT,
            )
        });
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.fbo));
        for (texture, internal_format, pixel_format, pixel_type) in
            std::iter::once(color).chain(depth)
        {
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                internal_format as i32,
                width,
                height,
                0,
                pixel_format,
                pixel_type,
                glow::PixelUnpackData::Slice(None::<&[u8]>),
            );
        }
        gl.bind_texture(glow::TEXTURE_2D, None);
        let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
        if status == glow::FRAMEBUFFER_COMPLETE {
            establish_blank_contents(gl, self.int_format(), self.has_depth());
        }
        status
    }

    unsafe fn respecify_texture_only(&self, gl: &crate::DebugGl, width: i32, height: i32) {
        let (internal_format, pixel_format, pixel_type, _) = color_format(self.format);
        gl.bind_texture(glow::TEXTURE_2D, Some(self.texture));
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            internal_format as i32,
            width,
            height,
            0,
            pixel_format,
            pixel_type,
            glow::PixelUnpackData::Slice(None::<&[u8]>),
        );
        gl.bind_texture(glow::TEXTURE_2D, None);
    }

    unsafe fn respecify_with_children(
        &self,
        gl: &crate::DebugGl,
        children: &[(usize, Canvas)],
        width: i32,
        height: i32,
    ) -> u32 {
        for (_, child) in children {
            child.respecify_texture_only(gl, width, height);
        }
        let (internal_format, pixel_format, pixel_type, _) = color_format(self.format);
        let color = (self.texture, internal_format, pixel_format, pixel_type);
        let depth = self.depth_texture.map(|depth| {
            (
                depth,
                glow::DEPTH_COMPONENT24,
                glow::DEPTH_COMPONENT,
                glow::UNSIGNED_INT,
            )
        });
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.fbo));
        for (texture, internal_format, pixel_format, pixel_type) in
            std::iter::once(color).chain(depth)
        {
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                internal_format as i32,
                width,
                height,
                0,
                pixel_format,
                pixel_type,
                glow::PixelUnpackData::Slice(None::<&[u8]>),
            );
        }
        gl.bind_texture(glow::TEXTURE_2D, None);
        let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
        if status == glow::FRAMEBUFFER_COMPLETE {
            if children.is_empty() {
                establish_blank_contents(gl, self.int_format(), self.has_depth());
            } else {
                let mut slots = [(0usize, false); DRAW_BUFFERS];
                slots[0] = (0, self.int_format());
                let mut count = 1usize;
                for &(slot, child) in children {
                    if count < DRAW_BUFFERS {
                        slots[count] = (slot, child.int_format());
                        count += 1;
                    }
                }
                establish_blank_contents_multi(gl, &slots[..count], self.has_depth());
            }
        }
        status
    }

    unsafe fn delete(&self, gl: &crate::DebugGl) {
        if self.color_source == 0 {
            gl.delete_texture(self.texture);
        }
        if let Some(depth) = self.depth_texture {
            gl.delete_texture(depth);
        }
        gl.delete_framebuffer(self.fbo);
    }
}

fn rebind_current(gl: &crate::DebugGl) {
    unsafe { gl.bind_framebuffer(glow::FRAMEBUFFER, resolve_target(current_fbo())) };
}

thread_local! {
    static CANVASES: RefCell<HashMap<u64, Canvas>> = RefCell::new(HashMap::new());
    static COLOR_VIEWS: RefCell<HashMap<u64, Vec<u64>>> = RefCell::new(HashMap::new());
    static CURRENT_FBO: RefCell<Option<glow::Framebuffer>> = const { RefCell::new(None) };
    static CURRENT_CANVAS_HANDLE: Cell<u64> = const { Cell::new(0) };
    static CURRENT_TARGET_SIZE: RefCell<(f32, f32)> = const { RefCell::new((0.0, 0.0)) };
    // Letterbox rectangle in logical window pixels with a top-left origin;
    // the GL viewport uses bottom-origin drawable pixels.
    static PRESENT_VIEWPORT: RefCell<Option<(i32, i32, i32, i32)>> = const { RefCell::new(None) };
    static DEFAULT_FBO: RefCell<Option<glow::Framebuffer>> = const { RefCell::new(None) };
}

pub(crate) fn current_canvas_handle() -> u64 {
    CURRENT_CANVAS_HANDLE.with(Cell::get)
}

pub(crate) fn current_target_size_raw() -> (f32, f32) {
    CURRENT_TARGET_SIZE.with(|c| *c.borrow())
}

pub(crate) fn restore_target_cache(handle: u64, fbo: Option<glow::Framebuffer>, size: (f32, f32)) {
    CURRENT_CANVAS_HANDLE.with(|c| c.set(handle));
    CURRENT_FBO.with(|c| *c.borrow_mut() = fbo);
    CURRENT_TARGET_SIZE.with(|c| *c.borrow_mut() = size);
}

pub(crate) fn reset_current_canvas_handle() {
    CURRENT_CANVAS_HANDLE.with(|c| c.set(0));
    CURRENT_FBO.with(|c| *c.borrow_mut() = None);
    CURRENT_TARGET_SIZE.with(|c| *c.borrow_mut() = (0.0, 0.0));
}

fn with_canvas<T>(handle: u64, f: impl FnOnce(&Canvas) -> T) -> Option<T> {
    CANVASES.with(|canvases| canvases.borrow().get(&handle).map(f))
}

fn with_canvas_mut<T>(handle: u64, f: impl FnOnce(&mut Canvas) -> T) -> Option<T> {
    CANVASES.with(|canvases| canvases.borrow_mut().get_mut(&handle).map(f))
}

pub(crate) fn set_default_framebuffer(fbo: Option<glow::Framebuffer>) {
    HEADLESS_SCREEN.with(|h| *h.borrow_mut() = None);
    DEFAULT_FBO.with(|d| *d.borrow_mut() = fbo);
}

/// None denotes the screen framebuffer.
pub(crate) fn resolve_target(fbo: Option<glow::Framebuffer>) -> Option<glow::Framebuffer> {
    fbo.or_else(|| DEFAULT_FBO.with(|d| *d.borrow()))
}

/// Never-mapped X11 windows can retain the default framebuffer's creation size
/// after resize. Own a drawable-sized RGB8/depth-24 framebuffer instead.
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
        // A renderbuffer name is not an object until first bound; attaching it unbound is
        // INVALID_OPERATION.
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

/// The screen framebuffer must be bound.
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

// Derive the ortho from the currently bound target. An FFI struct-by-value
// return can abort a LuaJIT trace, so expose two scalar getters.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_get_live_canvas_bytes() -> u64 {
    live_canvas_bytes()
}

/// rock-ffi stone-rendering private
#[no_mangle]
pub extern "C" fn stone_current_target_width() -> c_float {
    current_target_size().0
}

/// rock-ffi stone-rendering private
#[no_mangle]
pub extern "C" fn stone_current_target_height() -> c_float {
    current_target_size().1
}

/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_clear_canvas(r: c_float, g: c_float, b: c_float, a: c_float) {
    with_gl(|gl| unsafe {
        gl.clear_color(r, g, b, a);
        gl.clear(glow::COLOR_BUFFER_BIT);
    });
}

/// Rect is (x, y, w, h) in logical window px, top-left origin. Re-bind canvas 0 to apply.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_set_present_viewport(active: bool, x: c_int, y: c_int, w: c_int, h: c_int) {
    PRESENT_VIEWPORT.with(|p| {
        *p.borrow_mut() = if active { Some((x, y, w, h)) } else { None };
    });
}

/// Set a canvas's active rectangle: the live image inside its allocated
/// capacity, anchored at the GL origin. Clamped into [1, capacity]. Takes
/// effect at the next bind of that canvas.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_set_canvas_active(handle: u64, w: c_int, h: c_int) {
    let changed = with_canvas_mut(handle, |canvas| {
        if canvas.color_source != 0 {
            return false;
        }
        let new_w = w.clamp(1, canvas.width);
        let new_h = h.clamp(1, canvas.height);
        if canvas.active_w != new_w || canvas.active_h != new_h {
            canvas.active_w = new_w;
            canvas.active_h = new_h;
            canvas.generation = crate::handles::next_generation();
            return true;
        }
        false
    });
    if changed == Some(true) {
        destroy_color_views(handle);
    }
}

/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_set_viewport_rect(x: c_int, y: c_int, w: c_int, h: c_int) {
    if w <= 0 || h <= 0 {
        crate::log_once!("CANVAS", "stone_set_viewport_rect: empty rect {}x{}", w, h);
        return;
    }
    let (lw, lh) = current_target_size();
    let (sx, sy, ph) = if current_fbo().is_none() {
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

/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_set_canvas(handle: u64) {
    if handle == 0 {
        with_gl(|gl| unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, resolve_target(None));
            gl.disable(glow::SCISSOR_TEST);
        });
        CURRENT_FBO.with(|c| *c.borrow_mut() = None);
        CURRENT_CANVAS_HANDLE.with(|c| c.set(0));

        with_window(|window| {
            let (dw, dh) = window.window.drawable_size();
            let (lw, lh) = window.window.size();
            let pv = PRESENT_VIEWPORT.with(|p| *p.borrow());
            with_gl(|gl| unsafe {
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
        });
        track_canvas_switch();
        return;
    }

    with_canvas_mut(handle, |canvas| {
        canvas.bound_as_target = true;
        with_gl(|gl| unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(canvas.fbo));
            gl.viewport(0, 0, canvas.active_w, canvas.active_h);
            // glClear ignores the viewport but respects the scissor, which keeps clears off the
            // inactive margin.
            if canvas.active_w < canvas.width || canvas.active_h < canvas.height {
                gl.enable(glow::SCISSOR_TEST);
                gl.scissor(0, 0, canvas.active_w, canvas.active_h);
            } else {
                gl.disable(glow::SCISSOR_TEST);
            }
        });
        CURRENT_FBO.with(|c| *c.borrow_mut() = Some(canvas.fbo));
        CURRENT_CANVAS_HANDLE.with(|c| c.set(handle));
        CURRENT_TARGET_SIZE
            .with(|c| *c.borrow_mut() = (canvas.active_w as f32, canvas.active_h as f32));
    });

    track_canvas_switch();
}

// STONE_CANVAS_RGBA8 = 0, RGBA16F = 1, RGBA32F = 2, R32I = 3 (integer entity-id target).
// (internal format, pixel format, pixel type, bytes per texel)
fn color_format(format: c_int) -> (u32, u32, u32, u64) {
    match format {
        1 => (glow::RGBA16F, glow::RGBA, glow::HALF_FLOAT, 8),
        2 => (glow::RGBA32F, glow::RGBA, glow::FLOAT, 16),
        3 => (glow::R32I, glow::RED_INTEGER, glow::INT, 4),
        _ => (glow::RGBA8, glow::RGBA, glow::UNSIGNED_BYTE, 4),
    }
}

/// Texture allocation leaves contents unspecified. Clear them, restoring GL
/// state because creation and resize can run during a render.
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

/// Clear every color attachment in `slots` (pair of `(slot_index, is_int_format)`)
/// and optional depth, restoring all touched GL state including draw buffers.
unsafe fn establish_blank_contents_multi(
    gl: &crate::DebugGl,
    slots: &[(usize, bool)],
    has_depth: bool,
) {
    let saved_scissor = gl.is_enabled(glow::SCISSOR_TEST);
    let saved_color_mask = gl.get_parameter_bool_array::<4>(glow::COLOR_WRITEMASK);
    let saved_depth_mask = gl.get_parameter_bool(glow::DEPTH_WRITEMASK);
    let mut saved_clear_color = [0.0f32; 4];
    gl.get_parameter_f32_slice(glow::COLOR_CLEAR_VALUE, &mut saved_clear_color);
    let mut saved_clear_depth = [0.0f32; 1];
    gl.get_parameter_f32_slice(glow::DEPTH_CLEAR_VALUE, &mut saved_clear_depth);

    let max_slot = slots.iter().map(|&(s, _)| s).max().unwrap_or(0);
    let count = (max_slot + 1).min(DRAW_BUFFERS);
    let mut saved_draw_buffers = [glow::NONE; DRAW_BUFFERS];
    for (i, dst) in saved_draw_buffers.iter_mut().enumerate().take(count) {
        *dst = gl.get_parameter_i32(glow::DRAW_BUFFER0 + i as u32) as u32;
    }

    gl.disable(glow::SCISSOR_TEST);
    gl.color_mask(true, true, true, true);

    let mut non_int_bufs = [glow::NONE; DRAW_BUFFERS];
    let mut has_non_int = false;
    let mut all_bufs = [glow::NONE; DRAW_BUFFERS];
    let mut has_int = false;
    for &(slot, is_int) in slots {
        if slot >= DRAW_BUFFERS {
            continue;
        }
        all_bufs[slot] = glow::COLOR_ATTACHMENT0 + slot as u32;
        if is_int {
            has_int = true;
        } else {
            non_int_bufs[slot] = glow::COLOR_ATTACHMENT0 + slot as u32;
            has_non_int = true;
        }
    }
    if has_non_int {
        gl.draw_buffers(&non_int_bufs[..count]);
        gl.clear_color(0.0, 0.0, 0.0, 0.0);
        gl.clear(glow::COLOR_BUFFER_BIT);
    }
    if has_int {
        gl.draw_buffers(&all_bufs[..count]);
        for &(slot, is_int) in slots {
            if is_int && slot < DRAW_BUFFERS {
                gl.clear_buffer_i32_slice(glow::COLOR, slot as u32, &[0, 0, 0, 0]);
            }
        }
    }
    if has_depth {
        crate::gl_state::clear_depth_unmasked(gl, 1.0);
    }

    let restore_len = saved_draw_buffers[..count]
        .iter()
        .rposition(|&b| b != glow::NONE)
        .map_or(1, |idx| idx + 1);
    gl.draw_buffers(&saved_draw_buffers[..restore_len]);
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

fn canvas_dimensions_fit(width: i32, height: i32, limit: i32) -> bool {
    limit > 0 && width > 0 && height > 0 && width <= limit && height <= limit
}

fn canvas_dimensions_supported(gl: &crate::DebugGl, width: i32, height: i32) -> bool {
    let limit = unsafe { gl.get_parameter_i32(glow::MAX_TEXTURE_SIZE) };
    if canvas_dimensions_fit(width, height, limit) {
        return true;
    }
    ::log::error!(target: "CANVAS", "canvas {}x{} is outside GL texture size range 1..={}; refusing allocation/resize", width, height, limit);
    false
}

#[test]
fn canvas_dimensions_respect_both_device_axes() {
    assert!(canvas_dimensions_fit(16384, 1, 16384));
    assert!(canvas_dimensions_fit(1, 16384, 16384));
    assert!(!canvas_dimensions_fit(16385, 1, 16384));
    assert!(!canvas_dimensions_fit(1, 16385, 16384));
    assert!(!canvas_dimensions_fit(1, 1, 0));
    assert!(!canvas_dimensions_fit(0, 1, 16384));
    assert!(!canvas_dimensions_fit(1, -1, 16384));
}

/// rock-ffi stone-rendering private
#[no_mangle]
pub extern "C" fn stone_create_canvas(
    width: c_int,
    height: c_int,
    format: c_int,
    with_depth: bool,
) -> u64 {
    with_gl(|gl| unsafe {
        if !canvas_dimensions_supported(gl, width, height) {
            return 0;
        }
        let fbo = match gl.create_framebuffer() {
            Ok(fbo) => fbo,
            Err(e) => {
                crate::log_once!("CANVAS", "Failed to create framebuffer ({}x{}): {}", width, height, e);
                return 0;
            }
        };
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));

        let texture = match gl.create_texture() {
            Ok(tex) => tex,
            Err(e) => {
                crate::log_once!("CANVAS", "Failed to create canvas texture ({}x{}): {}", width, height, e);
                gl.delete_framebuffer(fbo);
                return 0;
            }
        };
        let depth_texture = if with_depth {
            gl.create_texture()
                .inspect_err(|e| crate::log_once!("CANVAS", "Failed to create depth texture ({}x{}): {} (depth testing disabled for this canvas)", width, height, e))
                .ok()
        } else {
            None
        };
        // NEAREST without mips: a linear-filtered integer texture is incomplete, and so
        // is a depth texture under the default MIPMAP min filter.
        for tex in std::iter::once(texture).chain(depth_texture) {
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::NEAREST as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::NEAREST as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
        }
        if depth_texture.is_some() {
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_COMPARE_MODE, glow::NONE as i32);
        }
        let canvas = Canvas {
            fbo,
            texture,
            depth_texture,
            width,
            height,
            active_w: width,
            active_h: height,
            format,
            bound_as_target: false,
            generation: crate::handles::next_generation(),
            owned_attachments: [0; DRAW_BUFFERS],
            attached_slots: [0; DRAW_BUFFERS],
            owner: 0,
            color_source: 0,
        };
        gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(texture), 0);
        gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::DEPTH_ATTACHMENT, glow::TEXTURE_2D, depth_texture, 0);
        let status = canvas.respecify(gl, width, height);
        if status != glow::FRAMEBUFFER_COMPLETE {
            crate::log_once!("CANVAS", "Framebuffer not complete: 0x{:X} ({}x{})", status, width, height);
            canvas.delete(gl);
            gl.bind_framebuffer(glow::FRAMEBUFFER, resolve_target(None));
            return 0;
        }
        rebind_current(gl);

        let id = crate::handles::mint(Class::Canvas);
        CANVASES.with(|canvases| canvases.borrow_mut().insert(id, canvas));
        id
    })
    .unwrap_or(0)
}

/// Integer color buffers require an integer clear. glClear is undefined for R32I.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_clear_canvas_int(draw_buffer: c_int, value: c_int) {
    with_gl(|gl| unsafe {
        gl.clear_buffer_i32_slice(glow::COLOR, draw_buffer as u32, &[value, 0, 0, 0]);
    });
}

/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_canvas_bound_as_target(handle: u64) -> bool {
    with_canvas(handle, |c| c.bound_as_target).unwrap_or(false)
}

/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_canvas_active_is_creation(handle: u64) -> bool {
    with_canvas(handle, |c| c.active_w == c.width && c.active_h == c.height).unwrap_or(false)
}

/// Active width, or -1 for an unknown handle.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_canvas_active_w(handle: u64) -> c_int {
    with_canvas(handle, |c| c.active_w).unwrap_or(-1)
}

/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_canvas_active_h(handle: u64) -> c_int {
    with_canvas(handle, |c| c.active_h).unwrap_or(-1)
}

/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_canvas_capacity_w(handle: u64) -> c_int {
    with_canvas(handle, |c| c.width).unwrap_or(-1)
}

/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_canvas_capacity_h(handle: u64) -> c_int {
    with_canvas(handle, |c| c.height).unwrap_or(-1)
}

/// Live canvas handles (`native_ledger`).
pub(crate) fn live_canvas_ids() -> Vec<u64> {
    CANVASES.with(|c| c.borrow().keys().copied().collect())
}

/// Asset-report census over the canvas registry: population and one handle
/// per index in id order.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_get_live_canvas_count() -> c_int {
    get_canvas_count().min(c_int::MAX as usize) as c_int
}

/// rock-ffi stone-rendering
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

/// rock-ffi stone-rendering private
#[no_mangle]
pub extern "C" fn stone_canvas_report(handle: u64, width: *mut c_int, height: *mut c_int) -> c_int {
    if handle == 0 || width.is_null() || height.is_null() {
        return -1;
    }
    let found = lookup_canvas(handle)
        .filter(|canvas| canvas.color_source == 0)
        .map(|canvas| (canvas.width, canvas.height, canvas.format));
    let Some((w, h, format)) = found else {
        return -1;
    };
    unsafe {
        *width = w;
        *height = h;
    }
    format
}

/// Re-specify storage while retaining the handle, framebuffer and textures.
/// Invalid requests fail; an incomplete framebuffer restores old storage.
/// `bound_as_target` remains set if the canvas was ever painted.
/// rock-ffi stone-rendering private
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
    let Some(canvas) = lookup_canvas(handle) else {
        crate::log_once!(
            "CANVAS",
            "stone_canvas_resize: unknown canvas handle {}",
            handle
        );
        return false;
    };
    if canvas.color_source != 0 || refuse_owned_attachment("stone_canvas_resize", handle) {
        return false;
    }
    if (canvas.width, canvas.height) == (width, height) {
        return true;
    }

    let mut children: Vec<(usize, u64, Canvas)> = Vec::new();
    for (slot, &child_handle) in canvas.owned_attachments.iter().enumerate().skip(1) {
        if child_handle != 0 {
            let Some(child) = lookup_canvas(child_handle) else {
                return false;
            };
            children.push((slot, child_handle, child));
        }
    }
    let child_pairs: Vec<(usize, Canvas)> = children.iter().map(|&(s, _, c)| (s, c)).collect();

    let resized = with_gl(|gl| unsafe {
        if !canvas_dimensions_supported(gl, width, height) {
            return None;
        }
        let status = canvas.respecify_with_children(gl, &child_pairs, width, height);
        let complete = status == glow::FRAMEBUFFER_COMPLETE;
        if !complete {
            crate::log_once!(
                "CANVAS",
                "stone_canvas_resize: canvas {} is incomplete at {}x{} (0x{:X}); keeping {}x{}",
                handle,
                width,
                height,
                status,
                canvas.width,
                canvas.height
            );
            canvas.respecify_with_children(gl, &child_pairs, canvas.width, canvas.height);
        }
        rebind_current(gl);
        Some(complete)
    });
    let Some(respecified) = resized else {
        crate::log_once!(
            "CANVAS",
            "stone_canvas_resize: no GL context for canvas {}",
            handle
        );
        return false;
    };
    let Some(complete) = respecified else {
        return false;
    };

    // Re-specified storage has new contents even when the old size was kept.
    for target in std::iter::once(handle).chain(children.iter().map(|&(_, child, _)| child)) {
        with_canvas_mut(target, |c| {
            if complete {
                c.width = width;
                c.height = height;
                c.active_w = width;
                c.active_h = height;
            }
            c.generation = crate::handles::next_generation();
        });
        destroy_color_views(target);
    }
    complete
}

/// True (and logged) when `handle` is an attachment owned by another target.
fn refuse_owned_attachment(operation: &str, handle: u64) -> bool {
    let owner = with_canvas(handle, |c| c.owner).unwrap_or(0);
    if owner != 0 {
        ::log::error!(
            target: "CANVAS",
            "{operation}: canvas {handle} is an attachment owned by target {owner}; refused (use the owner)"
        );
    }
    owner != 0
}

/// rock-ffi stone-rendering private
#[no_mangle]
pub extern "C" fn stone_destroy_canvas(handle: u64) {
    if handle == 0 || refuse_owned_attachment("stone_destroy_canvas", handle) {
        return;
    }
    destroy_canvas(handle);
}

fn destroy_color_views(source: u64) {
    let views = COLOR_VIEWS.with_borrow_mut(|views| views.remove(&source));
    if let Some(views) = views {
        for view in views {
            destroy_canvas(view);
        }
    }
}

fn destroy_canvas(handle: u64) {
    let removed = CANVASES.with(|canvases| canvases.borrow_mut().remove(&handle));
    if let Some(canvas) = removed {
        if canvas.color_source == 0 {
            destroy_color_views(handle);
        } else {
            COLOR_VIEWS.with_borrow_mut(|views| {
                if let Some(siblings) = views.get_mut(&canvas.color_source) {
                    siblings.retain(|&view| view != handle);
                    if siblings.is_empty() {
                        views.remove(&canvas.color_source);
                    }
                }
            });
        }
        if CURRENT_CANVAS_HANDLE.with(Cell::get) == handle {
            if canvas.color_source != 0 {
                stone_set_canvas(0);
            } else {
                CURRENT_CANVAS_HANDLE.with(|c| c.set(0));
                CURRENT_FBO.with(|c| *c.borrow_mut() = None);
            }
        }
        crate::handles::retire(Class::Canvas, handle);
        with_gl(|gl| unsafe { canvas.delete(gl) });
        for child_handle in canvas.owned_attachments.into_iter().skip(1) {
            if child_handle != 0 {
                destroy_canvas(child_handle);
            }
        }
    }
}

/// Create a native-owned render target with `attachment_count` (1..=8) colour
/// attachments whose formats are `STONE_CANVAS_*` codes plus an optional depth
/// attachment. Owned secondary attachments resize and release atomically with
/// the returned target handle.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_target_create(
    width: c_int,
    height: c_int,
    attachment_formats: *const c_int,
    attachment_count: c_int,
    with_depth: bool,
) -> u64 {
    if attachment_formats.is_null()
        || !(1..=DRAW_BUFFERS as c_int).contains(&attachment_count)
        || width < 1
        || height < 1
    {
        return 0;
    }
    let formats =
        unsafe { std::slice::from_raw_parts(attachment_formats, attachment_count as usize) };
    if formats.iter().any(|f| !(0..=3).contains(f)) {
        return 0;
    }
    let primary = stone_create_canvas(width, height, formats[0], with_depth);
    if primary == 0 {
        return 0;
    }
    if !create_target_attachments(
        primary,
        formats,
        |format| stone_create_canvas(width, height, format, false),
        |child, slot| {
            if !stone_canvas_attach_color_target(primary, child, slot as i32) {
                return false;
            }
            with_canvas_mut(primary, |c| c.owned_attachments[slot] = child);
            with_canvas_mut(child, |c| c.owner = primary);
            true
        },
        |handle| stone_destroy_canvas(handle),
    ) {
        return 0;
    }
    if formats.len() > 1 {
        if let Some(canvas) = lookup_canvas(primary) {
            let mut slots = [(0usize, false); DRAW_BUFFERS];
            for (slot, &fmt) in formats.iter().enumerate() {
                slots[slot] = (slot, fmt == 3);
            }
            with_gl(|gl| unsafe {
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(canvas.fbo));
                establish_blank_contents_multi(gl, &slots[..formats.len()], with_depth);
                rebind_current(gl);
            });
        }
    }
    primary
}

fn create_target_attachments(
    primary: u64,
    formats: &[c_int],
    mut create: impl FnMut(c_int) -> u64,
    attach: impl Fn(u64, usize) -> bool,
    destroy: impl Fn(u64),
) -> bool {
    for (slot, &format) in formats.iter().enumerate().skip(1) {
        let child = create(format);
        if child == 0 {
            destroy(primary);
            return false;
        }
        if !attach(child, slot) {
            destroy(child);
            destroy(primary);
            return false;
        }
    }
    true
}

#[test]
fn target_create_releases_children_and_primary_on_attach_failure() {
    let mut children = [2, 3].into_iter();
    let operations = RefCell::new(Vec::new());
    let result = create_target_attachments(
        1,
        &[0, 0, 0, 0],
        |_format| {
            let child = children.next().unwrap();
            operations.borrow_mut().push(("create", child));
            child
        },
        |child, slot| {
            operations.borrow_mut().push(("attach", child));
            slot == 1
        },
        |handle| operations.borrow_mut().push(("destroy", handle)),
    );
    assert!(!result);
    assert_eq!(
        operations.into_inner(),
        [
            ("create", 2),
            ("attach", 2),
            ("create", 3),
            ("attach", 3),
            ("destroy", 3),
            ("destroy", 1),
        ]
    );
}

/// Borrow a target's primary color image as output zero of a color-only FBO.
/// The returned handle is retired whenever the source's generation changes.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_target_color_view(source: u64) -> u64 {
    with_gl(|gl| unsafe {
        let Some(canvas) = lookup_canvas(source).filter(|canvas| canvas.color_source == 0) else {
            return 0;
        };
        let Ok(fbo) = gl.create_framebuffer() else {
            return 0;
        };
        let previous = crate::native_renderer::read_framebuffer(gl, glow::DRAW_FRAMEBUFFER_BINDING);
        gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(fbo));
        gl.framebuffer_texture_2d(
            glow::DRAW_FRAMEBUFFER,
            glow::COLOR_ATTACHMENT0,
            glow::TEXTURE_2D,
            Some(canvas.texture),
            0,
        );
        let status = gl.check_framebuffer_status(glow::DRAW_FRAMEBUFFER);
        gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, previous);
        if status != glow::FRAMEBUFFER_COMPLETE {
            gl.delete_framebuffer(fbo);
            return 0;
        }

        let view = Canvas {
            fbo,
            depth_texture: None,
            bound_as_target: false,
            generation: crate::handles::next_generation(),
            owned_attachments: [0; DRAW_BUFFERS],
            attached_slots: [0; DRAW_BUFFERS],
            owner: 0,
            color_source: source,
            ..canvas
        };
        let handle = crate::handles::mint(Class::Canvas);
        CANVASES.with_borrow_mut(|canvases| canvases.insert(handle, view));
        COLOR_VIEWS.with_borrow_mut(|views| views.entry(source).or_default().push(handle));
        handle
    })
    .unwrap_or(0)
}

/// Resize a native-owned render target and all of its owned attachments.
/// Invalidates cached handle generations on success.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_target_resize(handle: u64, width: c_int, height: c_int) -> bool {
    stone_canvas_resize(handle, width, height)
}

/// Destroy a native-owned render target and all of its owned attachments.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_target_destroy(handle: u64) {
    stone_destroy_canvas(handle);
}

/// Return the canvas handle for color attachment `slot` (0 = primary target,
/// 1..7 = secondary attached target), or 0 if absent or invalid.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_target_attachment(handle: u64, slot: c_int) -> u64 {
    if handle == 0 || !(0..DRAW_BUFFERS as c_int).contains(&slot) {
        return 0;
    }
    if slot == 0 {
        return if stone_canvas_exists(handle) {
            handle
        } else {
            0
        };
    }
    let Some(att) = with_canvas(handle, |c| c.attached_slots[slot as usize]) else {
        return 0;
    };
    if att != 0 && stone_canvas_exists(att) {
        att
    } else {
        0
    }
}

/// GL texture name of colour attachment `slot` of target `handle` (`0` = the
/// target's own image), or 0. Borrowed: never delete it or change its
/// parameters. It stays the same while the attachment's generation does.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_target_gl_texture(handle: u64, slot: c_int) -> u32 {
    if with_gl(|_| ()).is_none() {
        return 0;
    }
    canvas_texture_id(stone_target_attachment(handle, slot)).unwrap_or(0)
}

/// GL name of target `handle`'s depth texture, or 0 without depth. Borrowed.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_target_gl_depth_texture(handle: u64) -> u32 {
    if handle == 0 || with_gl(|_| ()).is_none() {
        return 0;
    }
    with_canvas(handle, |c| c.depth_texture.map_or(0, |t| t.0.get())).unwrap_or(0)
}

/// GL name of target `handle`'s framebuffer, or 0. Borrowed: bind it, but never
/// change its attachments. Restore any draw or read buffer you change on it
/// before the stage ends; the stage itself restores only the draw buffers of
/// the framebuffer bound when it began.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_target_gl_framebuffer(handle: u64) -> u32 {
    if handle == 0 || with_gl(|_| ()).is_none() {
        return 0;
    }
    with_canvas(handle, |c| c.fbo.0.get()).unwrap_or(0)
}

pub(crate) fn canvas_generation(handle: u64) -> u64 {
    if handle == 0 {
        return 0;
    }
    with_canvas(handle, |c| c.generation).unwrap_or(0)
}

pub(crate) fn canvas_info(handle: u64) -> Option<(u64, i32, i32, c_int, i32, bool)> {
    if handle == 0 {
        return None;
    }
    let c = lookup_canvas(handle)?;
    let mut color_attachments = 1i32;
    for &att in c.attached_slots.iter().skip(1) {
        if att != 0 && stone_canvas_exists(att) {
            color_attachments += 1;
        }
    }
    Some((
        c.generation,
        c.active_w,
        c.active_h,
        c.format,
        color_attachments,
        c.has_depth(),
    ))
}

pub(crate) fn destroy_all() {
    let handles: Vec<u64> = CANVASES.with(|c| c.borrow().keys().copied().collect());
    for handle in handles {
        destroy_canvas(handle);
    }
}

/// Bind a canvas texture by handle without exposing the backend GL name.
/// rock-ffi stone-rendering private
#[no_mangle]
pub extern "C" fn stone_bind_canvas_texture_to_unit(handle: u64, unit: c_int) -> bool {
    if handle == 0 {
        return false;
    }

    let tex = with_canvas(handle, |c| c.texture.0.get());
    match tex {
        Some(t) => crate::shader::stone_bind_texture_to_unit(t, unit) != 0,
        None => false,
    }
}

pub(crate) fn canvas_texture_id(handle: u64) -> Option<u32> {
    if handle == 0 {
        return None;
    }
    with_canvas(handle, |c| c.texture.0.get())
}

/// rock-ffi stone-rendering private
#[no_mangle]
pub extern "C" fn stone_bind_canvas_depth_to_unit(handle: u64, unit: c_int) -> bool {
    if handle == 0 {
        return false;
    }

    let tex = with_canvas(handle, |c| c.depth_texture.map(|t| t.0.get())).flatten();
    match tex {
        Some(t) => crate::shader::stone_bind_texture_to_unit(t, unit) != 0,
        None => false,
    }
}

/// rock-ffi stone-rendering private
#[no_mangle]
pub extern "C" fn stone_canvas_exists(handle: u64) -> bool {
    if handle == 0 {
        return false;
    }
    with_canvas(handle, |_| ()).is_some()
}

/// rock-ffi stone-rendering private
#[no_mangle]
pub extern "C" fn stone_canvas_has_depth(handle: u64) -> bool {
    if handle == 0 {
        return false;
    }

    with_canvas(handle, |c| c.depth_texture.is_some()).unwrap_or(false)
}

/// The colour attachment's GL name. NOT for the rendering path, which binds
/// through `stone_bind_canvas_texture_to_unit` -- this exists for cimgui, whose
/// renderer is GL and whose `ImTextureRef_TextureID` takes a GL texture.
/// rock-ffi stone-rendering private
#[no_mangle]
pub extern "C" fn stone_get_canvas_texture(handle: u64) -> u32 {
    if handle == 0 {
        return 0;
    }

    with_canvas(handle, |c| c.texture.0.get()).unwrap_or(0)
}

/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_canvas_attach_id_target(color_handle: u64, id_handle: u64) -> bool {
    stone_canvas_attach_color_target(color_handle, id_handle, 1)
}

/// rock-ffi stone-rendering rename:id_handle=target_handle
#[no_mangle]
pub extern "C" fn stone_canvas_attach_color_target(
    color_handle: u64,
    id_handle: u64,
    slot: i32,
) -> bool {
    if !(1..crate::gl_state::DRAW_BUFFERS as i32).contains(&slot) {
        crate::log_once!(
            "CANVAS",
            "stone_canvas_attach_color_target: unsupported slot {slot}"
        );
        return false;
    }
    if color_handle == 0 || id_handle == 0 {
        return false;
    }
    if refuse_owned_attachment("stone_canvas_attach_color_target", color_handle)
        || refuse_owned_attachment("stone_canvas_attach_color_target", id_handle)
    {
        return false;
    }
    if let Some(owned) = with_canvas(color_handle, |c| c.owned_attachments[slot as usize]) {
        if owned != 0 {
            ::log::error!(
                target: "CANVAS",
                "stone_canvas_attach_color_target: slot {slot} of target {color_handle} holds its owned attachment {owned}; refused"
            );
            return false;
        }
    }

    let (Some(color), Some(id)) = (lookup_canvas(color_handle), lookup_canvas(id_handle)) else {
        crate::log_once!(
            "CANVAS",
            "stone_canvas_attach_color_target: unknown handle(s) color={color_handle} id={id_handle}"
        );
        return false;
    };
    if color.color_source != 0 || id.color_source != 0 {
        return false;
    }
    let (color_wh, id_wh) = ((color.width, color.height), (id.width, id.height));
    if color_wh != id_wh {
        crate::log_once!(
            "CANVAS",
            "stone_canvas_attach_color_target: size mismatch color {:?} vs id {:?} -- MRT attachments must match",
            color_wh,
            id_wh
        );
        return false;
    }

    let ok = with_gl(|gl| unsafe {
        let attach = |texture| {
            let attachment = glow::COLOR_ATTACHMENT0 + slot as u32;
            gl.framebuffer_texture_2d(glow::FRAMEBUFFER, attachment, glow::TEXTURE_2D, texture, 0);
        };
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(color.fbo));
        attach(Some(id.texture));
        let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
        let ok = status == glow::FRAMEBUFFER_COMPLETE;
        if !ok {
            crate::log_once!(
                "CANVAS",
                "stone_canvas_attach_color_target: FBO incomplete 0x{:X} after MRT attach",
                status
            );
            attach(None);
        }
        rebind_current(gl);
        ok
    })
    .unwrap_or(false);
    if ok {
        with_canvas_mut(color_handle, |c| {
            c.attached_slots[slot as usize] = id_handle;
            c.generation = crate::handles::next_generation();
        });
        destroy_color_views(color_handle);
    }
    ok
}

/// Read a single channel from a canvas at (x, y) in screen coordinates (top-left origin).
/// Returns 0.0 if the canvas doesn't exist, coordinates are out of bounds, or the channel is invalid.
/// rock-ffi stone-rendering
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
    if read_canvas_region_rgba32f(&canvas, (x, y, 1, 1), &mut pixel) {
        pixel[channel as usize]
    } else {
        0.0
    }
}

/// Read the red channel at top-left screen coordinates.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_read_canvas_pixel(handle: u64, x: c_int, y: c_int) -> c_float {
    stone_read_canvas_pixel_channel(handle, x, y, 0)
}

/// (x, y) are top-left drawable coordinates. Valid only before the swap: the post-swap
/// back buffer is undefined.
/// rock-ffi stone-rendering private
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
    let out = unsafe { std::slice::from_raw_parts_mut(out, (w * h * 4) as usize) };
    read_rows_top_down(None, dh, (x, y, w, h), glow::RGBA, glow::UNSIGNED_BYTE, out);
}

/// Read colour 0 of the bound read framebuffer when it is a framebuffer object.
/// The read buffer is framebuffer state native code may change, so readbacks
/// select it instead of assuming the default. The window's own framebuffer
/// keeps its default.
pub(crate) unsafe fn set_color_read_buffer(gl: &crate::DebugGl, fbo: Option<glow::Framebuffer>) {
    if fbo.is_some() {
        gl.read_buffer(glow::COLOR_ATTACHMENT0);
    }
}

/// Read `w` x `h` texels at top-left `(x, y)` of a framebuffer whose image is
/// `height` tall (None is the screen) into `out`, rows top-down.
pub(crate) fn read_rows_top_down(
    fbo: Option<glow::Framebuffer>,
    height: i32,
    (x, y, w, h): (i32, i32, i32, i32),
    format: u32,
    ty: u32,
    out: &mut [u8],
) -> bool {
    with_gl(|gl| unsafe {
        let target = resolve_target(fbo);
        gl.bind_framebuffer(glow::FRAMEBUFFER, target);
        set_color_read_buffer(gl, target);
        gl.pixel_store_i32(glow::PACK_ALIGNMENT, 1);
        gl.read_pixels(
            x,
            height - y - h,
            w,
            h,
            format,
            ty,
            glow::PixelPackData::Slice(Some(&mut *out)),
        );
        rebind_current(gl);
    })
    .is_some_and(|()| {
        let row = out.len() / h as usize;
        for top in 0..h as usize / 2 {
            let bottom = h as usize - 1 - top;
            let (upper, lower) = out.split_at_mut(bottom * row);
            upper[top * row..(top + 1) * row].swap_with_slice(&mut lower[..row]);
        }
        true
    })
}

/// Read the depth buffer value at (x, y) from a canvas with a depth attachment.
/// Returns depth as a float in [0, 1]. Returns 1.0 (far plane) if no depth attachment.
/// rock-ffi stone-rendering private
#[no_mangle]
pub extern "C" fn stone_read_depth_pixel(handle: u64, x: c_int, y: c_int) -> c_float {
    let Some(canvas) = lookup_canvas(handle) else {
        return 1.0;
    };
    if !canvas.has_depth() || x < 0 || y < 0 || x >= canvas.active_w || y >= canvas.active_h {
        return 1.0;
    }
    let mut depth = [0.0f32];
    let read = read_rows_top_down(
        Some(canvas.fbo),
        canvas.active_h,
        (x, y, 1, 1),
        glow::DEPTH_COMPONENT,
        glow::FLOAT,
        bytemuck::cast_slice_mut(&mut depth),
    );
    if read {
        depth[0]
    } else {
        1.0
    }
}

/// Read the entire color buffer of an RGBA8 canvas into a caller-supplied
/// byte buffer. Pixels are returned top-to-bottom (already vertically
/// flipped from the GL bottom-left origin). Returns the number of bytes
/// written, or -1 on any failure (unknown handle, capacity too small,
/// readback error). Required size = canvas_w * canvas_h * 4.
/// rock-ffi stone-rendering private
#[no_mangle]
pub extern "C" fn stone_canvas_read_rgba8(handle: u64, out: *mut u8, capacity: c_int) -> c_int {
    if handle == 0 || out.is_null() || capacity <= 0 {
        return -1;
    }
    let Some(info) = lookup_canvas(handle) else {
        return -1;
    };
    let needed = (info.width as usize) * (info.height as usize) * 4;
    if (capacity as usize) < needed {
        return -1;
    }
    let out = unsafe { std::slice::from_raw_parts_mut(out, needed) };
    if read_canvas_rgba8(&info, out) {
        needed as c_int
    } else {
        -1
    }
}

/// The whole capacity, not the active image.
fn read_canvas_rgba8(info: &Canvas, out: &mut [u8]) -> bool {
    let rect = (0, 0, info.width, info.height);
    read_rows_top_down(
        Some(info.fbo),
        info.height,
        rect,
        glow::RGBA,
        glow::UNSIGNED_BYTE,
        out,
    )
}

fn read_canvas_region_rgba32f(
    canvas: &Canvas,
    rect: (i32, i32, i32, i32),
    out: &mut [f32],
) -> bool {
    let read = |format, ty, bytes: &mut [u8]| {
        read_rows_top_down(Some(canvas.fbo), canvas.active_h, rect, format, ty, bytes)
    };
    if canvas.float_format() {
        return read(glow::RGBA, glow::FLOAT, bytemuck::cast_slice_mut(out));
    }
    // Single-pixel reads run per frame and stay off the heap.
    let mut small = [0i32; 4];
    let mut large = Vec::new();
    let texels = if out.len() <= small.len() {
        &mut small[..out.len()]
    } else {
        large.resize(out.len(), 0);
        large.as_mut_slice()
    };
    if canvas.int_format() {
        let read = read(
            glow::RGBA_INTEGER,
            glow::INT,
            bytemuck::cast_slice_mut(texels),
        );
        if read {
            for (dst, src) in out.iter_mut().zip(texels.iter()) {
                *dst = *src as f32;
            }
        }
        read
    } else {
        let bytes = &mut bytemuck::cast_slice_mut::<i32, u8>(texels)[..out.len()];
        let read = read(glow::RGBA, glow::UNSIGNED_BYTE, bytes);
        if read {
            for (dst, src) in out.iter_mut().zip(bytes.iter()) {
                *dst = *src as f32 / 255.0;
            }
        }
        read
    }
}

/// Validate a read of `channels` values per texel inside the active image
/// against the caller's capacity; returns the value count `read` filled, or -1.
fn read_region<T>(
    handle: u64,
    (x, y, w, h): (i32, i32, i32, i32),
    channels: usize,
    (out, capacity): (*mut T, c_int),
    read: impl FnOnce(&Canvas, &mut [T]) -> bool,
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
        .and_then(|n| n.checked_mul(channels))
        .filter(|&n| n <= c_int::MAX as usize && n <= capacity as usize)
    else {
        return -1;
    };
    let values = unsafe { std::slice::from_raw_parts_mut(out, needed) };
    if read(&canvas, values) {
        needed as c_int
    } else {
        -1
    }
}

/// rock-ffi stone-rendering
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
    read_region(
        handle,
        (x, y, w, h),
        4,
        (out, capacity),
        |canvas, values| read_canvas_region_rgba32f(canvas, (x, y, w, h), values),
    )
}

/// rock-ffi stone-rendering private
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
    read_region(
        handle,
        (x, y, w, h),
        1,
        (out, capacity),
        |canvas, values| {
            let (format, ty) = (glow::RED_INTEGER, glow::INT);
            let bytes = bytemuck::cast_slice_mut(values);
            canvas.int_format()
                && read_rows_top_down(
                    Some(canvas.fbo),
                    canvas.active_h,
                    (x, y, w, h),
                    format,
                    ty,
                    bytes,
                )
        },
    )
}

/// Upload top-to-bottom RGBA8 pixels into an existing RGBA8 canvas. The
/// caller owns the input buffer; GL keeps the canvas contents.
/// rock-ffi stone-rendering private
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
    if info.float_format() || info.int_format() {
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
/// rock-ffi stone-rendering
#[no_mangle]
pub unsafe extern "C" fn stone_canvas_upload_rgba32f(
    handle: u64,
    pixels: *const f32,
    count: c_int,
) -> bool {
    if pixels.is_null() || count <= 0 {
        return false;
    }
    let Some(info) = lookup_canvas(handle).filter(|c| c.format == 2) else {
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
        with_canvas_mut(handle, |canvas| canvas.bound_as_target = true);
    }
    uploaded
}

/// rock-ffi stone-rendering
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
    let mut pixels = vec![0u8; info.width as usize * info.height as usize * 4];
    if !read_canvas_rgba8(&info, &mut pixels) {
        return false;
    }

    match stone_capture::save_rgba_png(
        path_str,
        info.width as u32,
        info.height as u32,
        stone_capture::RowOrigin::TopDown,
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
            .filter(|canvas| canvas.color_source == 0)
            .map(|canvas| {
                let color_bytes = color_format(canvas.format).3;
                let pixels = canvas.width.max(0) as u64 * canvas.height.max(0) as u64;
                pixels * (color_bytes + if canvas.has_depth() { 4 } else { 0 })
            })
            .sum()
    })
}

pub(crate) fn canvas_capacity(handle: u64) -> Option<(i32, i32)> {
    with_canvas(handle, |c| (c.width, c.height))
}

pub(crate) fn canvas_framebuffer(handle: u64) -> Option<glow::Framebuffer> {
    with_canvas(handle, |c| c.fbo)
}

pub(crate) fn lookup_canvas(handle: u64) -> Option<Canvas> {
    with_canvas(handle, |c| *c)
}

pub(crate) fn current_fbo() -> Option<glow::Framebuffer> {
    CURRENT_FBO.with(|c| *c.borrow())
}

pub(crate) fn capture_texture_is_owned(texture: u32) -> bool {
    !capture_texture_owner(texture).is_null()
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
        (c.color_source == 0 && (c.texture.0.get() == texture || depth)).then(|| serde_json::json!({
            "canvas_handle": handle, "depth": depth, "active_width": c.active_w, "active_height": c.active_h,
        }))
    })).unwrap_or(serde_json::Value::Null)
}

pub(crate) fn capture_depth_texture(handle: u64) -> Option<glow::Texture> {
    with_canvas(handle, |c| c.depth_texture).flatten()
}
