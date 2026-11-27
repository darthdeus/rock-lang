//! Portable integration test for the native renderer C ABI using a second
//! compiled cdylib that never links `stone-rs`, reaching the engine solely
//! through `stone_native_renderer_api(1)`.

// SDL's Cocoa driver needs the process main thread.
fn main() {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let args: Vec<_> = std::env::args_os().skip(1).collect();
        if args.iter().any(|arg| arg == "--ignored") {
            return;
        }
        if args.iter().any(|arg| arg == "--list") {
            println!("native_renderer_reads_asset_bytes: test");
            println!("second_native_library_renders_multi_attachment_target_restores_state_and_invalidates_handles: test");
            return;
        }
        native_renderer_reads_asset_bytes();
        second_native_library_renders_multi_attachment_target_restores_state_and_invalidates_handles();
    }
}

use glow::HasContext;
use std::ffi::{c_char, c_float, c_int, c_void, CString};
use std::path::PathBuf;
use std::process::Command;

#[path = "support/host.rs"]
mod host;
use host::with_host;

const STONE_NATIVE_RENDERER_ABI_VERSION: u32 = 1;
const STONE_RESOURCE_TARGET: c_int = 1;
const STONE_RESOURCE_SHADER: c_int = 2;
const STONE_RESOURCE_TEXTURE: c_int = 3;
const STONE_RESOURCE_MESH: c_int = 4;

const STONE_STAGE_RESTORED: u32 = 1 << 0;
const STONE_STAGE_TARGET_CHANGED: u32 = 1 << 1;
const STONE_STAGE_SHADER_CHANGED: u32 = 1 << 2;
const STONE_STAGE_DRAW_BUFFERS_CHANGED: u32 = 1 << 3;
const STONE_STAGE_UNWOUND: u32 = 1 << 4;
const STONE_STAGE_GL_ERROR: u32 = 1 << 5;

const STONE_FRAME_HOOK_WORLD_BEGIN: u32 = 1;
const STONE_FRAME_HOOK_WORLD_BATCHES: u32 = 2;
const STONE_FRAME_HOOKS_WROTE: u32 = 1 << 8;
const STONE_FRAME_HOOKS_WORLD_IMAGE: u32 = 1 << 9;

const STONE_CANVAS_RGBA8: c_int = 0;

extern "C" {
    fn stone_native_renderer_api(abi_version: u32) -> *const c_void;
    fn stone_native_stage_begin() -> u32;
    fn stone_native_stage_end(scope: u32) -> u32;
    fn stone_native_frame_active() -> bool;
    fn stone_native_frame_target() -> u64;
    fn stone_native_frame_size(width: *mut c_int, height: *mut c_int) -> bool;
    fn stone_native_context_generation() -> u64;
    fn stone_native_handle_valid(kind: c_int, handle: u64) -> bool;
    fn stone_native_handle_generation(kind: c_int, handle: u64) -> u64;
    fn stone_native_find_shader(name: *const c_char) -> u64;
    fn stone_shader_register_name(handle: u64, name: *const c_char) -> bool;
    fn stone_shader_register_block_member(
        handle: u64,
        binding: c_int,
        name: *const c_char,
        offset: c_int,
        kind: c_int,
        capacity: c_int,
        stride: c_int,
    ) -> bool;
    fn stone_target_create(
        width: c_int,
        height: c_int,
        formats: *const c_int,
        count: c_int,
        with_depth: bool,
    ) -> u64;
    fn stone_target_resize(handle: u64, width: c_int, height: c_int) -> bool;
    fn stone_native_target_info(
        handle: u64,
        width: *mut c_int,
        height: *mut c_int,
        format: *mut c_int,
        color_attachments: *mut c_int,
        has_depth: *mut bool,
    ) -> u64;
    fn stone_canvas_resize(handle: u64, width: c_int, height: c_int) -> bool;
    fn stone_canvas_attach_color_target(color: u64, attachment: u64, slot: c_int) -> bool;
    fn stone_target_destroy(handle: u64);
    fn stone_target_attachment(handle: u64, slot: c_int) -> u64;

    fn stone_create_shader_from_combined(src: *const c_char) -> u64;
    fn stone_destroy_shader(handle: u64);
    fn stone_shader_bind_uniform_block(
        handle: u64,
        shader_key: *const c_char,
        block_name: *const c_char,
        binding: c_int,
        size: c_int,
        fingerprint: u64,
    ) -> bool;
    fn stone_shader_set_texture(h: u64, n: *const c_char, unit: c_int);
    fn stone_shader_texture_slot_find(handle: u64, name: *const c_char) -> c_int;
    fn stone_set_shader(handle: u64);
    fn stone_create_canvas(width: c_int, height: c_int, format: c_int, with_depth: bool) -> u64;
    fn stone_set_canvas(handle: u64);
    fn stone_destroy_canvas(handle: u64);
    fn stone_get_live_canvas_count() -> c_int;
    fn stone_get_live_canvas_bytes() -> u64;
    fn stone_set_canvas_active(handle: u64, width: c_int, height: c_int);
    fn stone_read_canvas_pixel_channel(handle: u64, x: c_int, y: c_int, channel: c_int) -> f32;
    fn stone_current_shader_bound() -> bool;
    fn stone_set_draw_buffer_mask(mask: c_int);
    fn stone_clear_canvas(r: c_float, g: c_float, b: c_float, a: c_float);
    fn stone_set_blend_mode_i(mode: c_int);
    fn stone_set_depth_mode(compare: *const c_char, write: bool);
    fn stone_set_color_mask(r: bool, g: bool, b: bool, a: bool);
    fn stone_set_scissor(enabled: bool, x: c_int, y: c_int, w: c_int, h: c_int);
    fn stone_bind_sampler(unit: c_int, mode: c_int, image: u64);
    fn stone_bind_canvas_texture_to_unit(handle: u64, unit: c_int) -> bool;
    fn stone_draw_fullscreen_rect(u0: c_float, v0: c_float, u1: c_float, v1: c_float);
    fn stone_canvas_read_region_rgba32f(
        handle: u64,
        x: c_int,
        y: c_int,
        w: c_int,
        h: c_int,
        out: *mut f32,
        capacity: c_int,
    ) -> c_int;
    fn stone_canvas_read_region_i32(
        handle: u64,
        x: c_int,
        y: c_int,
        w: c_int,
        h: c_int,
        out: *mut i32,
        capacity: c_int,
    ) -> c_int;
    fn stone_get_white_image() -> u64;
    fn stone_create_image_data(width: c_int, height: c_int) -> u64;
    fn stone_new_image_from_data(data_handle: u64, filter_mode: c_int) -> u64;
    fn stone_image_update_from_data(image_handle: u64, data_handle: u64) -> bool;
    fn stone_unload_image(handle: u64);
    fn stone_load_image(
        path: *const c_char,
        filter_mode: c_int,
        flags: c_int,
        width: c_int,
        height: c_int,
    ) -> u64;
    fn stone_reload_image(handle: u64, path: *const c_char) -> bool;
    fn stone_native_find_texture(path: *const c_char) -> u64;
    fn stone_destroy_image_data(handle: u64);
    fn stone_mesh_create(
        mode: c_int,
        verts: *const c_float,
        vert_count: c_int,
        stride_floats: c_int,
        attr_sizes: *const c_int,
        attr_count: c_int,
    ) -> u64;
    fn stone_mesh_free(handle: u64);
    fn stone_get_canvas_texture(handle: u64) -> u32;
    fn stone_native_frame_hooks_run(
        point: u32,
        scene: u64,
        partner: u64,
        offset_x: c_float,
        offset_y: c_float,
        scale: c_float,
    ) -> u32;
}

/// Mirrors `HookProbe` in the second library.
#[repr(C)]
struct HookProbe {
    api: *const c_void,
    tag: u32,
    mode: u32,
    world_image: u64,
    scene: u64,
}

struct ProbeLib {
    hook: *const c_void,
    release: *const c_void,
    take_order: unsafe extern "C" fn(*mut u32, u32) -> u32,
    released: unsafe extern "C" fn() -> u32,
    failures: unsafe extern "C" fn() -> u32,
}

impl ProbeLib {
    unsafe fn load(lib: *mut c_void) -> Self {
        let sym = |name: &std::ffi::CStr| {
            let ptr = libc::dlsym(lib, name.as_ptr());
            assert!(!ptr.is_null(), "dlsym {name:?}");
            ptr as *const c_void
        };
        Self {
            hook: sym(c"probe_frame_hook"),
            release: sym(c"probe_release"),
            take_order: std::mem::transmute::<
                *const c_void,
                unsafe extern "C" fn(*mut u32, u32) -> u32,
            >(sym(c"probe_take_order")),
            released: std::mem::transmute::<*const c_void, unsafe extern "C" fn() -> u32>(sym(
                c"probe_released",
            )),
            failures: std::mem::transmute::<*const c_void, unsafe extern "C" fn() -> u32>(sym(
                c"probe_failures",
            )),
        }
    }

    unsafe fn order(&self) -> Vec<u32> {
        let mut out = [0u32; 16];
        let len = (self.take_order)(out.as_mut_ptr(), 16);
        out[..len as usize].to_vec()
    }
}

const MRT_SHADER: &str = r#"#version 330 core
#ifdef VERTEX
layout(location = 0) in vec2 aPos;
void main() {
    gl_Position = vec4(aPos, 0.0, 1.0);
}
#endif
#ifdef PIXEL
layout(std140) uniform ProbeFragU {
    vec4 uColor;
    vec4 uAux;
    int uTag;
};
uniform sampler2D uTexture;
uniform sampler2D uAuxTex;
layout(location = 0) out vec4 oColor;
layout(location = 1) out int oId;
layout(location = 2) out vec4 oAux;
void main() {
    vec4 t = texture(uTexture, vec2(0.5)) * texture(uAuxTex, vec2(0.5));
    oColor = uColor * t;
    oId = uTag;
    oAux = uAux;
}
#endif
"#;

const BLIT_SHADER: &str = r#"#version 330 core
#ifdef VERTEX
layout(location = 0) in vec2 aPos;
layout(location = 1) in vec2 aUv;
out vec2 vUv;
void main() {
    vUv = aUv;
    gl_Position = vec4(aPos, 0.0, 1.0);
}
#endif
#ifdef PIXEL
uniform sampler2D uTexture;
in vec2 vUv;
out vec4 oColor;
void main() {
    oColor = texture(uTexture, vUv);
}
#endif
"#;

const SECOND_LIB_SRC: &str = r#"
use std::ffi::{c_char, c_float, c_int, c_void};

const ABI_VERSION: u32 = 1;
const RES_SHADER: c_int = 2;
const RES_TEXTURE: c_int = 3;
const RES_MESH: c_int = 4;

#[repr(C)]
struct ApiV1 {
    abi_version: u32,
    struct_size: u32,

    stage_begin: extern "C" fn() -> u32,
    stage_end: extern "C" fn(u32) -> u32,
    frame_active: extern "C" fn() -> bool,
    frame_target: extern "C" fn() -> u64,
    frame_size: extern "C" fn(*mut c_int, *mut c_int) -> bool,

    context_generation: extern "C" fn() -> u64,
    handle_valid: extern "C" fn(c_int, u64) -> bool,
    handle_generation: extern "C" fn(c_int, u64) -> u64,
    find_shader: extern "C" fn(*const c_char) -> u64,
    find_texture: extern "C" fn(*const c_char) -> u64,
    target_info: extern "C" fn(u64, *mut c_int, *mut c_int, *mut c_int, *mut c_int, *mut bool) -> u64,
    shader_info: extern "C" fn(u64, *mut c_int, *mut c_int) -> u64,
    texture_info: extern "C" fn(u64, *mut c_int, *mut c_int, *mut c_int, *mut bool) -> u64,
    mesh_info: extern "C" fn(u64, *mut c_int, *mut c_int, *mut c_int) -> u64,

    target_create: extern "C" fn(c_int, c_int, *const c_int, c_int, bool) -> u64,
    target_resize: extern "C" fn(u64, c_int, c_int) -> bool,
    target_destroy: extern "C" fn(u64),
    target_attachment: extern "C" fn(u64, c_int) -> u64,
    set_canvas: extern "C" fn(u64),
    set_canvas_active: extern "C" fn(u64, c_int, c_int),
    clear_canvas: extern "C" fn(c_float, c_float, c_float, c_float),
    clear_canvas_int: extern "C" fn(c_int, c_int),
    clear_depth: extern "C" fn(c_float),

    shader_uniform_block_count: extern "C" fn(u64) -> c_int,
    shader_uniform_block_find_by_binding: extern "C" fn(u64, c_int) -> c_int,
    shader_uniform_block_find_by_name: extern "C" fn(u64, *const c_char) -> c_int,
    shader_uniform_block_info: extern "C" fn(u64, c_int, *mut c_int, *mut c_int, *mut c_int, *mut u8, c_int) -> bool,
    shader_uniform_block_fingerprint: extern "C" fn(u64, c_int) -> u64,
    shader_uniform_block_member_info: extern "C" fn(u64, c_int, c_int, *mut c_int, *mut c_int, *mut c_int, *mut c_int) -> bool,
    shader_uniform_block_member_name: extern "C" fn(u64, c_int, c_int, *mut u8, c_int) -> c_int,
    shader_uniform_block_member_offset: extern "C" fn(u64, c_int, *const c_char) -> c_int,
    shader_texture_slot_count: extern "C" fn(u64) -> c_int,
    shader_texture_slot_find: extern "C" fn(u64, *const c_char) -> c_int,
    shader_texture_slot_info: extern "C" fn(u64, c_int, *mut c_int, *mut u8, c_int) -> c_int,

    set_shader: extern "C" fn(u64),
    upload_uniform_block: extern "C" fn(u64, *const u8, c_int, u64) -> bool,
    get_white_image: extern "C" fn() -> u64,
    bind_image_texture_to_unit: extern "C" fn(u64, c_int) -> bool,
    bind_canvas_texture_to_unit: extern "C" fn(u64, c_int) -> bool,
    bind_canvas_depth_to_unit: extern "C" fn(u64, c_int) -> bool,
    bind_sampler: extern "C" fn(c_int, c_int, u64),
    set_depth_mode: extern "C" fn(*const c_char, bool),
    set_blend_mode_i: extern "C" fn(c_int),
    set_draw_buffer_mask: extern "C" fn(c_int),
    set_color_mask: extern "C" fn(bool, bool, bool, bool),
    set_scissor: extern "C" fn(bool, c_int, c_int, c_int, c_int),
    set_gl_viewport: extern "C" fn(c_int, c_int, c_int, c_int),
    draw_mesh: extern "C" fn(*const c_float, c_int, c_int, *const c_int, c_int, c_int, u64) -> c_int,
    draw_fullscreen_quad: extern "C" fn(),
    draw_fullscreen_rect: extern "C" fn(c_float, c_float, c_float, c_float),

    mesh_create: extern "C" fn(c_int, *const c_float, c_int, c_int, *const c_int, c_int) -> u64,
    mesh_set_instances: extern "C" fn(u64, *const c_float, c_int, c_int, *const c_int, c_int) -> c_int,
    mesh_update_instances: extern "C" fn(u64, *const c_float, c_int) -> c_int,
    mesh_draw: extern "C" fn(u64, u64, c_int) -> c_int,
    mesh_free: extern "C" fn(u64),
    target_color_view: extern "C" fn(u64) -> u64,
    get_live_mesh_bytes: extern "C" fn() -> u64,

    gl_get_proc_address: extern "C" fn(*const c_char) -> *const c_void,
    image_gl_texture: extern "C" fn(u64) -> u32,
    target_gl_texture: extern "C" fn(u64, c_int) -> u32,
    target_gl_depth_texture: extern "C" fn(u64) -> u32,
    target_gl_framebuffer: extern "C" fn(u64) -> u32,
    image_import_gl_texture: extern "C" fn(u32, c_int, c_int, u32) -> u64,
    image_import_update: extern "C" fn(u64, c_int, c_int) -> bool,
    image_release_import: extern "C" fn(u64) -> bool,
    frame_hook_register: extern "C" fn(u32, c_int, *const c_void, *const c_void, *mut c_void, *const c_char) -> u64,
    frame_hook_unregister: extern "C" fn(u64) -> bool,
    frame_set_world_image: extern "C" fn(u64, u32) -> bool,
}

#[repr(C)]
pub struct FrameInfo {
    struct_size: u32,
    point: u32,
    frame: u64,
    context_generation: u64,
    scene_target: u64,
    scene_partner: u64,
    scene_generation: u64,
    target_width: c_int,
    target_height: c_int,
    active_width: c_int,
    active_height: c_int,
    offset_x: c_float,
    offset_y: c_float,
    scale: c_float,
    _reserved: u32,
}

/// One registered hook. `mode`: 0 records only, 1 writes the scene target
/// with raw GL and leaves state dirty, 2 sets `world_image`, 3 sets it with
/// the reserved SKIP_POST flag.
#[repr(C)]
pub struct HookProbe {
    pub api: *const c_void,
    pub tag: u32,
    pub mode: u32,
    pub world_image: u64,
    pub scene: u64,
}

use std::sync::atomic::{AtomicU32, Ordering};
static ORDER: [AtomicU32; 16] = [const { AtomicU32::new(0) }; 16];
static ORDER_LEN: AtomicU32 = AtomicU32::new(0);
static RELEASED: AtomicU32 = AtomicU32::new(0);
static FAILURES: AtomicU32 = AtomicU32::new(0);

#[no_mangle]
pub extern "C" fn probe_take_order(out: *mut u32, cap: u32) -> u32 {
    let len = ORDER_LEN.swap(0, Ordering::SeqCst).min(cap).min(16);
    for i in 0..len as usize {
        unsafe { *out.add(i) = ORDER[i].load(Ordering::SeqCst) };
    }
    len
}

#[no_mangle]
pub extern "C" fn probe_released() -> u32 {
    RELEASED.load(Ordering::SeqCst)
}

#[no_mangle]
pub extern "C" fn probe_failures() -> u32 {
    FAILURES.load(Ordering::SeqCst)
}

#[no_mangle]
pub unsafe extern "C" fn probe_release(userdata: *mut c_void) {
    if !userdata.is_null() {
        RELEASED.fetch_add(1, Ordering::SeqCst);
    }
}

unsafe fn gl_fn<T>(api: &ApiV1, name: &std::ffi::CStr) -> Option<T> {
    let ptr = (api.gl_get_proc_address)(name.as_ptr());
    (!ptr.is_null()).then(|| std::mem::transmute_copy(&ptr))
}

#[no_mangle]
pub unsafe extern "C" fn probe_frame_hook(userdata: *mut c_void, info: *const FrameInfo) -> i32 {
    let probe = &*(userdata as *const HookProbe);
    let api = &*(probe.api.cast::<ApiV1>());
    let info = &*info;
    let slot = ORDER_LEN.fetch_add(1, Ordering::SeqCst) as usize;
    if slot < 16 {
        ORDER[slot].store(probe.tag, Ordering::SeqCst);
    }
    if info.struct_size as usize != std::mem::size_of::<FrameInfo>()
        || info.scene_target != probe.scene
        || info.context_generation != (api.context_generation)()
        || !(api.frame_active)()
        || (api.frame_target)() != probe.scene
    {
        FAILURES.fetch_add(1, Ordering::SeqCst);
        return -1;
    }
    match probe.mode {
        1 => {
            type Enable = extern "C" fn(u32);
            type PixelStore = extern "C" fn(u32, i32);
            type ClearColor = extern "C" fn(f32, f32, f32, f32);
            type GenBuffers = extern "C" fn(i32, *mut u32);
            type BindBuffer = extern "C" fn(u32, u32);
            type BindVertexArray = extern "C" fn(u32);
            type GetInt = extern "C" fn(u32, *mut i32);
            let (Some(enable), Some(pixel_store), Some(clear_color), Some(clear), Some(gen_buffers), Some(bind_buffer), Some(read_buffer), Some(get_int), Some(_bind_vao)) = (
                gl_fn::<Enable>(api, c"glEnable"),
                gl_fn::<PixelStore>(api, c"glPixelStorei"),
                gl_fn::<ClearColor>(api, c"glClearColor"),
                gl_fn::<Enable>(api, c"glClear"),
                gl_fn::<GenBuffers>(api, c"glGenBuffers"),
                gl_fn::<BindBuffer>(api, c"glBindBuffer"),
                gl_fn::<Enable>(api, c"glReadBuffer"),
                gl_fn::<GetInt>(api, c"glGetIntegerv"),
                gl_fn::<BindVertexArray>(api, c"glBindVertexArray"),
            ) else {
                FAILURES.fetch_add(1, Ordering::SeqCst);
                return -1;
            };
            // The stage bound vertex array 0 before the hook ran.
            let mut vao = -1;
            get_int(0x85B5, &mut vao);
            if vao != 0 {
                FAILURES.fetch_add(1, Ordering::SeqCst);
            }
            clear_color(0.0, 1.0, 0.0, 1.0);
            clear(0x4000);
            pixel_store(0x0CF5, 4); // UNPACK_ALIGNMENT
            pixel_store(0x0D02, 7); // PACK_ROW_LENGTH
            let mut buffer = 0;
            gen_buffers(1, &mut buffer);
            bind_buffer(0x88EC, buffer); // PIXEL_UNPACK_BUFFER
            enable(0x8DB9); // FRAMEBUFFER_SRGB
            // GL_NONE on the scene target, left changed: Stone's readbacks
            // select their own read buffer.
            read_buffer(0);
            enable(0xFFFF); // INVALID_ENUM, left pending
            1
        }
        2 => if (api.frame_set_world_image)(probe.world_image, 1) { 0 } else { -2 },
        3 => if (api.frame_set_world_image)(probe.world_image, 2) { -3 } else { 0 },
        _ => 0,
    }
}

#[no_mangle]
pub unsafe extern "C" fn probe_render_into_owned_target(
    api_ptr: *const c_void,
    shader_name: *const c_char,
    expected_entry_target: u64,
    expected_entry_w: c_int,
    expected_entry_h: c_int,
    mesh_handle: u64,
) -> u64 {
    if api_ptr.is_null() || shader_name.is_null() {
        return 0;
    }
    let api = &*(api_ptr.cast::<ApiV1>());
    if api.abi_version != ABI_VERSION || (api.struct_size as usize) < std::mem::size_of::<ApiV1>() {
        return 0;
    }
    if !(api.frame_active)() {
        return 0;
    }
    if (api.frame_target)() != expected_entry_target {
        return 0;
    }
    let (mut fw, mut fh) = (0, 0);
    if !(api.frame_size)(&mut fw, &mut fh) || fw != expected_entry_w || fh != expected_entry_h {
        return 0;
    }
    if (api.context_generation)() == 0 {
        return 0;
    }

    let shader = (api.find_shader)(shader_name);
    if shader == 0 || !(api.handle_valid)(RES_SHADER, shader) {
        return 0;
    }
    let white = (api.get_white_image)();
    if white == 0 || !(api.handle_valid)(RES_TEXTURE, white) {
        return 0;
    }
    if mesh_handle == 0 || !(api.handle_valid)(RES_MESH, mesh_handle) {
        return 0;
    }

    // Query uniform block layout and texture slots from the engine.
    let block_idx = (api.shader_uniform_block_find_by_binding)(shader, 2);
    if block_idx < 0 {
        return 0;
    }
    let (mut binding, mut size, mut members) = (-1, 0, 0);
    let mut name_buf = [0u8; 64];
    if !(api.shader_uniform_block_info)(
        shader,
        block_idx,
        &mut binding,
        &mut size,
        &mut members,
        name_buf.as_mut_ptr(),
        name_buf.len() as c_int,
    ) {
        return 0;
    }
    if binding != 2 || size < 36 || members != 3 {
        return 0;
    }
    let fp = (api.shader_uniform_block_fingerprint)(shader, block_idx);
    let off_color = (api.shader_uniform_block_member_offset)(shader, block_idx, c"uColor".as_ptr());
    let off_aux = (api.shader_uniform_block_member_offset)(shader, block_idx, c"uAux".as_ptr());
    let off_tag = (api.shader_uniform_block_member_offset)(shader, block_idx, c"uTag".as_ptr());
    if off_color != 0 || off_aux != 16 || off_tag != 32 {
        return 0;
    }
    let unit_tex = (api.shader_texture_slot_find)(shader, c"uTexture".as_ptr());
    let unit_aux = (api.shader_texture_slot_find)(shader, c"uAuxTex".as_ptr());
    if unit_tex != 0 || unit_aux != 1 {
        return 0;
    }

    // Create a 3-attachment native-owned target (RGBA8 + R32I + RGBA8 + depth).
    let formats: [c_int; 3] = [0, 3, 0];
    let target = (api.target_create)(32, 32, formats.as_ptr(), 3, true);
    if target == 0 {
        return 0;
    }
    if (api.target_attachment)(target, 0) != target
        || (api.target_attachment)(target, 1) == 0
        || (api.target_attachment)(target, 2) == 0
    {
        (api.target_destroy)(target);
        return 0;
    }

    // Intentionally mutate target, shader, UBO, samplers, blend, depth, draw buffers, scissor, color mask
    // and return WITHOUT restoring them; the engine's stage_end must restore all of them.
    (api.set_canvas)(target);
    (api.set_draw_buffer_mask)(0b101);
    (api.clear_canvas)(0.0, 0.0, 0.0, 1.0);
    (api.set_draw_buffer_mask)(0b111);
    (api.clear_canvas_int)(1, 0);
    (api.clear_depth)(1.0);
    (api.set_shader)(shader);

    let mut ubo = vec![0u8; size as usize];
    let write_f32 = |buf: &mut [u8], off: usize, v: f32| {
        buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
    };
    write_f32(&mut ubo, off_color as usize, 0.25);
    write_f32(&mut ubo, off_color as usize + 4, 0.75);
    write_f32(&mut ubo, off_color as usize + 8, 0.50);
    write_f32(&mut ubo, off_color as usize + 12, 1.0);
    write_f32(&mut ubo, off_aux as usize, 0.80);
    write_f32(&mut ubo, off_aux as usize + 4, 0.20);
    write_f32(&mut ubo, off_aux as usize + 8, 0.60);
    write_f32(&mut ubo, off_aux as usize + 12, 1.0);
    ubo[off_tag as usize..off_tag as usize + 4].copy_from_slice(&4242i32.to_le_bytes());

    if !(api.upload_uniform_block)(shader, ubo.as_ptr(), size, fp) {
        (api.target_destroy)(target);
        return 0;
    }

    (api.bind_image_texture_to_unit)(white, unit_tex);
    (api.bind_image_texture_to_unit)(white, unit_aux);
    (api.bind_sampler)(unit_tex, 0, white);
    (api.bind_sampler)(unit_aux, 1, white);
    (api.set_depth_mode)(c"lequal".as_ptr(), true);
    (api.set_blend_mode_i)(4); // replace
    (api.set_color_mask)(true, true, true, true);
    (api.set_scissor)(false, 0, 0, 16, 16);

    let quad: [c_float; 12] = [
        -1.0, -1.0,  1.0, -1.0,  1.0,  1.0,
        -1.0, -1.0,  1.0,  1.0, -1.0,  1.0,
    ];
    let attrs: [c_int; 1] = [2];
    let drawn = (api.draw_mesh)(quad.as_ptr(), 6, 2, attrs.as_ptr(), 1, 0, white);
    if drawn < 0 {
        (api.target_destroy)(target);
        return 0;
    }
    // Leave dirty color mask and viewport to verify stage_end restores them.
    (api.set_color_mask)(false, true, false, true);
    (api.set_gl_viewport)(2, 3, 10, 11);
    target
}
"#;

fn compile_second_library(dir: &std::path::Path) -> PathBuf {
    let src = dir.join("second_renderer.rs");
    std::fs::write(&src, SECOND_LIB_SRC).unwrap();
    let out = dir.join(format!(
        "{}second_renderer{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    ));
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let status = Command::new(rustc)
        .args(["--edition=2021", "--crate-type=cdylib"])
        .arg(&src)
        .arg("-o")
        .arg(&out)
        .status()
        .expect("invoke rustc for test cdylib");
    assert!(status.success(), "compiling test cdylib failed");
    out
}

unsafe fn bound_draw_fbo(gl: &glow::Context) -> i32 {
    gl.get_parameter_i32(glow::DRAW_FRAMEBUFFER_BINDING)
}

unsafe fn stage_scopes_unwind_reject_stale_tokens_and_clear_at_frame_start(gl: &glow::Context) {
    let host = stone_create_canvas(16, 16, STONE_CANVAS_RGBA8, false);
    let other = stone_create_canvas(8, 8, STONE_CANVAS_RGBA8, false);
    assert!(host != 0 && other != 0);
    stone_set_canvas(host);
    stone_set_blend_mode_i(0);
    let host_fbo = bound_draw_fbo(gl);
    let entry_src_rgb = gl.get_parameter_i32(glow::BLEND_SRC_RGB);

    // End without begin.
    assert_eq!(stone_native_stage_end(0), 0);
    assert_eq!(stone_native_stage_end(0x7fff_0001), 0);
    assert!(!stone_native_frame_active());

    // Nested scopes close in LIFO order.
    let outer = stone_native_stage_begin();
    let inner = stone_native_stage_begin();
    assert!(outer != 0 && inner != 0 && outer != inner);
    stone_set_canvas(other);
    let flags = stone_native_stage_end(inner);
    assert_ne!(flags & STONE_STAGE_RESTORED, 0);
    assert_eq!(flags & STONE_STAGE_UNWOUND, 0);
    assert!(stone_native_frame_active());
    assert_eq!(stone_native_frame_target(), host);
    let flags = stone_native_stage_end(outer);
    assert_ne!(flags & STONE_STAGE_RESTORED, 0);
    assert_eq!(flags & STONE_STAGE_UNWOUND, 0);
    assert!(!stone_native_frame_active());

    // Ending the outer scope with the inner one still open unwinds the inner
    // scope and restores the outer snapshot.
    let outer = stone_native_stage_begin();
    let inner = stone_native_stage_begin();
    stone_set_canvas(other);
    stone_set_blend_mode_i(4);
    let flags = stone_native_stage_end(outer);
    assert_ne!(flags & STONE_STAGE_RESTORED, 0);
    assert_ne!(flags & STONE_STAGE_UNWOUND, 0);
    assert_ne!(flags & STONE_STAGE_TARGET_CHANGED, 0);
    assert!(!stone_native_frame_active());
    assert_eq!(bound_draw_fbo(gl), host_fbo);
    assert_eq!(gl.get_parameter_i32(glow::BLEND_SRC_RGB), entry_src_rgb);
    assert_eq!(
        stone_native_stage_end(inner),
        0,
        "an unwound scope is closed"
    );

    // A stale token from a closed scope cannot close a later scope.
    let first = stone_native_stage_begin();
    assert_ne!(stone_native_stage_end(first) & STONE_STAGE_RESTORED, 0);
    let second = stone_native_stage_begin();
    stone_set_canvas(other);
    assert_eq!(stone_native_stage_end(first), 0);
    assert!(stone_native_frame_active());
    assert_eq!(stone_native_frame_target(), other);
    assert_ne!(stone_native_stage_end(second) & STONE_STAGE_RESTORED, 0);
    assert_eq!(bound_draw_fbo(gl), host_fbo);

    // A scope left open by an early return is dropped at the next frame start.
    let leaked = stone_native_stage_begin();
    assert!(stone_native_frame_active());
    stone_rs::stone_reset_frame_stats();
    assert!(!stone_native_frame_active());
    assert_eq!(stone_native_stage_end(leaked), 0);

    stone_set_canvas(0);
    stone_destroy_canvas(other);
    stone_destroy_canvas(host);
}

unsafe fn only_published_shaders_are_findable_and_a_failed_reload_keeps_the_live_one() {
    let src = CString::new(MRT_SHADER).unwrap();
    let live = stone_create_shader_from_combined(src.as_ptr());
    assert_ne!(live, 0);
    assert!(stone_shader_register_name(live, c"reload_probe".as_ptr()));

    // A reload binds the new program's blocks before it is published.
    let fresh = stone_create_shader_from_combined(src.as_ptr());
    assert_ne!(fresh, 0);
    assert!(stone_shader_bind_uniform_block(
        fresh,
        c"reload_probe".as_ptr(),
        c"ProbeFragU".as_ptr(),
        2,
        48,
        0x1234,
    ));
    assert_eq!(stone_native_find_shader(c"reload_probe".as_ptr()), live);
    // Its layout check fails, so the watcher destroys it unpublished.
    stone_destroy_shader(fresh);
    assert_eq!(stone_native_find_shader(c"reload_probe".as_ptr()), live);

    let unpublished = stone_create_shader_from_combined(src.as_ptr());
    assert!(stone_shader_bind_uniform_block(
        unpublished,
        c"never_published".as_ptr(),
        c"ProbeFragU".as_ptr(),
        2,
        48,
        0x1234,
    ));
    assert_eq!(stone_native_find_shader(c"never_published".as_ptr()), 0);

    stone_destroy_shader(unpublished);
    stone_destroy_shader(live);
    assert_eq!(stone_native_find_shader(c"reload_probe".as_ptr()), 0);
}

unsafe fn target_width(handle: u64) -> c_int {
    let (mut w, mut h, mut fmt, mut count, mut depth) = (0, 0, 0, 0, false);
    stone_native_target_info(handle, &mut w, &mut h, &mut fmt, &mut count, &mut depth);
    w
}

unsafe fn owned_attachments_change_only_through_their_owner() {
    let formats: [c_int; 2] = [0, 3];
    let target = stone_target_create(16, 16, formats.as_ptr(), 2, false);
    assert_ne!(target, 0);
    let child = stone_target_attachment(target, 1);
    assert_ne!(child, 0);
    let other = stone_create_canvas(16, 16, STONE_CANVAS_RGBA8, false);
    let target_gen = stone_native_handle_generation(STONE_RESOURCE_TARGET, target);
    let child_gen = stone_native_handle_generation(STONE_RESOURCE_TARGET, child);

    stone_destroy_canvas(child);
    stone_target_destroy(child);
    assert!(stone_native_handle_valid(STONE_RESOURCE_TARGET, child));
    assert!(!stone_canvas_resize(child, 20, 20));
    assert!(!stone_target_resize(child, 20, 20));
    assert_eq!(target_width(child), 16);
    assert!(!stone_canvas_attach_color_target(target, other, 1));
    assert_eq!(stone_target_attachment(target, 1), child);
    assert!(!stone_canvas_attach_color_target(other, child, 1));
    assert_eq!(stone_target_attachment(other, 1), 0);
    assert!(!stone_canvas_attach_color_target(child, other, 1));
    assert_eq!(stone_target_attachment(child, 1), 0);
    assert_eq!(
        stone_native_handle_generation(STONE_RESOURCE_TARGET, target),
        target_gen
    );
    assert_eq!(
        stone_native_handle_generation(STONE_RESOURCE_TARGET, child),
        child_gen
    );

    // A size the driver cannot hold is refused before storage is touched.
    assert!(!stone_target_resize(target, 1 << 20, 16));
    assert_eq!(
        stone_native_handle_generation(STONE_RESOURCE_TARGET, target),
        target_gen
    );

    assert!(stone_target_resize(target, 24, 24));
    assert_eq!(target_width(child), 24);
    stone_target_destroy(target);
    assert!(!stone_native_handle_valid(STONE_RESOURCE_TARGET, child));
    stone_destroy_canvas(other);
}

unsafe fn texture_on_unit(gl: &glow::Context, unit: u32) -> i32 {
    gl.active_texture(glow::TEXTURE0 + unit);
    let bound = gl.get_parameter_i32(glow::TEXTURE_BINDING_2D);
    gl.active_texture(glow::TEXTURE0);
    bound
}

unsafe fn stage_restores_every_texture_unit_a_shader_samples(gl: &glow::Context) {
    let src = CString::new(BLIT_SHADER).unwrap();
    let shader = stone_create_shader_from_combined(src.as_ptr());
    assert_ne!(shader, 0);
    stone_shader_set_texture(shader, c"uTexture".as_ptr(), 9);
    assert_eq!(
        stone_shader_texture_slot_find(shader, c"uTexture".as_ptr()),
        9
    );

    let script_texture = gl.create_texture().unwrap();
    let native_texture = gl.create_texture().unwrap();
    gl.active_texture(glow::TEXTURE9);
    gl.bind_texture(glow::TEXTURE_2D, Some(script_texture));
    gl.active_texture(glow::TEXTURE0);
    let entry = texture_on_unit(gl, 9);

    let scope = stone_native_stage_begin();
    gl.active_texture(glow::TEXTURE9);
    gl.bind_texture(glow::TEXTURE_2D, Some(native_texture));
    gl.active_texture(glow::TEXTURE0);
    assert_ne!(stone_native_stage_end(scope) & STONE_STAGE_RESTORED, 0);
    assert_eq!(texture_on_unit(gl, 9), entry);

    // Units past the saved range are refused where shaders register them.
    stone_shader_set_texture(shader, c"uTexture".as_ptr(), 40);
    assert_eq!(
        stone_shader_texture_slot_find(shader, c"uTexture".as_ptr()),
        9
    );

    gl.active_texture(glow::TEXTURE9);
    gl.bind_texture(glow::TEXTURE_2D, None);
    gl.active_texture(glow::TEXTURE0);
    gl.delete_texture(script_texture);
    gl.delete_texture(native_texture);
    stone_destroy_shader(shader);
}

unsafe fn texture_lookup_follows_every_live_load_of_a_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = |name: &str| {
        let file = dir.path().join(name);
        image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 0, 0, 255]))
            .save(&file)
            .unwrap();
        CString::new(file.to_str().unwrap()).unwrap()
    };
    let (first_path, second_path) = (path("first.png"), path("second.png"));

    let first = stone_load_image(first_path.as_ptr(), 0, 0, 0, 0);
    let again = stone_load_image(first_path.as_ptr(), 0, 0, 0, 0);
    assert!(first != 0 && again != 0 && first != again);
    stone_unload_image(again);
    assert_eq!(stone_native_find_texture(first_path.as_ptr()), first);

    assert!(stone_reload_image(first, second_path.as_ptr()));
    assert_eq!(stone_native_find_texture(second_path.as_ptr()), first);
    assert_eq!(stone_native_find_texture(first_path.as_ptr()), 0);

    stone_unload_image(first);
    assert_eq!(stone_native_find_texture(second_path.as_ptr()), 0);
}

/// State a native stage must give back that the probe changes.
#[derive(Debug, PartialEq)]
struct PipelineObservation {
    blend: [i32; 5],
    ubo_bindings: [(i32, i32, i32); 4],
    textures: [i32; 2],
    samplers: [i32; 2],
    vao: i32,
    clear_color: [u32; 4],
    clear_depth: u32,
}

unsafe fn observe_pipeline(gl: &glow::Context) -> PipelineObservation {
    let int = |param| gl.get_parameter_i32(param);
    let mut clear_color = [0.0f32; 4];
    gl.get_parameter_f32_slice(glow::COLOR_CLEAR_VALUE, &mut clear_color);
    let mut clear_depth = [0.0f32; 1];
    gl.get_parameter_f32_slice(glow::DEPTH_CLEAR_VALUE, &mut clear_depth);
    let mut ubo_bindings = [(0, 0, 0); 4];
    for (index, slot) in ubo_bindings.iter_mut().enumerate() {
        let index = index as u32;
        *slot = (
            gl.get_parameter_indexed_i32(glow::UNIFORM_BUFFER_BINDING, index),
            gl.get_parameter_indexed_i32(glow::UNIFORM_BUFFER_START, index),
            gl.get_parameter_indexed_i32(glow::UNIFORM_BUFFER_SIZE, index),
        );
    }
    let mut textures = [0; 2];
    let mut samplers = [0; 2];
    for unit in 0..2 {
        gl.active_texture(glow::TEXTURE0 + unit as u32);
        textures[unit] = int(glow::TEXTURE_BINDING_2D);
        samplers[unit] = int(glow::SAMPLER_BINDING);
    }
    gl.active_texture(glow::TEXTURE0);
    PipelineObservation {
        blend: [
            gl.is_enabled(glow::BLEND) as i32,
            int(glow::BLEND_SRC_RGB),
            int(glow::BLEND_DST_RGB),
            int(glow::BLEND_SRC_ALPHA),
            int(glow::BLEND_DST_ALPHA),
        ],
        ubo_bindings,
        textures,
        samplers,
        vao: int(glow::VERTEX_ARRAY_BINDING),
        clear_color: clear_color.map(f32::to_bits),
        clear_depth: clear_depth[0].to_bits(),
    }
}

unsafe fn target_create_rejects_bad_requests_without_leaking() {
    let live_before = stone_get_live_canvas_count();
    let rgba = [STONE_CANVAS_RGBA8; 9];
    assert_eq!(stone_target_create(8, 8, rgba.as_ptr(), 0, false), 0);
    assert_eq!(stone_target_create(8, 8, rgba.as_ptr(), 9, false), 0);
    assert_eq!(stone_target_create(8, 8, std::ptr::null(), 1, false), 0);
    assert_eq!(stone_target_create(0, 8, rgba.as_ptr(), 1, false), 0);
    assert_eq!(stone_target_create(8, -1, rgba.as_ptr(), 1, false), 0);
    for bad in [-1, 4] {
        let formats = [STONE_CANVAS_RGBA8, bad];
        assert_eq!(stone_target_create(8, 8, formats.as_ptr(), 2, false), 0);
    }
    // Larger than any GL implementation allows: canvas creation fails.
    assert_eq!(stone_target_create(1 << 20, 8, rgba.as_ptr(), 3, true), 0);
    assert_eq!(stone_get_live_canvas_count(), live_before);
}

fn second_native_library_renders_multi_attachment_target_restores_state_and_invalidates_handles() {
    let tmp = tempfile::tempdir().unwrap();
    let plugin_path = compile_second_library(tmp.path());

    type ProbeFn =
        unsafe extern "C" fn(*const c_void, *const c_char, u64, c_int, c_int, u64) -> u64;

    let c_path = CString::new(plugin_path.to_str().unwrap()).unwrap();
    let lib = unsafe { libc::dlopen(c_path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    assert!(!lib.is_null(), "dlopen second library failed");
    let sym = unsafe { libc::dlsym(lib, c"probe_render_into_owned_target".as_ptr()) };
    assert!(
        !sym.is_null(),
        "dlsym probe_render_into_owned_target failed"
    );
    let probe_fn: ProbeFn = unsafe { std::mem::transmute(sym) };

    with_host(|gl| unsafe {
        stage_scopes_unwind_reject_stale_tokens_and_clear_at_frame_start(gl);
        only_published_shaders_are_findable_and_a_failed_reload_keeps_the_live_one();
        owned_attachments_change_only_through_their_owner();
        stage_restores_every_texture_unit_a_shader_samples(gl);
        texture_lookup_follows_every_live_load_of_a_path();
        target_create_rejects_bad_requests_without_leaking();
        stage_binds_no_vertex_array_and_drains_gl_errors(gl);
        gl_names_follow_handles_and_imports_stay_native_owned(gl);
        let probe = ProbeLib::load(lib);
        let hook_data = frame_hooks_run_in_order_inside_stages_and_set_the_world_image(gl, &probe);

        assert!(stone_native_renderer_api(999).is_null());
        let api_ptr = stone_native_renderer_api(STONE_NATIVE_RENDERER_ABI_VERSION);
        assert!(!api_ptr.is_null());

        let ctx_gen = stone_native_context_generation();
        assert_ne!(ctx_gen, 0);
        assert!(!stone_native_frame_active());
        let (mut w, mut h) = (-1, -1);
        assert!(!stone_native_frame_size(&mut w, &mut h));

        let blit_src = CString::new(BLIT_SHADER).unwrap();
        let blit = stone_create_shader_from_combined(blit_src.as_ptr());
        assert_ne!(blit, 0);
        stone_shader_set_texture(blit, c"uTexture".as_ptr(), 0);
        color_view_samples_siblings_and_tracks_source_lifetime(gl, blit);

        let mrt_src = CString::new(MRT_SHADER).unwrap();
        let shader = stone_create_shader_from_combined(mrt_src.as_ptr());
        assert_ne!(shader, 0);
        assert!(stone_shader_register_name(
            shader,
            c"native_probe_shader".as_ptr()
        ));
        assert!(stone_shader_bind_uniform_block(
            shader,
            c"native_probe_shader".as_ptr(),
            c"ProbeFragU".as_ptr(),
            2,
            48,
            0xABCD_1234_5678_9EF0,
        ));
        assert!(stone_shader_register_block_member(
            shader,
            2,
            c"uColor".as_ptr(),
            0,
            3,
            0,
            0
        ));
        assert!(stone_shader_register_block_member(
            shader,
            2,
            c"uAux".as_ptr(),
            16,
            3,
            0,
            0
        ));
        assert!(stone_shader_register_block_member(
            shader,
            2,
            c"uTag".as_ptr(),
            32,
            4,
            0,
            0
        ));
        stone_shader_set_texture(shader, c"uTexture".as_ptr(), 0);
        stone_shader_set_texture(shader, c"uAuxTex".as_ptr(), 1);
        assert_eq!(
            stone_native_find_shader(c"native_probe_shader".as_ptr()),
            shader
        );

        let mesh_verts: [c_float; 6] = [0.0, 0.0, 1.0, 0.0, 0.0, 1.0];
        let mesh_attrs: [c_int; 1] = [2];
        let mesh = stone_mesh_create(1, mesh_verts.as_ptr(), 3, 2, mesh_attrs.as_ptr(), 1);
        assert_ne!(mesh, 0);

        let host_canvas = stone_create_canvas(40, 24, STONE_CANVAS_RGBA8, true);
        assert_ne!(host_canvas, 0);
        stone_set_canvas(host_canvas);
        stone_clear_canvas(0.1, 0.2, 0.3, 1.0);
        stone_set_shader(blit);
        stone_set_blend_mode_i(1); // additive
        stone_set_depth_mode(c"greater".as_ptr(), false);
        stone_set_color_mask(true, false, true, true);
        stone_set_scissor(true, 1, 2, 20, 12);
        gl.enable(glow::CULL_FACE);

        let entry_fbo = gl.get_parameter_i32(glow::DRAW_FRAMEBUFFER_BINDING);
        let entry_program = gl.get_parameter_i32(glow::CURRENT_PROGRAM);
        let mut entry_viewport = [0i32; 4];
        gl.get_parameter_i32_slice(glow::VIEWPORT, &mut entry_viewport);
        let mut entry_scissor = [0i32; 4];
        gl.get_parameter_i32_slice(glow::SCISSOR_BOX, &mut entry_scissor);
        gl.clear_depth_f32(0.5);
        let entry_texture_unit1 = gl.create_texture().unwrap();
        gl.active_texture(glow::TEXTURE1);
        gl.bind_texture(glow::TEXTURE_2D, Some(entry_texture_unit1));
        gl.active_texture(glow::TEXTURE0);
        let entry = observe_pipeline(gl);

        let scope = stone_native_stage_begin();
        assert_ne!(scope, 0);
        assert!(stone_native_frame_active());
        assert_eq!(stone_native_frame_target(), host_canvas);

        // Background thread must not see the render thread's frame scope.
        let off_thread_active = std::thread::spawn(|| stone_native_frame_active())
            .join()
            .unwrap();
        assert!(!off_thread_active);

        let native_target = probe_fn(
            api_ptr,
            c"native_probe_shader".as_ptr(),
            host_canvas,
            40,
            24,
            mesh,
        );
        assert_ne!(
            native_target, 0,
            "second native library failed to render into owned target"
        );

        let flags = stone_native_stage_end(scope);
        assert_ne!(flags & STONE_STAGE_RESTORED, 0);
        assert_ne!(flags & STONE_STAGE_TARGET_CHANGED, 0);
        assert_ne!(flags & STONE_STAGE_SHADER_CHANGED, 0);
        assert!(!stone_native_frame_active());

        // Verify full GL state restoration.
        assert_eq!(
            gl.get_parameter_i32(glow::DRAW_FRAMEBUFFER_BINDING),
            entry_fbo
        );
        assert_eq!(gl.get_parameter_i32(glow::CURRENT_PROGRAM), entry_program);
        let mut after_viewport = [0i32; 4];
        gl.get_parameter_i32_slice(glow::VIEWPORT, &mut after_viewport);
        assert_eq!(after_viewport, entry_viewport);
        let mut after_scissor = [0i32; 4];
        gl.get_parameter_i32_slice(glow::SCISSOR_BOX, &mut after_scissor);
        assert_eq!(after_scissor, entry_scissor);
        assert!(gl.is_enabled(glow::SCISSOR_TEST));
        assert!(gl.is_enabled(glow::CULL_FACE));
        assert_eq!(
            gl.get_parameter_bool_array::<4>(glow::COLOR_WRITEMASK),
            [true, false, true, true]
        );
        assert_eq!(gl.get_parameter_i32(glow::DEPTH_FUNC) as u32, glow::GREATER);
        assert!(!gl.get_parameter_bool(glow::DEPTH_WRITEMASK));
        assert_eq!(observe_pipeline(gl), entry);
        assert!(stone_current_shader_bound());
        let check = stone_native_stage_begin();
        assert_eq!(stone_native_frame_target(), host_canvas);
        let (mut cache_w, mut cache_h) = (0, 0);
        assert!(stone_native_frame_size(&mut cache_w, &mut cache_h));
        assert_eq!((cache_w, cache_h), (40, 24));
        assert_eq!(
            stone_native_stage_end(check),
            STONE_STAGE_RESTORED,
            "the stage left GL and Stone's caches in agreement"
        );

        // Draw buffers are per framebuffer: changing the host's is reported.
        let scope = stone_native_stage_begin();
        stone_set_draw_buffer_mask(0b11);
        let flags = stone_native_stage_end(scope);
        assert_ne!(flags & STONE_STAGE_DRAW_BUFFERS_CHANGED, 0);
        assert_eq!(
            gl.get_parameter_i32(glow::DRAW_BUFFER0) as u32,
            glow::COLOR_ATTACHMENT0
        );
        assert_eq!(gl.get_parameter_i32(glow::DRAW_BUFFER1) as u32, glow::NONE);
        gl.active_texture(glow::TEXTURE1);
        gl.bind_texture(glow::TEXTURE_2D, None);
        gl.active_texture(glow::TEXTURE0);
        gl.delete_texture(entry_texture_unit1);

        // Verify multi-attachment output in the native-owned target.
        let att_id = stone_target_attachment(native_target, 1);
        let att_aux = stone_target_attachment(native_target, 2);
        assert_ne!(att_id, 0);
        assert_ne!(att_aux, 0);

        let mut color_px = [0.0f32; 4];
        assert_eq!(
            stone_canvas_read_region_rgba32f(native_target, 16, 16, 1, 1, color_px.as_mut_ptr(), 4),
            4
        );
        assert!((color_px[0] - 0.25).abs() < 0.02);
        assert!((color_px[1] - 0.75).abs() < 0.02);
        assert!((color_px[2] - 0.50).abs() < 0.02);

        let mut id_px = [0i32; 1];
        assert_eq!(
            stone_canvas_read_region_i32(att_id, 16, 16, 1, 1, id_px.as_mut_ptr(), 1),
            1
        );
        assert_eq!(id_px[0], 4242);

        let mut aux_px = [0.0f32; 4];
        assert_eq!(
            stone_canvas_read_region_rgba32f(att_aux, 16, 16, 1, 1, aux_px.as_mut_ptr(), 4),
            4
        );
        assert!((aux_px[0] - 0.80).abs() < 0.02);
        assert!((aux_px[1] - 0.20).abs() < 0.02);
        assert!((aux_px[2] - 0.60).abs() < 0.02);

        // Draw the native-owned target's color texture onto host_canvas.
        gl.disable(glow::CULL_FACE);
        stone_set_color_mask(true, true, true, true);
        stone_set_scissor(false, 0, 0, 40, 24);
        stone_set_blend_mode_i(4);
        stone_set_depth_mode(c"always".as_ptr(), false);
        assert!(stone_bind_canvas_texture_to_unit(native_target, 0));
        stone_bind_sampler(0, 0, 0);
        stone_draw_fullscreen_rect(0.0, 0.0, 1.0, 1.0);
        stone_bind_sampler(0, -1, 0);

        let mut blit_px = [0.0f32; 4];
        assert_eq!(
            stone_canvas_read_region_rgba32f(host_canvas, 20, 12, 1, 1, blit_px.as_mut_ptr(), 4),
            4
        );
        assert!((blit_px[0] - 0.25).abs() < 0.02);
        assert!((blit_px[1] - 0.75).abs() < 0.02);
        assert!((blit_px[2] - 0.50).abs() < 0.02);

        // Resize native target and verify handle generations invalidate.
        let gen_target_before =
            stone_native_handle_generation(STONE_RESOURCE_TARGET, native_target);
        let gen_id_before = stone_native_handle_generation(STONE_RESOURCE_TARGET, att_id);
        assert_ne!(gen_target_before, 0);
        assert_ne!(gen_id_before, 0);
        assert!(stone_target_resize(native_target, 64, 48));
        let gen_target_after = stone_native_handle_generation(STONE_RESOURCE_TARGET, native_target);
        let gen_id_after = stone_native_handle_generation(STONE_RESOURCE_TARGET, att_id);
        assert_ne!(gen_target_after, 0);
        assert_ne!(gen_target_after, gen_target_before);
        assert_ne!(gen_id_after, 0);
        assert_ne!(gen_id_after, gen_id_before);

        // Texture reload / pixel update invalidates texture generation; unload invalidates handle.
        let img_data = stone_create_image_data(2, 2);
        assert_ne!(img_data, 0);
        let dyn_tex = stone_new_image_from_data(img_data, 0);
        assert_ne!(dyn_tex, 0);
        let gen_tex_before = stone_native_handle_generation(STONE_RESOURCE_TEXTURE, dyn_tex);
        assert_ne!(gen_tex_before, 0);
        assert!(stone_image_update_from_data(dyn_tex, img_data));
        let gen_tex_after = stone_native_handle_generation(STONE_RESOURCE_TEXTURE, dyn_tex);
        assert_ne!(gen_tex_after, 0);
        assert_ne!(gen_tex_after, gen_tex_before);
        stone_unload_image(dyn_tex);
        assert!(!stone_native_handle_valid(STONE_RESOURCE_TEXTURE, dyn_tex));
        assert_eq!(
            stone_native_handle_generation(STONE_RESOURCE_TEXTURE, dyn_tex),
            0
        );
        stone_destroy_image_data(img_data);

        // Shader reload replaces handle under the same name and invalidates the old handle.
        let reloaded_shader = stone_create_shader_from_combined(mrt_src.as_ptr());
        assert_ne!(reloaded_shader, 0);
        assert!(stone_shader_register_name(
            reloaded_shader,
            c"native_probe_shader".as_ptr()
        ));
        stone_destroy_shader(shader);
        assert!(!stone_native_handle_valid(STONE_RESOURCE_SHADER, shader));
        assert_eq!(
            stone_native_find_shader(c"native_probe_shader".as_ptr()),
            reloaded_shader
        );
        assert!(stone_native_handle_valid(
            STONE_RESOURCE_SHADER,
            reloaded_shader
        ));

        // Mesh free & target destroy invalidate handles (including owned attachments).
        stone_mesh_free(mesh);
        assert!(!stone_native_handle_valid(STONE_RESOURCE_MESH, mesh));
        assert_eq!(stone_native_handle_generation(STONE_RESOURCE_MESH, mesh), 0);

        stone_target_destroy(native_target);
        assert!(!stone_native_handle_valid(
            STONE_RESOURCE_TARGET,
            native_target
        ));
        assert!(!stone_native_handle_valid(STONE_RESOURCE_TARGET, att_id));
        assert!(!stone_native_handle_valid(STONE_RESOURCE_TARGET, att_aux));

        stone_destroy_shader(blit);
        teardown_invalidates_handles_in_the_next_context(host_canvas, reloaded_shader);
        assert_eq!(
            (probe.released)(),
            7,
            "teardown released the remaining hooks"
        );
        drop(hook_data);
    });

    unsafe { libc::dlclose(lib) };
}

unsafe fn color_view_samples_siblings_and_tracks_source_lifetime(gl: &glow::Context, blit: u64) {
    use stone_rs::native_renderer::StoneNativeRendererApiV1;

    let api = &*stone_native_renderer_api(1).cast::<StoneNativeRendererApiV1>();
    let formats = [STONE_CANVAS_RGBA8; 2];
    let target = stone_target_create(8, 8, formats.as_ptr(), 2, true);
    let sibling = stone_target_attachment(target, 1);
    assert_ne!(sibling, 0);
    stone_set_canvas_active(target, 4, 4);

    let bytes = stone_get_live_canvas_bytes();
    let count = stone_get_live_canvas_count();
    let view = (api.target_color_view)(target);
    assert_ne!(view, 0);
    assert_ne!(view, target);
    assert_eq!(stone_get_live_canvas_bytes(), bytes);
    assert_eq!(stone_get_live_canvas_count(), count + 1);
    assert_eq!((api.target_color_view)(0), 0);
    assert_eq!((api.target_color_view)(view), 0);
    assert_eq!(stone_target_attachment(view, 0), view);
    assert_eq!(stone_target_attachment(view, 1), 0);
    assert!(!stone_target_resize(view, 12, 12));
    assert!(!stone_canvas_attach_color_target(view, sibling, 1));
    assert!(!stone_canvas_attach_color_target(target, view, 1));

    let (mut w, mut h, mut format, mut outputs, mut depth) = (0, 0, -1, 0, true);
    assert_ne!(
        stone_native_target_info(view, &mut w, &mut h, &mut format, &mut outputs, &mut depth),
        0
    );
    assert_eq!(
        (w, h, format, outputs, depth),
        (4, 4, STONE_CANVAS_RGBA8, 1, false)
    );

    stone_set_canvas(sibling);
    stone_clear_canvas(0.75, 0.0, 0.0, 1.0);
    stone_set_canvas(view);
    let color = gl.get_framebuffer_attachment_parameter_i32(
        glow::DRAW_FRAMEBUFFER,
        glow::COLOR_ATTACHMENT1,
        glow::FRAMEBUFFER_ATTACHMENT_OBJECT_TYPE,
    );
    assert_eq!(color as u32, glow::NONE);
    stone_set_shader(blit);
    stone_set_blend_mode_i(4);
    assert!(stone_bind_canvas_texture_to_unit(sibling, 0));
    (api.draw_fullscreen_quad)();
    stone_set_shader(0);
    stone_set_canvas(0);
    assert!((stone_read_canvas_pixel_channel(target, 1, 2, 0) - 0.75).abs() < 0.01);

    let discarded = (api.target_color_view)(target);
    assert_ne!(discarded, 0);
    stone_target_destroy(discarded);
    assert!(stone_native_handle_valid(STONE_RESOURCE_TARGET, target));
    stone_set_canvas_active(target, 8, 8);
    assert!(!stone_native_handle_valid(STONE_RESOURCE_TARGET, view));
    assert_eq!(stone_read_canvas_pixel_channel(target, 6, 7, 0), 0.0);

    let child_view = (api.target_color_view)(sibling);
    assert_ne!(child_view, 0);
    assert!(stone_target_resize(target, 16, 16));
    assert!(!stone_native_handle_valid(
        STONE_RESOURCE_TARGET,
        child_view
    ));
    let child_view = (api.target_color_view)(sibling);
    assert_ne!(child_view, 0);
    stone_target_destroy(target);
    assert!(!stone_native_handle_valid(
        STONE_RESOURCE_TARGET,
        child_view
    ));
    assert_eq!(stone_get_live_canvas_count(), count - 2);
}

fn api() -> &'static stone_rs::native_renderer::StoneNativeRendererApiV1 {
    unsafe {
        &*stone_native_renderer_api(1).cast::<stone_rs::native_renderer::StoneNativeRendererApiV1>()
    }
}

fn native_renderer_reads_asset_bytes() {
    use std::ptr::{null, null_mut};
    use stone_rs::asset_registry::stone_register_asset;

    let dir = tempfile::tempdir().unwrap();
    let embedded = CString::new(dir.path().join("embedded.bin").to_str().unwrap()).unwrap();
    let filesystem_path = dir.path().join("filesystem.bin");
    let filesystem = CString::new(filesystem_path.to_str().unwrap()).unwrap();
    let missing = CString::new(dir.path().join("missing.bin").to_str().unwrap()).unwrap();
    let embedded_bytes = [0x81, 0, 0x23, 0x45];
    let filesystem_bytes = [0x67, 0x89, 0, 0xab];
    stone_register_asset(
        embedded.as_ptr(),
        embedded_bytes.as_ptr(),
        embedded_bytes.len(),
    );
    std::fs::write(filesystem_path, filesystem_bytes).unwrap();

    let read = api().asset_read;
    for (path, bytes) in [(&embedded, embedded_bytes), (&filesystem, filesystem_bytes)] {
        let size = bytes.len() as i64;
        assert_eq!(read(path.as_ptr(), null_mut(), 0), size, "{path:?}");

        let mut buffer = [0xff; 6];
        assert_eq!(read(path.as_ptr(), buffer.as_mut_ptr(), 2), size);
        assert_eq!(buffer, [bytes[0], bytes[1], 0xff, 0xff, 0xff, 0xff]);
        assert_eq!(read(path.as_ptr(), buffer.as_mut_ptr(), buffer.len()), size);
        assert_eq!(&buffer[..bytes.len()], &bytes);
        assert_eq!(&buffer[bytes.len()..], &[0xff; 2]);
    }

    assert_eq!(read(missing.as_ptr(), null_mut(), 0), -1);
    assert_eq!(read(null(), null_mut(), 0), -1);
    assert_eq!(read(c"\xff".as_ptr(), null_mut(), 0), -1);
}

unsafe fn stage_binds_no_vertex_array_and_drains_gl_errors(gl: &glow::Context) {
    let vao = gl.create_vertex_array().unwrap();
    gl.bind_vertex_array(Some(vao));
    // An error pending before the stage is drained, not attributed to it.
    gl.enable(0xFFFF);
    let scope = stone_native_stage_begin();
    assert_eq!(gl.get_parameter_i32(glow::VERTEX_ARRAY_BINDING), 0);
    assert_eq!(gl.get_error(), glow::NO_ERROR);
    let flags = stone_native_stage_end(scope);
    assert_ne!(flags & STONE_STAGE_RESTORED, 0);
    assert_eq!(flags & STONE_STAGE_GL_ERROR, 0);
    assert_eq!(
        gl.get_parameter_i32(glow::VERTEX_ARRAY_BINDING) as u32,
        vao.0.get()
    );

    let scope = stone_native_stage_begin();
    gl.enable(0xFFFF);
    assert_ne!(stone_native_stage_end(scope) & STONE_STAGE_GL_ERROR, 0);
    assert_eq!(gl.get_error(), glow::NO_ERROR);
    gl.bind_vertex_array(None);
    gl.delete_vertex_array(vao);
}

unsafe fn gl_names_follow_handles_and_imports_stay_native_owned(gl: &glow::Context) {
    let api = api();
    assert!(!(api.gl_get_proc_address)(c"glClear".as_ptr()).is_null());
    assert!((api.gl_get_proc_address)(std::ptr::null()).is_null());

    let formats = [STONE_CANVAS_RGBA8; 2];
    let target = stone_target_create(8, 8, formats.as_ptr(), 2, true);
    assert_ne!(target, 0);
    let color = (api.target_gl_texture)(target, 0);
    assert_ne!(color, 0);
    assert_eq!(color, stone_get_canvas_texture(target));
    let second = (api.target_gl_texture)(target, 1);
    assert!(second != 0 && second != color);
    assert_eq!((api.target_gl_texture)(target, 2), 0);
    assert_ne!((api.target_gl_depth_texture)(target), 0);
    stone_set_canvas(target);
    assert_eq!(
        (api.target_gl_framebuffer)(target) as i32,
        gl.get_parameter_i32(glow::DRAW_FRAMEBUFFER_BINDING)
    );
    stone_set_canvas(0);
    stone_target_destroy(target);
    assert_eq!((api.target_gl_texture)(target, 0), 0);
    assert_eq!((api.target_gl_framebuffer)(target), 0);
    let plain = stone_create_canvas(4, 4, STONE_CANVAS_RGBA8, false);
    assert_eq!((api.target_gl_depth_texture)(plain), 0);
    stone_destroy_canvas(plain);
    let white = stone_get_white_image();
    assert_ne!((api.image_gl_texture)(white), 0);

    let texture = gl.create_texture().unwrap();
    gl.bind_texture(glow::TEXTURE_2D, Some(texture));
    let texels = [255u8; 2 * 2 * 4];
    gl.tex_image_2d(
        glow::TEXTURE_2D,
        0,
        glow::RGBA8 as i32,
        2,
        2,
        0,
        glow::RGBA,
        glow::UNSIGNED_BYTE,
        glow::PixelUnpackData::Slice(Some(&texels)),
    );
    gl.bind_texture(glow::TEXTURE_2D, None);
    let name = texture.0.get();
    assert_eq!((api.image_import_gl_texture)(0, 2, 2, 0), 0);
    assert_eq!((api.image_import_gl_texture)(name, 2, 2, 0x80), 0);
    assert_eq!((api.image_import_gl_texture)(name, 0, 2, 0), 0);
    let image = (api.image_import_gl_texture)(name, 2, 2, 1);
    assert_ne!(image, 0);
    assert!(stone_native_handle_valid(STONE_RESOURCE_TEXTURE, image));
    assert_eq!((api.image_gl_texture)(image), name);
    let generation = stone_native_handle_generation(STONE_RESOURCE_TEXTURE, image);
    stone_unload_image(image);
    assert!(stone_native_handle_valid(STONE_RESOURCE_TEXTURE, image));
    assert!((api.image_import_update)(image, 4, 4));
    assert_ne!(
        stone_native_handle_generation(STONE_RESOURCE_TEXTURE, image),
        generation
    );
    assert!(!(api.image_import_update)(white, 4, 4));
    assert!(!(api.image_release_import)(white));
    assert!((api.image_release_import)(image));
    assert!(!stone_native_handle_valid(STONE_RESOURCE_TEXTURE, image));
    assert!(gl.is_texture(texture));
    gl.delete_texture(texture);

    let off_thread = std::thread::spawn(move || {
        let api = self::api();
        (
            (api.image_gl_texture)(white),
            (api.gl_get_proc_address)(c"glClear".as_ptr()).is_null(),
            (api.frame_hook_register)(
                1,
                0,
                std::ptr::dangling::<c_void>(),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null(),
            ),
        )
    })
    .join()
    .unwrap();
    assert_eq!(off_thread, (0, true, 0));
}

unsafe fn frame_hooks_run_in_order_inside_stages_and_set_the_world_image(
    gl: &glow::Context,
    probe: &ProbeLib,
) -> [Box<HookProbe>; 3] {
    let api = api();
    let api_ptr = stone_native_renderer_api(1);
    let scene = stone_create_canvas(16, 16, STONE_CANVAS_RGBA8, true);
    let partner = stone_create_canvas(16, 16, STONE_CANVAS_RGBA8, true);
    let ambient = stone_create_canvas(8, 8, STONE_CANVAS_RGBA8, false);
    let image = stone_create_canvas(16, 16, STONE_CANVAS_RGBA8, false);
    let small = stone_create_canvas(8, 8, STONE_CANVAS_RGBA8, false);
    stone_set_canvas(image);
    stone_clear_canvas(1.0, 0.0, 0.0, 1.0);
    stone_set_canvas(scene);
    stone_clear_canvas(0.0, 0.0, 0.0, 1.0);

    let probe_for = |tag, mode, world_image| {
        Box::new(HookProbe {
            api: api_ptr,
            tag,
            mode,
            world_image,
            scene,
        })
    };
    let register = |point, order, data: &HookProbe| {
        (api.frame_hook_register)(
            point,
            order,
            probe.hook,
            probe.release,
            data as *const HookProbe as *mut c_void,
            c"probe".as_ptr(),
        )
    };
    let writer = probe_for(1, 1, 0);
    let early = probe_for(2, 0, 0);
    let same_order = probe_for(3, 0, 0);
    let a = register(STONE_FRAME_HOOK_WORLD_BATCHES, 10, &writer);
    let b = register(STONE_FRAME_HOOK_WORLD_BATCHES, -5, &early);
    let c = register(STONE_FRAME_HOOK_WORLD_BATCHES, 10, &same_order);
    assert!(a != 0 && b != 0 && c != 0);
    assert_eq!(
        (api.frame_hook_register)(
            9,
            0,
            probe.hook,
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null()
        ),
        0
    );
    assert_eq!(
        (api.frame_hook_register)(
            1,
            0,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null()
        ),
        0
    );

    stone_set_canvas(ambient);
    let ambient_fbo = gl.get_parameter_i32(glow::DRAW_FRAMEBUFFER_BINDING);
    let read_buffer = gl.get_parameter_i32(glow::READ_BUFFER);
    let flags = stone_native_frame_hooks_run(
        STONE_FRAME_HOOK_WORLD_BATCHES,
        scene,
        partner,
        1.0,
        2.0,
        1.0,
    );
    assert_eq!(probe.order(), [2, 1, 3]);
    assert_eq!((probe.failures)(), 0);
    assert_ne!(flags & STONE_STAGE_RESTORED, 0);
    assert_ne!(flags & STONE_FRAME_HOOKS_WROTE, 0);
    assert_ne!(flags & STONE_STAGE_GL_ERROR, 0);
    assert_eq!(flags & STONE_FRAME_HOOKS_WORLD_IMAGE, 0);
    assert_eq!(gl.get_error(), glow::NO_ERROR);
    assert_eq!(
        gl.get_parameter_i32(glow::DRAW_FRAMEBUFFER_BINDING),
        ambient_fbo
    );
    assert_eq!(gl.get_parameter_i32(glow::READ_BUFFER), read_buffer);
    assert_eq!(gl.get_parameter_i32(glow::UNPACK_ALIGNMENT), 1);
    assert_eq!(gl.get_parameter_i32(glow::PACK_ROW_LENGTH), 0);
    assert_eq!(gl.get_parameter_i32(glow::PIXEL_UNPACK_BUFFER_BINDING), 0);
    assert!(!gl.is_enabled(glow::FRAMEBUFFER_SRGB));
    assert!((stone_read_canvas_pixel_channel(scene, 3, 3, 1) - 1.0).abs() < 0.01);

    assert!((api.frame_hook_unregister)(b));
    assert!(!(api.frame_hook_unregister)(b));
    assert_eq!((probe.released)(), 1);
    assert_eq!(
        stone_native_frame_hooks_run(STONE_FRAME_HOOK_WORLD_BEGIN, scene, partner, 0.0, 0.0, 1.0),
        0
    );
    probe.order();

    // Outside WORLD_BEGIN dispatch the world image is refused.
    assert!(!(api.frame_set_world_image)(image, 1));

    let supplies = probe_for(4, 2, image);
    let d = register(STONE_FRAME_HOOK_WORLD_BEGIN, 0, &supplies);
    let flags =
        stone_native_frame_hooks_run(STONE_FRAME_HOOK_WORLD_BEGIN, scene, partner, 0.0, 0.0, 1.0);
    assert_ne!(flags & STONE_FRAME_HOOKS_WORLD_IMAGE, 0);
    assert!((stone_read_canvas_pixel_channel(scene, 3, 3, 0) - 1.0).abs() < 0.01);
    assert!(stone_read_canvas_pixel_channel(scene, 3, 3, 1) < 0.01);
    assert!((api.frame_hook_unregister)(d));

    for (tag, mode, target) in [(5, 3, image), (6, 2, small), (7, 2, 0)] {
        let rejected = probe_for(tag, mode, target);
        let hook = register(STONE_FRAME_HOOK_WORLD_BEGIN, 0, &rejected);
        let flags = stone_native_frame_hooks_run(
            STONE_FRAME_HOOK_WORLD_BEGIN,
            scene,
            partner,
            0.0,
            0.0,
            1.0,
        );
        assert_eq!(flags & STONE_FRAME_HOOKS_WORLD_IMAGE, 0, "hook {tag}");
        assert!((api.frame_hook_unregister)(hook));
    }
    assert_eq!(probe.order(), [4, 5, 6, 7]);
    assert_eq!((probe.released)(), 5);

    stone_set_canvas(0);
    for canvas in [scene, partner, ambient, image, small] {
        stone_destroy_canvas(canvas);
    }
    // `writer` and `same_order` stay registered for teardown to release.
    [writer, early, same_order]
}

unsafe fn teardown_invalidates_handles_in_the_next_context(target: u64, shader: u64) {
    let make_mesh = || {
        let verts = [0.0, 0.0, 1.0, 0.0, 0.0, 1.0];
        stone_mesh_create(1, verts.as_ptr(), 3, 2, &2, 1)
    };
    let previous_context = stone_native_context_generation();
    assert_ne!(previous_context, 0);
    let previous_resources = [
        (STONE_RESOURCE_TARGET, target),
        (STONE_RESOURCE_SHADER, shader),
        (STONE_RESOURCE_TEXTURE, stone_get_white_image()),
        (STONE_RESOURCE_MESH, make_mesh()),
    ];
    for (kind, handle) in previous_resources {
        assert!(stone_native_handle_valid(kind, handle));
    }
    assert!(stone_rs::stone_teardown());
    assert_eq!(stone_native_context_generation(), 0);

    // The shared test host deliberately refuses to reopen after teardown.
    assert!(stone_rs::stone_init());
    assert!(
        !stone_rs::stone_create_window_ex(c"Stone".as_ptr(), 0, 0, 320, 240, 0, false, 0).is_null()
    );
    assert!(stone_native_context_generation() > previous_context);
    let src = CString::new(BLIT_SHADER).unwrap();
    let new_resources = [
        stone_create_canvas(8, 8, STONE_CANVAS_RGBA8, false),
        stone_create_shader_from_combined(src.as_ptr()),
        stone_get_white_image(),
        make_mesh(),
    ];
    for ((kind, stale), live) in previous_resources.into_iter().zip(new_resources) {
        assert!(stone_native_handle_valid(kind, live));
        assert!(!stone_native_handle_valid(kind, stale));
        assert_eq!(stone_native_handle_generation(kind, stale), 0);
    }
}
