//! Bounded, on-demand GL execution evidence. No replay or continuous recorder.
use crate::DebugGl;
use serde_json::{json, Value};
use std::{
    cell::RefCell,
    collections::HashMap,
    ffi::{c_char, CStr, CString},
    path::PathBuf,
};

const MAX_OPERATIONS: usize = 4096;
const MAX_BYTES: usize = 64 * 1024 * 1024;
struct Capture {
    operations: Vec<Value>,
    omitted: usize,
    directory: PathBuf,
    snapshots: HashMap<u32, Value>,
    bytes: usize,
    frame: i32,
}
thread_local! {
    static CAPTURE: RefCell<Option<Capture>> = const { RefCell::new(None) };
    static RESULT: RefCell<Option<CString>> = const { RefCell::new(None) };
}

#[no_mangle]
pub unsafe extern "C" fn stone_graphics_capture_begin(directory: *const c_char, frame: i32) {
    let directory = if directory.is_null() {
        PathBuf::new()
    } else {
        PathBuf::from(CStr::from_ptr(directory).to_string_lossy().as_ref())
    };
    CAPTURE.with(|c| {
        *c.borrow_mut() = Some(Capture {
            operations: Vec::new(),
            omitted: 0,
            directory,
            snapshots: HashMap::new(),
            bytes: 0,
            frame,
        })
    });
    crate::with_gl(|gl| gl.set_capture(Some(record)));
}

#[no_mangle]
pub extern "C" fn stone_graphics_capture_cancel() {
    crate::with_gl(|gl| gl.set_capture(None));
    CAPTURE.with(|c| *c.borrow_mut() = None);
}

#[no_mangle]
pub extern "C" fn stone_graphics_capture_finish_json() -> *const c_char {
    crate::with_gl(|gl| gl.set_capture(None));
    let capture = CAPTURE.with(|c| c.borrow_mut().take());
    let value = crate::with_gl(|gl| unsafe {
        capture.map(|c| json!({
            "status": if c.snapshots.values().any(|s| s["status"] == "error") { "error" } else { "ok" },
            "texture_targets": "2D (Stone's sampled image/canvas target)",
            "operations": c.operations, "omitted_operations": c.omitted,
            "operation_limit": MAX_OPERATIONS, "snapshot_byte_limit": MAX_BYTES,
            "snapshots": c.snapshots.iter().map(|(texture, snapshot)| json!({"texture": texture, "snapshot": snapshot})).collect::<Vec<_>>(),
            "snapshot_bytes": c.bytes, "byte_order": if cfg!(target_endian = "little") { "little" } else { "big" },
            "snapshot_policy": "first sampled use of each canvas texture on the first two frames; RGBA float32 or signed int32, depth scalar float32; targets over 512 pixels per side sampled on an integer grid; native endian, GL bottom-left origin; source images are metadata only",
            "gpu": {"vendor": gl.get_parameter_string(glow::VENDOR),
                "renderer": gl.get_parameter_string(glow::RENDERER),
                "version": gl.get_parameter_string(glow::VERSION),
                "glsl": gl.get_parameter_string(glow::SHADING_LANGUAGE_VERSION)},
            "resources": crate::canvas::capture_resources(),
            "images": crate::image_texture::capture_resources(),
        })).unwrap_or(Value::Null)
    }).unwrap_or(Value::Null);
    RESULT.with(|r| {
        let mut r = r.borrow_mut();
        *r = Some(CString::new(value.to_string()).expect("JSON has no literal NUL"));
        r.as_ref().unwrap().as_ptr()
    })
}

#[no_mangle]
pub extern "C" fn stone_graphics_capture_scene_depth(handle: u64) {
    let Some(texture) = crate::canvas::capture_depth_texture(handle) else {
        return;
    };
    crate::with_gl(|gl| unsafe {
        CAPTURE.with(|capture| {
            let mut capture = capture.borrow_mut();
            let Some(c) = capture.as_mut() else { return };
            if c.snapshots.contains_key(&texture.0.get()) {
                return;
            }
            let old = gl.get_parameter_i32(glow::TEXTURE_BINDING_2D);
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            let width = gl.get_tex_level_parameter_i32(glow::TEXTURE_2D, 0, glow::TEXTURE_WIDTH);
            let height = gl.get_tex_level_parameter_i32(glow::TEXTURE_2D, 0, glow::TEXTURE_HEIGHT);
            let format =
                gl.get_tex_level_parameter_i32(glow::TEXTURE_2D, 0, glow::TEXTURE_INTERNAL_FORMAT);
            let value = snapshot(gl, c, texture.0.get(), width, height, format);
            c.snapshots.insert(texture.0.get(), value);
            gl.bind_texture(
                glow::TEXTURE_2D,
                std::num::NonZeroU32::new(old as u32).map(glow::NativeTexture),
            );
        });
    });
}

unsafe fn record(gl: &DebugGl, operation: &str, arguments: &[i64]) {
    CAPTURE.with(|capture| {
        let mut capture = capture.borrow_mut();
        let Some(c) = capture.as_mut() else { return };
        if c.operations.len() >= MAX_OPERATIONS { c.omitted += 1; return; }
        let active = gl.get_parameter_i32(glow::ACTIVE_TEXTURE);
        let pack = gl.get_parameter_i32(glow::PIXEL_PACK_BUFFER_BINDING);
        let pack_names = [glow::PACK_ALIGNMENT, glow::PACK_ROW_LENGTH, glow::PACK_SKIP_PIXELS, glow::PACK_SKIP_ROWS, glow::PACK_SWAP_BYTES, glow::PACK_LSB_FIRST];
        let pack_state: Vec<_> = pack_names.iter().map(|&n| gl.get_parameter_i32(n)).collect();
        let mut sampled = std::collections::HashSet::new();
        let program = gl.get_parameter_i32(glow::CURRENT_PROGRAM);
        if operation.starts_with("draw_") {
            if let Some(id) = std::num::NonZeroU32::new(program as u32) {
                let program = glow::NativeProgram(id);
                for index in 0..gl.get_active_uniforms(program) {
                    if let Some(uniform) = gl.get_active_uniform(program, index) {
                        if ![glow::SAMPLER_2D, glow::SAMPLER_2D_SHADOW, glow::INT_SAMPLER_2D, glow::UNSIGNED_INT_SAMPLER_2D].contains(&uniform.utype) { continue; }
                        for element in 0..uniform.size {
                            let name = if uniform.size > 1 { format!("{}[{element}]", uniform.name.trim_end_matches("[0]")) } else { uniform.name.clone() };
                            if let Some(location) = gl.get_uniform_location(program, &name) {
                                let mut unit = [0];
                                gl.get_uniform_i32(program, &location, &mut unit);
                                sampled.insert(unit[0]);
                            }
                        }
                    }
                }
            }
        }
        let mut textures = Vec::new();
        let units = gl.get_parameter_i32(glow::MAX_COMBINED_TEXTURE_IMAGE_UNITS);
        // Enumerate all units: vertex samplers need not occupy fragment slots.
        for unit in 0..units {
            gl.active_texture(glow::TEXTURE0 + unit as u32);
            let texture = gl.get_parameter_i32(glow::TEXTURE_BINDING_2D) as u32;
            if texture == 0 { continue; }
            let sampler = gl.get_parameter_indexed_i32(glow::SAMPLER_BINDING, unit as u32);
            let width = gl.get_tex_level_parameter_i32(glow::TEXTURE_2D, 0, glow::TEXTURE_WIDTH);
            let height = gl.get_tex_level_parameter_i32(glow::TEXTURE_2D, 0, glow::TEXTURE_HEIGHT);
            let format = gl.get_tex_level_parameter_i32(glow::TEXTURE_2D, 0, glow::TEXTURE_INTERNAL_FORMAT);
            let parameters = |name| {
                if let Some(id) = std::num::NonZeroU32::new(sampler as u32) {
                    gl.get_sampler_parameter_i32(glow::NativeSampler(id), name)
                } else { gl.get_tex_parameter_i32(glow::TEXTURE_2D, name) }
            };
            let snapshot = if sampled.contains(&unit) && crate::canvas::capture_texture_is_owned(texture) {
                if !c.snapshots.contains_key(&texture) {
                    let value = snapshot(gl, c, texture, width, height, format);
                    c.snapshots.insert(texture, value);
                }
                c.snapshots[&texture].clone()
            } else { json!({"status": "metadata_only"}) };
            textures.push(json!({"unit": unit, "sampled": sampled.contains(&unit), "texture": texture, "owner": crate::canvas::capture_texture_owner(texture), "sampler": sampler,
                "width": width, "height": height, "internal_format": format,
                "min_filter": parameters(glow::TEXTURE_MIN_FILTER), "mag_filter": parameters(glow::TEXTURE_MAG_FILTER),
                "wrap_s": parameters(glow::TEXTURE_WRAP_S), "wrap_t": parameters(glow::TEXTURE_WRAP_T),
                "compare_mode": parameters(glow::TEXTURE_COMPARE_MODE), "compare_func": parameters(glow::TEXTURE_COMPARE_FUNC),
                "snapshot": snapshot}));
        }
        for (&n, &v) in pack_names.iter().zip(&pack_state) { gl.pixel_store_i32(n, v); }
        gl.bind_buffer(glow::PIXEL_PACK_BUFFER, std::num::NonZeroU32::new(pack as u32).map(glow::NativeBuffer));
        gl.active_texture(active as u32);
        let restored = gl.get_parameter_i32(glow::ACTIVE_TEXTURE) == active
            && gl.get_parameter_i32(glow::PIXEL_PACK_BUFFER_BINDING) == pack
            && pack_names.iter().zip(&pack_state).all(|(&n, &v)| gl.get_parameter_i32(n) == v);
        let mut viewport = [0; 4];
        let mut scissor = [0; 4];
        gl.get_parameter_i32_slice(glow::VIEWPORT, &mut viewport);
        gl.get_parameter_i32_slice(glow::SCISSOR_BOX, &mut scissor);
        let framebuffer = gl.get_parameter_i32(glow::DRAW_FRAMEBUFFER_BINDING);
        let mut attachments = Vec::new();
        if framebuffer != 0 {
            for attachment in (0..gl.get_parameter_i32(glow::MAX_COLOR_ATTACHMENTS))
                .map(|i| glow::COLOR_ATTACHMENT0 + i as u32).chain([glow::DEPTH_ATTACHMENT, glow::STENCIL_ATTACHMENT]) {
                let kind = gl.get_framebuffer_attachment_parameter_i32(glow::DRAW_FRAMEBUFFER, attachment, glow::FRAMEBUFFER_ATTACHMENT_OBJECT_TYPE);
                if kind == glow::NONE as i32 { continue; }
                attachments.push(json!({"attachment": attachment, "type": kind,
                    "object": gl.get_framebuffer_attachment_parameter_i32(glow::DRAW_FRAMEBUFFER, attachment, glow::FRAMEBUFFER_ATTACHMENT_OBJECT_NAME)}));
            }
        }
        let draw_buffers: Vec<_> = (0..gl.get_parameter_i32(glow::MAX_DRAW_BUFFERS))
            .map(|i| gl.get_parameter_i32(glow::DRAW_BUFFER0 + i as u32)).collect();
        let shader = crate::SHADERS.with(|s| s.borrow().iter().find_map(|(handle, s)|
            (s.program.0.get() == program as u32).then_some(*handle)));
        let uniforms = shader.map(|handle| {
            let ptr = crate::uniform_block::stone_uniform_block_binding_state_json(handle);
            if ptr.is_null() { Value::Null } else {
                serde_json::from_slice(CStr::from_ptr(ptr).to_bytes()).unwrap_or(Value::Null)
            }
        });
        let mut clear_color = [0.0; 4];
        gl.get_parameter_f32_slice(glow::COLOR_CLEAR_VALUE, &mut clear_color);
        c.operations.push(json!({"operation": operation, "arguments": arguments,
            "program": program, "shader_handle": shader, "uniforms": uniforms, "clear_color": clear_color, "framebuffer": framebuffer,
            "read_framebuffer": gl.get_parameter_i32(glow::READ_FRAMEBUFFER_BINDING),
            "attachments": attachments, "draw_buffers": draw_buffers,
            "viewport": viewport, "scissor": {"enabled": gl.is_enabled(glow::SCISSOR_TEST), "box": scissor},
            "blend": {"enabled": gl.is_enabled(glow::BLEND),
                "src_rgb": gl.get_parameter_i32(glow::BLEND_SRC_RGB), "dst_rgb": gl.get_parameter_i32(glow::BLEND_DST_RGB),
                "src_alpha": gl.get_parameter_i32(glow::BLEND_SRC_ALPHA), "dst_alpha": gl.get_parameter_i32(glow::BLEND_DST_ALPHA),
                "equation_rgb": gl.get_parameter_i32(glow::BLEND_EQUATION_RGB), "equation_alpha": gl.get_parameter_i32(glow::BLEND_EQUATION_ALPHA)},
            "depth": {"enabled": gl.is_enabled(glow::DEPTH_TEST), "func": gl.get_parameter_i32(glow::DEPTH_FUNC),
                "write": gl.get_parameter_bool(glow::DEPTH_WRITEMASK)},
            "color_mask": gl.get_parameter_bool_array::<4>(glow::COLOR_WRITEMASK),
            "cull_enabled": gl.is_enabled(glow::CULL_FACE), "stencil_enabled": gl.is_enabled(glow::STENCIL_TEST),
            "textures": textures, "state_restored": restored}));
    });
}

unsafe fn snapshot(
    gl: &DebugGl,
    c: &mut Capture,
    texture: u32,
    width: i32,
    height: i32,
    format: i32,
) -> Value {
    if c.directory.as_os_str().is_empty() {
        return json!({"status": "json_only"});
    }
    let depth = [
        glow::DEPTH_COMPONENT16 as i32,
        glow::DEPTH_COMPONENT24 as i32,
        glow::DEPTH_COMPONENT32F as i32,
    ]
    .contains(&format);
    let channels = if depth { 1 } else { 4 };
    let read_bytes = (width.max(0) as usize)
        .saturating_mul(height.max(0) as usize)
        .saturating_mul(channels * 4);
    if read_bytes > 128 * 1024 * 1024 {
        return json!({"status": "readback_limit", "required_bytes": read_bytes});
    }
    let stride = ((width.max(height) + 511) / 512).max(1) as usize;
    let out_w = (width.max(0) as usize).div_ceil(stride);
    let out_h = (height.max(0) as usize).div_ceil(stride);
    let bytes = out_w * out_h * channels * 4;
    if bytes == 0 || bytes > MAX_BYTES.saturating_sub(c.bytes) {
        return json!({"status": "byte_limit", "required_bytes": bytes});
    }
    let integer = format == glow::R32I as i32;
    let mut data = vec![0u8; read_bytes];
    let pack_names = [
        glow::PACK_ALIGNMENT,
        glow::PACK_ROW_LENGTH,
        glow::PACK_SKIP_PIXELS,
        glow::PACK_SKIP_ROWS,
        glow::PACK_SWAP_BYTES,
        glow::PACK_LSB_FIRST,
    ];
    let pack_state: Vec<_> = pack_names
        .iter()
        .map(|&n| gl.get_parameter_i32(n))
        .collect();
    let pack = gl.get_parameter_i32(glow::PIXEL_PACK_BUFFER_BINDING);
    gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
    for (n, v) in [
        (glow::PACK_ALIGNMENT, 1),
        (glow::PACK_ROW_LENGTH, 0),
        (glow::PACK_SKIP_PIXELS, 0),
        (glow::PACK_SKIP_ROWS, 0),
        (glow::PACK_SWAP_BYTES, 0),
        (glow::PACK_LSB_FIRST, 0),
    ] {
        gl.pixel_store_i32(n, v);
    }
    gl.get_tex_image(
        glow::TEXTURE_2D,
        0,
        if depth {
            glow::DEPTH_COMPONENT
        } else if integer {
            glow::RGBA_INTEGER
        } else {
            glow::RGBA
        },
        if integer { glow::INT } else { glow::FLOAT },
        glow::PixelPackData::Slice(Some(&mut data)),
    );
    for (&n, &v) in pack_names.iter().zip(&pack_state) {
        gl.pixel_store_i32(n, v);
    }
    gl.bind_buffer(
        glow::PIXEL_PACK_BUFFER,
        std::num::NonZeroU32::new(pack as u32).map(glow::NativeBuffer),
    );
    if stride > 1 {
        let pixel = channels * 4;
        let mut sampled = Vec::with_capacity(bytes);
        for y in (0..height as usize).step_by(stride) {
            for x in (0..width as usize).step_by(stride) {
                let offset = (y * width as usize + x) * pixel;
                sampled.extend_from_slice(&data[offset..offset + pixel]);
            }
        }
        data = sampled;
    }
    c.bytes += bytes;
    let path = c.directory.join(format!(
        "frame_{}_texture_{}.{}32{}",
        c.frame,
        texture,
        if depth { "r" } else { "rgba" },
        if integer { "i" } else { "f" }
    ));
    match std::fs::write(&path, data) {
        Ok(()) => {
            json!({"status": "ok", "path": path, "bytes": bytes, "width": out_w, "height": out_h, "source_width": width, "source_height": height, "pixel_stride": stride, "captured_before_operation": c.operations.len(), "channels": channels, "scalar": if integer { "int32" } else { "float32" }})
        }
        Err(e) => {
            ::log::error!(target: "RENDER", "graphics capture {}: {e}", path.display());
            json!({"status": "error", "error": e.to_string()})
        }
    }
}
