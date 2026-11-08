//! On-demand state at the actual streamed mesh draw. No GL queries unless armed.
use crate::DebugGl;
use serde_json::{json, Value};
use std::cell::{Cell, RefCell};
use std::ffi::{c_char, c_void, CString};
use std::hash::{Hash, Hasher};

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static READ_DEPTH: Cell<bool> = const { Cell::new(false) };
    static DRAWS: RefCell<Vec<Value>> = const { RefCell::new(Vec::new()) };
    static RESULT: RefCell<Option<CString>> = const { RefCell::new(None) };
}

#[no_mangle]
pub extern "C" fn stone_mesh_capture_arm(read_depth: bool) {
    READ_DEPTH.with(|v| v.set(read_depth));
    DRAWS.with(|v| v.borrow_mut().clear());
    ARMED.with(|v| v.set(true));
}

#[no_mangle]
pub extern "C" fn stone_mesh_capture_finish_json() -> *const c_char {
    ARMED.with(|v| v.set(false));
    let json = DRAWS.with(|v| json!({"draws": *v.borrow()}).to_string());
    RESULT.with(|v| {
        let mut v = v.borrow_mut();
        *v = CString::new(json).ok();
        v.as_ref().map_or(std::ptr::null(), |s| s.as_ptr())
    })
}

pub(crate) fn armed() -> bool {
    ARMED.with(Cell::get)
}

fn digest(bytes: &[u8]) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    format!("{:016x}", h.finish())
}

// glow 0.14 does not expose vertex-attribute queries. Query the currently
// bound VAO directly through SDL's context-specific GL procedure lookup.
unsafe fn attributes(count: usize) -> Value {
    type GetInt = unsafe extern "system" fn(u32, u32, *mut i32);
    type GetPtr = unsafe extern "system" fn(u32, u32, *mut *mut c_void);
    let i = sdl2::sys::SDL_GL_GetProcAddress(c"glGetVertexAttribiv".as_ptr());
    let p = sdl2::sys::SDL_GL_GetProcAddress(c"glGetVertexAttribPointerv".as_ptr());
    if i.is_null() || p.is_null() {
        return Value::Null;
    }
    let get_i: GetInt = std::mem::transmute(i);
    let get_p: GetPtr = std::mem::transmute(p);
    let query = |index, name| {
        let mut value = 0;
        get_i(index, name, &mut value);
        value
    };
    (0..count)
        .map(|index| {
            let index = index as u32;
            let mut ptr: *mut c_void = std::ptr::null_mut();
            get_p(index, glow::VERTEX_ATTRIB_ARRAY_POINTER, &mut ptr);
            json!({"index": index, "enabled": query(index, glow::VERTEX_ATTRIB_ARRAY_ENABLED),
            "buffer": query(index, glow::VERTEX_ATTRIB_ARRAY_BUFFER_BINDING),
            "size": query(index, glow::VERTEX_ATTRIB_ARRAY_SIZE),
            "type": query(index, glow::VERTEX_ATTRIB_ARRAY_TYPE),
            "stride": query(index, glow::VERTEX_ATTRIB_ARRAY_STRIDE),
            "normalized": query(index, glow::VERTEX_ATTRIB_ARRAY_NORMALIZED),
            "offset": ptr as usize})
        })
        .collect()
}

pub(crate) unsafe fn record(
    gl: &DebugGl,
    data: &[u8],
    offset: i32,
    count: usize,
    stride: usize,
    attr_count: usize,
    index_data: Option<(&[u8], i32)>,
) {
    if !armed() {
        return;
    }
    let mut viewport = [0; 4];
    gl.get_parameter_i32_slice(glow::VIEWPORT, &mut viewport);
    let fbo = gl.get_parameter_i32(glow::DRAW_FRAMEBUFFER_BINDING);
    let proc = sdl2::sys::SDL_GL_GetProcAddress(c"glGetFramebufferAttachmentParameteriv".as_ptr());
    let attachment = |kind| {
        if fbo == 0 || proc.is_null() {
            return Value::Null;
        }
        let get: unsafe extern "system" fn(u32, u32, u32, *mut i32) = std::mem::transmute(proc);
        let query = |name| {
            let mut v = 0;
            get(glow::DRAW_FRAMEBUFFER, kind, name, &mut v);
            v
        };
        json!({"type": query(glow::FRAMEBUFFER_ATTACHMENT_OBJECT_TYPE),
            "name": query(glow::FRAMEBUFFER_ATTACHMENT_OBJECT_NAME)})
    };
    let depth_attachment = attachment(glow::DEPTH_ATTACHMENT);
    let pack_names = [
        glow::PACK_ALIGNMENT,
        glow::PACK_ROW_LENGTH,
        glow::PACK_SKIP_PIXELS,
        glow::PACK_SKIP_ROWS,
    ];
    let pack_before: Vec<i32> = pack_names
        .iter()
        .map(|&n| gl.get_parameter_i32(n))
        .collect();
    let read_fbo_before = gl.get_parameter_i32(glow::READ_FRAMEBUFFER_BINDING);
    let pack_buffer_before = gl.get_parameter_i32(glow::PIXEL_PACK_BUFFER_BINDING);
    let depth_digest = if READ_DEPTH.with(Cell::get)
        && depth_attachment["type"].as_i64().unwrap_or(0) != 0
        && viewport[2] > 0
        && viewport[3] > 0
        && (viewport[2] as usize).saturating_mul(viewport[3] as usize) <= 16_777_216
    {
        let old_read = gl.get_parameter_i32(glow::READ_FRAMEBUFFER_BINDING);
        let old_pack = gl.get_parameter_i32(glow::PIXEL_PACK_BUFFER_BINDING);
        let native_fbo = std::num::NonZeroU32::new(fbo as u32).map(glow::NativeFramebuffer);
        gl.bind_framebuffer(glow::READ_FRAMEBUFFER, native_fbo);
        gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
        // Read in bounded strips so a full-resolution diagnostic never allocates
        // an entire depth image. Preserve pack state because this is a probe.
        for (&n, v) in pack_names.iter().zip([1, 0, 0, 0]) {
            gl.pixel_store_i32(n, v);
        }
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        let mut y = 0;
        while y < viewport[3] {
            let rows = (viewport[3] - y).min(64);
            let mut bytes = vec![0; viewport[2] as usize * rows as usize * 4];
            gl.read_pixels(
                viewport[0],
                viewport[1] + y,
                viewport[2],
                rows,
                glow::DEPTH_COMPONENT,
                glow::FLOAT,
                glow::PixelPackData::Slice(Some(&mut bytes)),
            );
            hash.write(&bytes);
            y += rows;
        }
        for (&n, &v) in pack_names.iter().zip(pack_before.iter()) {
            gl.pixel_store_i32(n, v);
        }
        gl.bind_buffer(
            glow::PIXEL_PACK_BUFFER,
            std::num::NonZeroU32::new(old_pack as u32).map(glow::NativeBuffer),
        );
        gl.bind_framebuffer(
            glow::READ_FRAMEBUFFER,
            std::num::NonZeroU32::new(old_read as u32).map(glow::NativeFramebuffer),
        );
        Some(format!("{:016x}", hash.finish()))
    } else {
        None
    };
    let readback_state_restored = read_fbo_before
        == gl.get_parameter_i32(glow::READ_FRAMEBUFFER_BINDING)
        && pack_buffer_before == gl.get_parameter_i32(glow::PIXEL_PACK_BUFFER_BINDING)
        && pack_names
            .iter()
            .zip(pack_before.iter())
            .all(|(&n, &v)| gl.get_parameter_i32(n) == v)
        && fbo == gl.get_parameter_i32(glow::DRAW_FRAMEBUFFER_BINDING);
    let mut scissor = [0; 4];
    gl.get_parameter_i32_slice(glow::SCISSOR_BOX, &mut scissor);
    let mut depth_range = [0.0; 2];
    gl.get_parameter_f32_slice(glow::DEPTH_RANGE, &mut depth_range);
    let color_mask = gl.get_parameter_bool_array::<4>(glow::COLOR_WRITEMASK);
    let draw_buffers: Vec<i32> = (0..gl.get_parameter_i32(glow::MAX_DRAW_BUFFERS).clamp(0, 16))
        .map(|i| gl.get_parameter_i32(glow::DRAW_BUFFER0 + i as u32))
        .collect();
    let mut gpu = vec![0; data.len()];
    gl.get_buffer_sub_data(glow::ARRAY_BUFFER, offset, &mut gpu);
    let index_gpu = index_data.map(|(data, offset)| {
        let mut bytes = vec![0; data.len()];
        gl.get_buffer_sub_data(glow::ELEMENT_ARRAY_BUFFER, offset, &mut bytes);
        bytes
    });
    let sample: Vec<u32> = data
        .as_chunks::<4>()
        .0
        .iter()
        .take(44)
        .map(|b| u32::from_ne_bytes(*b))
        .collect();
    let value = json!({
        "framebuffer": fbo, "color_attachment": attachment(glow::COLOR_ATTACHMENT0),
        "depth_attachment": depth_attachment, "depth_digest": depth_digest, "viewport": viewport,
        "readback_state_restored": readback_state_restored,
        "draw_buffers": draw_buffers, "color_mask": color_mask,
        "scissor": {"enabled": gl.is_enabled(glow::SCISSOR_TEST), "box": scissor},
        "stencil_enabled": gl.is_enabled(glow::STENCIL_TEST),
        "cull": {"enabled": gl.is_enabled(glow::CULL_FACE), "mode": gl.get_parameter_i32(glow::CULL_FACE_MODE), "front_face": gl.get_parameter_i32(glow::FRONT_FACE)},
        "program": gl.get_parameter_i32(glow::CURRENT_PROGRAM),
        "blend": {"enabled": gl.is_enabled(glow::BLEND),
            "src_rgb": gl.get_parameter_i32(glow::BLEND_SRC_RGB), "dst_rgb": gl.get_parameter_i32(glow::BLEND_DST_RGB),
            "src_alpha": gl.get_parameter_i32(glow::BLEND_SRC_ALPHA), "dst_alpha": gl.get_parameter_i32(glow::BLEND_DST_ALPHA),
            "equation_rgb": gl.get_parameter_i32(glow::BLEND_EQUATION_RGB), "equation_alpha": gl.get_parameter_i32(glow::BLEND_EQUATION_ALPHA)},
        "depth": {"enabled": gl.is_enabled(glow::DEPTH_TEST), "function": gl.get_parameter_i32(glow::DEPTH_FUNC), "write": gl.get_parameter_bool(glow::DEPTH_WRITEMASK), "range": depth_range},
        "vertex_array": gl.get_parameter_i32(glow::VERTEX_ARRAY_BINDING),
        "array_buffer": gl.get_parameter_i32(glow::ARRAY_BUFFER_BINDING), "offset": offset,
        "vertex_count": count, "stride_bytes": stride, "attributes": attributes(attr_count),
        "cpu_digest": digest(data), "gpu_digest": digest(&gpu),
        "gpu_matches_cpu": gpu == data, "sample_f32_bits": sample,
        "index_cpu_digest": index_data.map(|(bytes, _)| digest(bytes)),
        "index_gpu_digest": index_gpu.as_ref().map(|bytes| digest(bytes)),
        "index_gpu_matches_cpu": index_data.zip(index_gpu.as_ref()).map(|((cpu, _), gpu)| cpu == gpu),
        "index_offset": index_data.map(|(_, offset)| offset),
        "index_count": index_data.map(|(bytes, _)| bytes.len() / 4),
        "element_buffer": gl.get_parameter_i32(glow::ELEMENT_ARRAY_BUFFER_BINDING),
    });
    DRAWS.with(|v| v.borrow_mut().push(value));
}
