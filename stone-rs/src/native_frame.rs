//! Per-frame entry points for a native renderer and the native world image.
//!
//! A native library registers hooks once; the engine's frame calls
//! `stone_native_frame_hooks_run` at fixed points of world rendering, and each
//! hook runs inside its own native stage and GPU scope.

use std::cell::{Cell, RefCell};
use std::ffi::{c_char, c_float, c_int, c_void, CStr, CString};

use crate::native_renderer::{stone_native_stage_begin, stone_native_stage_end};
use crate::with_gl;

pub const STONE_FRAME_HOOK_WORLD_BEGIN: u32 = 1;
pub const STONE_FRAME_HOOK_WORLD_BATCHES: u32 = 2;
pub const STONE_FRAME_HOOK_SCENE_END: u32 = 3;

pub const STONE_WORLD_IMAGE_KEEP_POST: u32 = 1;
/// Reserved: the world image is the composite and post processing is skipped.
pub const STONE_WORLD_IMAGE_SKIP_POST: u32 = 2;

/// `stone_native_frame_hooks_run` result bits above the stage flags.
pub const STONE_FRAME_HOOKS_WROTE: u32 = 1 << 8;
pub const STONE_FRAME_HOOKS_WORLD_IMAGE: u32 = 1 << 9;

/// What a hook may rely on for this frame. Read only during the call.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct StoneFrameInfo {
    pub struct_size: u32,
    pub point: u32,
    pub frame: u64,
    pub context_generation: u64,
    /// The scene target the world renders into this frame, and its partner.
    pub scene_target: u64,
    pub scene_partner: u64,
    pub scene_generation: u64,
    pub target_width: c_int,
    pub target_height: c_int,
    pub active_width: c_int,
    pub active_height: c_int,
    /// The camera script batches draw with: `stone_set_camera(offset, scale)`.
    pub world_to_scene_offset_x: c_float,
    pub world_to_scene_offset_y: c_float,
    pub world_to_scene_scale: c_float,
    pub _reserved: u32,
}

/// Returns 0 when it wrote nothing, 1 when it wrote the scene target, or a
/// negative value for a failure the engine logs.
pub type StoneFrameHookFn = unsafe extern "C" fn(*mut c_void, *const StoneFrameInfo) -> i32;
pub type StoneFrameHookReleaseFn = unsafe extern "C" fn(*mut c_void);

struct Hook {
    id: u64,
    point: u32,
    order: c_int,
    run: StoneFrameHookFn,
    release: Option<StoneFrameHookReleaseFn>,
    userdata: *mut c_void,
    name: CString,
    removed: bool,
}

#[derive(Clone, Copy)]
struct WorldImage {
    target: u64,
    frame: u64,
}

thread_local! {
    static HOOKS: RefCell<Vec<Hook>> = const { RefCell::new(Vec::new()) };
    static LAST_HOOK_ID: Cell<u64> = const { Cell::new(0) };
    static DISPATCHING: Cell<Option<(u32, u64, StoneFrameInfo)>> = const { Cell::new(None) };
    static WORLD_IMAGE: Cell<Option<WorldImage>> = const { Cell::new(None) };
    // `target` is an imported image handle here, not an engine target.
    static HOST_WORLD_IMAGE: Cell<Option<WorldImage>> = const { Cell::new(None) };
    // The read framebuffer the host image is attached to for its copy, with
    // the context generation it was created in.
    static HOST_IMAGE_FBO: Cell<Option<(u64, glow::Framebuffer)>> = const { Cell::new(None) };
}

/// Register `run` (a `StoneFrameHookFn`) to be called at `point` every frame,
/// in ascending `order` and then registration order. `release` (a
/// `StoneFrameHookReleaseFn`, may be null) runs once on unregister or at
/// teardown while the GL context is still current. `name` (may be null) names
/// the hook's GPU scope. Returns a non-zero hook id, or 0 with a logged error.
/// A hook registered during dispatch first runs at the next dispatch.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_native_frame_hook_register(
    point: u32,
    order: c_int,
    run: *const c_void,
    release: *const c_void,
    userdata: *mut c_void,
    name: *const c_char,
) -> u64 {
    if with_gl(|_| ()).is_none() {
        return 0;
    }
    if !(STONE_FRAME_HOOK_WORLD_BEGIN..=STONE_FRAME_HOOK_SCENE_END).contains(&point) {
        ::log::error!(target: "STONE", "stone_native_frame_hook_register: unknown point {point}");
        return 0;
    }
    if run.is_null() {
        ::log::error!(target: "STONE", "stone_native_frame_hook_register: null hook");
        return 0;
    }
    // The caller passes functions with these C signatures (rendering_ffi.h).
    let run: StoneFrameHookFn = unsafe { std::mem::transmute(run) };
    let release: Option<StoneFrameHookReleaseFn> =
        (!release.is_null()).then(|| unsafe { std::mem::transmute(release) });
    let name = if name.is_null() {
        c"native_frame_hook".to_owned()
    } else {
        unsafe { CStr::from_ptr(name) }.to_owned()
    };
    let id = LAST_HOOK_ID.with(|last| {
        last.set(last.get() + 1);
        last.get()
    });
    HOOKS.with(|hooks| {
        hooks.borrow_mut().push(Hook {
            id,
            point,
            order,
            run,
            release,
            userdata,
            name,
            removed: false,
        })
    });
    id
}

/// Unregister `hook` and call its release. Inside a dispatch the hook stops
/// running at once and its release runs when the dispatch ends.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_native_frame_hook_unregister(hook: u64) -> bool {
    let found = HOOKS.with(|hooks| {
        let mut hooks = hooks.borrow_mut();
        let found = hooks.iter_mut().find(|h| h.id == hook && !h.removed);
        found.map(|h| h.removed = true).is_some()
    });
    if !found {
        ::log::error!(target: "STONE", "stone_native_frame_hook_unregister: hook {hook} is not registered");
        return false;
    }
    if DISPATCHING.with(Cell::get).is_none() {
        release_removed();
    }
    true
}

fn release_removed() {
    let removed: Vec<Hook> = HOOKS.with(|hooks| {
        let mut hooks = hooks.borrow_mut();
        let (removed, kept) = hooks.drain(..).partition(|h| h.removed);
        *hooks = kept;
        removed
    });
    for hook in removed {
        if let Some(release) = hook.release {
            unsafe { release(hook.userdata) };
        }
    }
}

/// Release every hook while the context is current; teardown calls it first.
pub(crate) fn release_all() {
    HOOKS.with(|hooks| hooks.borrow_mut().iter_mut().for_each(|h| h.removed = true));
    release_removed();
    WORLD_IMAGE.with(|w| w.set(None));
    HOST_WORLD_IMAGE.with(|w| w.set(None));
    if let Some((_, fbo)) = HOST_IMAGE_FBO.with(Cell::take) {
        with_gl(|gl| unsafe { gl.delete_framebuffer(fbo) });
    }
    DISPATCHING.with(|d| d.set(None));
}

/// Run the hooks registered at `point` for this frame. The engine calls this
/// at each point with the frame's scene target pair and world transform; the
/// scene target is bound while hooks run, and the binding before the call is
/// restored after. Returns the OR of the hooks' `STONE_STAGE_*` flags plus
/// `STONE_FRAME_HOOKS_WROTE` when a hook wrote the scene target and, at
/// `WORLD_BEGIN`, `STONE_FRAME_HOOKS_WORLD_IMAGE` when a hook supplied the
/// world image (already copied into the scene target when it is another
/// target) or the host set one with `stone_frame_set_host_world_image`.
/// Returns 0 when no hook ran and no host image applied. Each hook's stage restores GL state
/// and Stone's caches, and the caller's target is rebound through
/// `stone_set_canvas`, so the caller's script-side caches stay valid.
/// rock-ffi stone-rendering private
#[no_mangle]
pub extern "C" fn stone_native_frame_hooks_run(
    point: u32,
    scene_target: u64,
    scene_partner: u64,
    offset_x: c_float,
    offset_y: c_float,
    scale: c_float,
) -> u32 {
    if DISPATCHING.with(Cell::get).is_some() {
        ::log::error!(target: "STONE", "stone_native_frame_hooks_run: called from inside a frame hook");
        return 0;
    }
    let mut due: Vec<(c_int, u64)> = HOOKS.with(|hooks| {
        hooks
            .borrow()
            .iter()
            .filter(|h| h.point == point && !h.removed)
            .map(|h| (h.order, h.id))
            .collect()
    });
    let host_image = if point == STONE_FRAME_HOOK_WORLD_BEGIN {
        let frame = crate::stream_buffer::frame();
        HOST_WORLD_IMAGE
            .with(|w| w.take())
            .filter(|image| image.frame == frame)
            .map(|image| image.target)
    } else {
        None
    };
    if due.is_empty() && host_image.is_none() {
        return 0;
    }
    let Some(context_generation) = with_gl(|_| crate::handles::context_generation()) else {
        return 0;
    };
    let Some((scene_generation, active_w, active_h, _, _, _)) =
        crate::canvas::canvas_info(scene_target)
    else {
        ::log::error!(target: "STONE", "stone_native_frame_hooks_run: scene target {scene_target} is not live");
        return 0;
    };
    let (target_w, target_h) =
        crate::canvas::canvas_capacity(scene_target).unwrap_or((active_w, active_h));
    due.sort_unstable();

    let frame = crate::stream_buffer::frame();
    let info = StoneFrameInfo {
        struct_size: std::mem::size_of::<StoneFrameInfo>() as u32,
        point,
        frame,
        context_generation,
        scene_target,
        scene_partner,
        scene_generation,
        target_width: target_w,
        target_height: target_h,
        active_width: active_w,
        active_height: active_h,
        world_to_scene_offset_x: offset_x,
        world_to_scene_offset_y: offset_y,
        world_to_scene_scale: scale,
        _reserved: 0,
    };
    if point == STONE_FRAME_HOOK_WORLD_BEGIN {
        WORLD_IMAGE.with(|w| w.set(None));
    }
    // Hooks run after it and may draw over it or replace it.
    let host_image_copied =
        host_image.is_some_and(|image| copy_host_image(image, scene_target, active_w, active_h));

    // Each hook's own stage restores GL state; the dispatch only binds the
    // scene target and gives the caller's target back.
    let previous_target = crate::canvas::current_canvas_handle();
    if previous_target != scene_target {
        crate::canvas::stone_set_canvas(scene_target);
    }
    let mut flags = 0;
    DISPATCHING.with(|d| d.set(Some((point, frame, info))));
    for (_, id) in due {
        let hook = HOOKS.with(|hooks| {
            let hooks = hooks.borrow();
            hooks
                .iter()
                .find(|h| h.id == id && !h.removed)
                .map(|h| (h.run, h.userdata, h.name.clone()))
        });
        let Some((run, userdata, name)) = hook else {
            continue;
        };
        unsafe { crate::stone_gpu_scope_begin(name.as_ptr()) };
        let scope = stone_native_stage_begin();
        let result = unsafe { run(userdata, &info) };
        let stage = stone_native_stage_end(scope);
        crate::stone_gpu_scope_end();
        flags |= stage;
        match result {
            0 => {}
            1 => flags |= STONE_FRAME_HOOKS_WROTE,
            other => {
                if crate::rock_log_reexport::once_should_emit_for(
                    concat!(file!(), ":", line!()),
                    id,
                ) {
                    ::log::error!(
                        target: "STONE",
                        "frame hook {} ({}) failed with {other}",
                        id,
                        name.to_string_lossy()
                    );
                }
            }
        }
    }
    DISPATCHING.with(|d| d.set(None));

    if point == STONE_FRAME_HOOK_WORLD_BEGIN {
        if let Some(image) = current_world_image() {
            let copied = image == scene_target
                || crate::canvas::canvas_framebuffer(image)
                    .is_some_and(|fbo| copy_color(fbo, scene_target, active_w, active_h));
            if !copied {
                ::log::error!(target: "STONE", "world image {image}: copy into scene target {scene_target} failed");
                WORLD_IMAGE.with(|w| w.set(None));
            }
        }
        if current_world_image().is_some() || host_image_copied {
            flags |= STONE_FRAME_HOOKS_WORLD_IMAGE;
        }
    }
    if previous_target != scene_target {
        crate::canvas::stone_set_canvas(previous_target);
    }
    release_removed();
    flags
}

/// Supply this frame's world image from a `WORLD_BEGIN` hook. With
/// `STONE_WORLD_IMAGE_KEEP_POST` the engine skips its own world rendering for
/// the frame (scene clear, batches, scene passes) and still runs post
/// processing, overlays and UI on it. `target` is an engine target whose
/// active size covers the scene's; colour 0 is copied into the scene target
/// unless it is the scene target. Returns false with a logged error outside a
/// `WORLD_BEGIN` hook, for an invalid target, or for unsupported flags
/// (`STONE_WORLD_IMAGE_SKIP_POST` is reserved); the frame then renders through
/// the engine path.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_native_frame_set_world_image(target: u64, flags: u32) -> bool {
    let Some((point, frame, info)) = DISPATCHING.with(Cell::get) else {
        ::log::error!(target: "STONE", "stone_native_frame_set_world_image: only a WORLD_BEGIN frame hook may set the world image");
        return false;
    };
    if point != STONE_FRAME_HOOK_WORLD_BEGIN {
        ::log::error!(target: "STONE", "stone_native_frame_set_world_image: called from hook point {point}, not WORLD_BEGIN");
        return false;
    }
    if flags != STONE_WORLD_IMAGE_KEEP_POST {
        ::log::error!(target: "STONE", "stone_native_frame_set_world_image: unsupported flags 0x{flags:X} (only KEEP_POST)");
        return false;
    }
    let Some((_, w, h, format, _, _)) = crate::canvas::canvas_info(target) else {
        ::log::error!(target: "STONE", "stone_native_frame_set_world_image: target {target} is not live");
        return false;
    };
    // R32I cannot be sampled as colour by post processing.
    if format == 3 || w < info.active_width || h < info.active_height {
        ::log::error!(
            target: "STONE",
            "stone_native_frame_set_world_image: target {target} ({w}x{h}, format {format}) cannot cover the {}x{} scene",
            info.active_width,
            info.active_height
        );
        return false;
    }
    WORLD_IMAGE.with(|wi| wi.set(Some(WorldImage { target, frame })));
    true
}

/// Supply this frame's world image from the embedding host that drives the
/// frame: `image` is registered through `stone_image_import_gl_texture` and its
/// size is the plan's active size. Call between `frame_update` and
/// `frame_draw`; it applies to that frame only. `flags` are as for
/// `stone_native_frame_set_world_image`. A `WORLD_BEGIN` hook still runs and may
/// draw over the image or replace it. Returns false with a logged error for an
/// invalid image or flags; an image whose size does not match the scene logs
/// at `WORLD_BEGIN`. Either way the frame renders through the engine path.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_frame_set_host_world_image(image: u64, flags: u32) -> bool {
    HOST_WORLD_IMAGE.with(|w| w.set(None));
    if DISPATCHING.with(Cell::get).is_some() {
        ::log::error!(target: "STONE", "stone_frame_set_host_world_image: called from a frame hook; use stone_native_frame_set_world_image");
        return false;
    }
    if flags != STONE_WORLD_IMAGE_KEEP_POST {
        ::log::error!(target: "STONE", "stone_frame_set_host_world_image: unsupported flags 0x{flags:X} (only KEEP_POST)");
        return false;
    }
    if !crate::image_texture::is_imported(image) {
        ::log::error!(target: "STONE", "stone_frame_set_host_world_image: image {image} is not an imported GL texture");
        return false;
    }
    let frame = crate::stream_buffer::frame();
    HOST_WORLD_IMAGE.with(|w| {
        w.set(Some(WorldImage {
            target: image,
            frame,
        }))
    });
    true
}

fn copy_host_image(image: u64, scene: u64, w: c_int, h: c_int) -> bool {
    let size = crate::image_texture::image_info(image).map(|(_, iw, ih, _, _)| (iw, ih));
    let texture = crate::image_texture::sample_image_texture(image)
        .and_then(std::num::NonZeroU32::new)
        .map(glow::NativeTexture);
    let (Some((iw, ih)), Some(texture)) = (size, texture) else {
        ::log::error!(target: "STONE", "host world image {image} is no longer live");
        return false;
    };
    if (iw, ih) != (w, h) {
        ::log::error!(target: "STONE", "host world image {image} is {iw}x{ih}, not the {w}x{h} scene");
        return false;
    }
    let scope = stone_native_stage_begin();
    let source = with_gl(|gl| unsafe {
        let generation = crate::handles::context_generation();
        let fbo = match HOST_IMAGE_FBO.with(Cell::get) {
            Some((created_in, fbo)) if created_in == generation => fbo,
            _ => {
                let fbo = gl.create_framebuffer().ok()?;
                HOST_IMAGE_FBO.with(|f| f.set(Some((generation, fbo))));
                fbo
            }
        };
        gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(fbo));
        gl.framebuffer_texture_2d(
            glow::READ_FRAMEBUFFER,
            glow::COLOR_ATTACHMENT0,
            glow::TEXTURE_2D,
            Some(texture),
            0,
        );
        Some(fbo)
    })
    .flatten();
    let copied = source.is_some_and(|fbo| {
        let complete = with_gl(|gl| unsafe {
            gl.check_framebuffer_status(glow::READ_FRAMEBUFFER) == glow::FRAMEBUFFER_COMPLETE
        }) == Some(true);
        complete && copy_color(fbo, scene, w, h)
    });
    if let Some(fbo) = source {
        // Detached, so the host's texture is not kept alive between frames.
        with_gl(|gl| unsafe {
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(fbo));
            gl.framebuffer_texture_2d(
                glow::READ_FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                None,
                0,
            );
        });
    }
    stone_native_stage_end(scope);
    if !copied {
        ::log::error!(target: "STONE", "host world image {image}: copy into scene target {scene} failed");
    }
    copied
}

fn current_world_image() -> Option<u64> {
    let frame = crate::stream_buffer::frame();
    WORLD_IMAGE
        .with(Cell::get)
        .filter(|image| image.frame == frame && crate::canvas::canvas_generation(image.target) != 0)
        .map(|image| image.target)
}

/// Blit colour 0 of `src_fbo` over the active rect of `scene`. Only colour 0 of
/// the scene is written: a blit writes every draw buffer, and integer
/// attachments cannot take a float source.
fn copy_color(src_fbo: glow::Framebuffer, scene: u64, w: c_int, h: c_int) -> bool {
    let Some(dst_fbo) = crate::canvas::canvas_framebuffer(scene) else {
        return false;
    };
    let scope = stone_native_stage_begin();
    let copied = with_gl(|gl| unsafe {
        gl.disable(glow::SCISSOR_TEST);
        gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(src_fbo));
        gl.read_buffer(glow::COLOR_ATTACHMENT0);
        gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(dst_fbo));
        let mut saved = [glow::NONE; crate::gl_state::DRAW_BUFFERS];
        let max = (gl.get_parameter_i32(glow::MAX_DRAW_BUFFERS).max(1) as usize).min(saved.len());
        for (i, slot) in saved.iter_mut().enumerate().take(max) {
            *slot = gl.get_parameter_i32(glow::DRAW_BUFFER0 + i as u32) as u32;
        }
        let len = saved[..max]
            .iter()
            .rposition(|&b| b != glow::NONE)
            .map_or(1, |i| i + 1);
        gl.draw_buffers(&[glow::COLOR_ATTACHMENT0]);
        gl.blit_framebuffer(
            0,
            0,
            w,
            h,
            0,
            0,
            w,
            h,
            glow::COLOR_BUFFER_BIT,
            glow::NEAREST,
        );
        gl.draw_buffers(&saved[..len]);
        gl.get_error() == glow::NO_ERROR
    })
    .unwrap_or(false);
    stone_native_stage_end(scope);
    copied
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_info_layout_is_stable() {
        assert_eq!(std::mem::size_of::<StoneFrameInfo>(), 80);
        assert_eq!(std::mem::offset_of!(StoneFrameInfo, scene_target), 24);
        assert_eq!(
            std::mem::offset_of!(StoneFrameInfo, world_to_scene_offset_x),
            64
        );
    }
}
