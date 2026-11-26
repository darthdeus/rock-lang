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
            println!("second_native_library_renders_multi_attachment_target_restores_state_and_invalidates_handles: test");
            return;
        }
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
const STONE_STAGE_UNWOUND: u32 = 1 << 4;

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
    assert_eq!(
        stone_native_handle_generation(STONE_RESOURCE_TARGET, target),
        target_gen
    );
    assert_eq!(
        stone_native_handle_generation(STONE_RESOURCE_TARGET, child),
        child_gen
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

    let mut target_after_teardown = 0u64;
    let mut shader_after_teardown = 0u64;

    with_host(|gl| unsafe {
        stage_scopes_unwind_reject_stale_tokens_and_clear_at_frame_start(gl);
        only_published_shaders_are_findable_and_a_failed_reload_keeps_the_live_one();
        owned_attachments_change_only_through_their_owner();
        stage_restores_every_texture_unit_a_shader_samples(gl);
        texture_lookup_follows_every_live_load_of_a_path();

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

        // Keep host_canvas and reloaded_shader live across teardown to verify context-loss invalidation.
        let _ = stone_get_white_image();
        target_after_teardown = host_canvas;
        shader_after_teardown = reloaded_shader;
        stone_destroy_shader(blit);
    });

    // After with_host teardown, context generation is 0 and handles are invalid.
    unsafe {
        assert_eq!(stone_native_context_generation(), 0);
        assert!(!stone_native_handle_valid(
            STONE_RESOURCE_TARGET,
            target_after_teardown
        ));
        assert!(!stone_native_handle_valid(
            STONE_RESOURCE_SHADER,
            shader_after_teardown
        ));
        assert_eq!(
            stone_native_handle_generation(STONE_RESOURCE_TARGET, target_after_teardown),
            0
        );
        libc::dlclose(lib);
    }
}
