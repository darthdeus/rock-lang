//! Opt-in UI submission snapshots. No pointer or borrowed buffer survives a hook.
use serde_json::{json, Value};
use std::cell::RefCell;
use std::ffi::{c_char, c_int, CStr};

#[derive(Default)]
struct Probe {
    on: bool,
    event: u64,
    windowless: bool,
    active: Option<usize>,
    stack: Vec<usize>,
    current_pass: Option<Value>,
    context: Option<Value>,
    semantic: Vec<Value>,
    host: Vec<Value>,
    rejected: Vec<String>,
    refusals: Vec<Value>,
    resources: Vec<Value>,
    observations: Vec<Value>,
    imgui_pending: Option<usize>,
}

thread_local! { static PROBE: RefCell<Probe> = RefCell::new(Probe::default()); }

pub(crate) fn enabled() -> bool {
    PROBE.with(|p| p.borrow().on)
}
pub(crate) fn windowless() -> bool {
    PROBE.with(|p| {
        let p = p.borrow();
        p.on && p.windowless
    })
}

fn reject(message: impl Into<String>) {
    PROBE.with(|p| p.borrow_mut().rejected.push(message.into()));
}

fn accept(kind: &str, key: &str, mut detail: Value) -> usize {
    PROBE.with(|p| {
        let mut p = p.borrow_mut();
        if kind == "batch" || kind == "custom" {
            if let Some(context) = &p.context {
                for key in ["phase", "pass", "surface", "space"] { detail[key] = context[key].clone(); }
            }
        }
        if kind == "fullscreen" {
            if let Some(pass) = &p.current_pass { detail["pass"] = pass.clone(); }
        }
        let id = p.semantic.len();
        p.event += 1;
        let event = p.event;
        let parent = p.active;
        p.semantic.push(json!({"id": id, "parent_id": parent, "event": event, "kind": kind, "key": key, "detail": detail}));
        p.stack.push(id);
        p.active = Some(id);
        id
    })
}

pub(super) fn consumed(kind: &str, payload: Value) {
    PROBE.with(|p| {
        let mut p = p.borrow_mut();
        let id = p.active;
        let mode = if p.windowless { "recording" } else { "device" };
        p.event += 1;
        let event = p.event;
        p.host.push(json!({"event": event, "semantic_id": id, "kind": kind, "mode": mode, "payload": payload}));
    });
}

fn snapshot() -> Value {
    PROBE.with(|p| {
        let p = p.borrow();
        json!({"schema": "rock-ui-submission/1", "resources": p.resources,
            "semantic": p.semantic, "host": p.host, "rejected": p.rejected, "refusals": p.refusals,
            "observations": p.observations})
    })
}

fn is_ancestor(rows: &[Value], ancestor: usize, child: usize) -> bool {
    let mut child = child;
    while let Some(parent) = rows[child]["parent_id"].as_u64() {
        let parent = parent as usize;
        if parent == ancestor {
            return true;
        }
        if parent >= child {
            return false;
        }
        child = parent;
    }
    false
}

fn population_error(p: &Probe, expected: usize) -> Option<String> {
    if expected == 0 || p.semantic.len() != expected {
        return Some(format!(
            "expected {expected} nonempty operations; accepted {}",
            p.semantic.len()
        ));
    }
    if !p.rejected.is_empty() {
        return Some(format!("rejections: {:?}", p.rejected));
    }
    let mut last = 0;
    for host in &p.host {
        let Some(id) = host["semantic_id"].as_u64() else {
            return Some("host draw has no accepted operation".into());
        };
        if id as usize >= p.semantic.len() {
            return Some("host has no accepted operation".into());
        }
        if id < last && !is_ancestor(&p.semantic, id as usize, last as usize) {
            return Some("host consumption order differs".into());
        }
        last = id;
    }
    for (id, semantic) in p.semantic.iter().enumerate() {
        let draws: Vec<_> = p
            .host
            .iter()
            .filter(|h| h["semantic_id"].as_u64() == Some(id as u64))
            .collect();
        if draws.is_empty() {
            return Some(format!("accepted operation {id} has no consumed draw"));
        }
        if semantic["kind"] == "batch" {
            if let Some(texture) = semantic["detail"].get("texture") {
                if draws
                    .iter()
                    .any(|h| h["kind"] == "mesh" && h["payload"]["texture"] != *texture)
                {
                    return Some(format!("operation {id} texture changed at consumption"));
                }
            }
        }
        if let Some(want) = semantic.get("vertices") {
            let mut first = 0;
            for draw in &draws {
                let payload = &draw["payload"];
                if payload["first"].as_u64() != Some(first) {
                    return Some("host vertex ranges are not contiguous".into());
                }
                let Some(count) = payload["count"].as_u64() else {
                    return Some("host vertex count is missing".into());
                };
                first += count;
            }
            let actual: Vec<_> = draws
                .iter()
                .flat_map(|h| {
                    h["payload"]["vertices"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .cloned()
                })
                .collect();
            if want.as_array() != Some(&actual) {
                return Some(format!(
                    "operation {id} vertex stream changed at consumption"
                ));
            }
        } else if semantic["kind"] == "custom" {
            if draws
                .iter()
                .filter(|h| h["kind"] == "custom_complete")
                .count()
                != 1
            {
                return Some("custom callback completion population differs".into());
            }
        } else if semantic["kind"] == "fullscreen" || semantic["kind"] == "imgui" {
            if draws.len() != 1 {
                return Some("duplicate host consumption".into());
            }
            if semantic["kind"] == "fullscreen" {
                let Some(uv) = semantic["detail"]["uv"].as_array() else {
                    return Some("missing fullscreen UV".into());
                };
                let bits: Vec<_> = uv
                    .iter()
                    .map(|v| v.as_f64().map(|v| (v as f32).to_bits()))
                    .collect();
                if bits.iter().any(Option::is_none)
                    || json!(bits) != draws[0]["payload"]["uv_f32_bits"]
                {
                    return Some("fullscreen UV changed at the f32 host boundary".into());
                }
            }
            if semantic["kind"] == "imgui" && draws[0]["payload"] != semantic["detail"] {
                return Some("ImGui packet changed at consumption".into());
            }
        }
    }
    if p.host.iter().any(|h| h["semantic_id"].is_null()) {
        return Some("host draw has no accepted operation".into());
    }
    None
}

unsafe fn string<'a>(p: *const c_char) -> Result<&'a str, String> {
    if p.is_null() {
        return Err("null trace string".into());
    }
    unsafe { CStr::from_ptr(p) }
        .to_str()
        .map_err(|e| e.to_string())
}

// The caller supplies the authoritative layout, in f32 lanes. Vertex's lanes
// are never restated here.
unsafe fn vertices(data: *const f32, count: usize, stride: usize) -> Result<Value, String> {
    if data.is_null() || count == 0 || stride == 0 || stride > 1024 {
        return Err("invalid or empty trace vertex payload".into());
    }
    let lanes = count
        .checked_mul(stride)
        .ok_or("trace vertex size overflow")?;
    let data = unsafe { std::slice::from_raw_parts(data, lanes) };
    if data.iter().any(|f| !f.is_finite()) {
        return Err("nonfinite trace geometry".into());
    }
    // JSON numeric values preserve exact f32 semantic values; bit lanes separately
    // preserve -0 and the GL wire, without decimal formatting tolerances.
    Ok(json!(data
        .chunks_exact(stride)
        .map(|v| {
            json!({"values": v, "f32_bits": v.iter().map(|f| f.to_bits()).collect::<Vec<_>>()})
        })
        .collect::<Vec<_>>()))
}

pub(crate) unsafe fn mesh_consumed(
    data: *const f32,
    count: usize,
    stride: usize,
    texture_kind: c_int,
    texture: u64,
    first: usize,
) -> bool {
    if !enabled() {
        return true;
    }
    match unsafe { vertices(data, count, stride) } {
        Ok(vertices) => {
            consumed(
                "mesh",
                json!({"vertices": vertices, "first": first,
                "count": count, "stride_f32": stride,
                "texture": {"kind": if texture_kind == 1 { "canvas" } else { "image" }, "handle": texture as u32}}),
            );
            true
        }
        Err(e) => {
            reject(e);
            false
        }
    }
}

// Probe truth follows the actual element order. Expansion exists only while
// observing a draw; production submissions keep their compact source buffer.
pub(crate) unsafe fn indexed_mesh_consumed(
    data: *const f32,
    stride: usize,
    indices: &[u32],
    texture_kind: c_int,
    texture: u64,
    first: usize,
) -> bool {
    if !enabled() {
        return true;
    }
    let Some(count) = indices.len().checked_mul(stride) else {
        reject("indexed mesh probe size overflow");
        return false;
    };
    let mut expanded = Vec::with_capacity(count);
    for &index in indices {
        expanded.extend_from_slice(unsafe {
            std::slice::from_raw_parts(data.add(index as usize * stride), stride)
        });
    }
    unsafe {
        mesh_consumed(
            expanded.as_ptr(),
            indices.len(),
            stride,
            texture_kind,
            texture,
            first,
        )
    }
}

pub(crate) fn fullscreen_consumed(uv: [f64; 4]) {
    if enabled() {
        if uv.iter().any(|v| !v.is_finite()) {
            reject("nonfinite fullscreen UV");
            return;
        }
        consumed(
            "fullscreen",
            json!({"uv": uv, "uv_f32_bits": uv.map(|v| (v as f32).to_bits())}),
        );
    }
}

#[no_mangle]
pub extern "C" fn stone_ui_probe_begin(windowless: bool) {
    PROBE.with(|p| *p.borrow_mut() = Probe { on: true, windowless,
        resources: vec![
            json!({"kind": "image", "handle": 0, "source": "builtin:none", "generation": 0, "born": 0, "retired": null}),
            json!({"kind": "window", "handle": 0, "source": "builtin:window", "generation": 0, "born": 0, "retired": null}),
            json!({"kind": "gl_program", "handle": 0, "source": "builtin:none", "generation": 0, "born": 0, "retired": null}),
            json!({"kind": "gl_framebuffer", "handle": 0, "source": "builtin:window", "generation": 0, "born": 0, "retired": null}),
            json!({"kind": "gl_texture", "handle": 0, "source": "builtin:none", "generation": 0, "born": 0, "retired": null}),
        ], ..Probe::default() });
}

#[no_mangle]
pub extern "C" fn stone_ui_probe_context(
    phase: *const c_char,
    pass: *const c_char,
    surface: *const c_char,
    space: *const c_char,
) {
    if !enabled() {
        return;
    }
    let parsed = unsafe {
        (|| {
            Ok::<_, String>((
                string(phase)?,
                string(pass)?,
                string(surface)?,
                string(space)?,
            ))
        })()
    };
    match parsed {
        Ok((phase, pass, surface, space)) => PROBE.with(|p| {
            p.borrow_mut().context =
                Some(json!({"phase": phase, "pass": pass, "surface": surface, "space": space}))
        }),
        Err(e) => reject(e),
    }
}

#[no_mangle]
pub extern "C" fn stone_ui_probe_end_operation() {
    if !enabled() {
        return;
    }
    PROBE.with(|p| {
        let mut p = p.borrow_mut();
        p.stack.pop();
        p.active = p.stack.last().copied();
    });
}

#[no_mangle]
pub extern "C" fn stone_ui_probe_custom_consumed() {
    if !enabled() {
        return;
    }
    consumed("custom_complete", json!({"restored": true}));
    crate::with_gl(device_state);
}

#[no_mangle]
pub extern "C" fn stone_ui_probe_stop() {
    PROBE.with(|p| p.borrow_mut().on = false);
}

#[no_mangle]
pub extern "C" fn stone_ui_probe_accept(
    kind: *const c_char,
    key: *const c_char,
    detail: *const c_char,
) -> c_int {
    if !enabled() {
        return -1;
    }
    let parsed = unsafe {
        (|| {
            Ok::<_, String>((
                string(kind)?,
                string(key)?,
                serde_json::from_str::<Value>(string(detail)?).map_err(|e| e.to_string())?,
            ))
        })()
    };
    match parsed {
        Ok((kind, key, detail)) => accept(kind, key, detail) as c_int,
        Err(e) => {
            reject(e);
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn stone_ui_probe_expect_vertices(
    id: c_int,
    data: *const f32,
    count: c_int,
    stride: c_int,
) -> bool {
    if !enabled() {
        return true;
    }
    if id < 0 || count <= 0 || stride <= 0 {
        reject("invalid expected vertex range");
        return false;
    }
    match unsafe { vertices(data, count as usize, stride as usize) } {
        Ok(v) => PROBE.with(|p| {
            let mut p = p.borrow_mut();
            let Some(row) = p.semantic.get_mut(id as usize) else {
                return false;
            };
            row["vertices"] = v;
            true
        }),
        Err(e) => {
            reject(e);
            false
        }
    }
}

#[no_mangle]
pub extern "C" fn stone_ui_probe_resource(
    kind: *const c_char,
    handle: u64,
    source: *const c_char,
    generation: c_int,
) -> bool {
    if !enabled() {
        return true;
    }
    let parsed = unsafe { (|| Ok::<_, String>((string(kind)?, string(source)?)))() };
    let (kind, source) = match parsed {
        Ok(v) => v,
        Err(e) => {
            reject(e);
            return false;
        }
    };
    PROBE.with(|p| {
        let mut p = p.borrow_mut();
        if generation < 0 || source.is_empty() {
            p.rejected.push("invalid resource identity".into());
            return false;
        }
        for old in &p.resources {
            if old["kind"] == kind
                && old["handle"] == handle
                && old["source"] == source
                && old["generation"] == generation
                && old["retired"].is_null()
            {
                return true;
            }
            if (old["kind"] == kind && old["handle"] == handle && old["retired"].is_null())
                || (old["kind"] == kind
                    && old["source"] == source
                    && old["generation"] == generation)
            {
                p.rejected.push(
                    "resource mapping is not bijective; retire the old lifetime explicitly".into(),
                );
                return false;
            }
        }
        p.event += 1;
        let event = p.event;
        p.resources
            .push(json!({"kind": kind, "handle": handle, "source": source,
            "generation": generation, "born": event, "retired": null}));
        true
    })
}

#[no_mangle]
pub extern "C" fn stone_ui_probe_retire(kind: *const c_char, handle: u64) -> bool {
    if !enabled() {
        return true;
    }
    let kind = match unsafe { string(kind) } {
        Ok(v) => v,
        Err(e) => {
            reject(e);
            return false;
        }
    };
    PROBE.with(|p| {
        let mut p = p.borrow_mut();
        p.event += 1;
        let event = p.event;
        let Some(row) = p
            .resources
            .iter_mut()
            .find(|v| v["kind"] == kind && v["handle"] == handle && v["retired"].is_null())
        else {
            p.rejected
                .push("retiring an unknown resource lifetime".into());
            return false;
        };
        row["retired"] = json!(event);
        true
    })
}

#[no_mangle]
pub extern "C" fn stone_ui_probe_observe(kind: *const c_char, detail: *const c_char) -> bool {
    if !enabled() {
        return true;
    }
    let parsed = unsafe {
        (|| {
            Ok::<_, String>((
                string(kind)?,
                serde_json::from_str::<Value>(string(detail)?).map_err(|e| e.to_string())?,
            ))
        })()
    };
    match parsed {
        Ok((kind, detail)) => {
            PROBE.with(|p| {
                let mut p = p.borrow_mut();
                p.event += 1;
                let event = p.event;
                if kind == "semantic_pass" {
                    p.current_pass = Some(detail.clone());
                }
                p.observations
                    .push(json!({"event": event, "kind": kind, "detail": detail}));
            });
            true
        }
        Err(e) => {
            reject(e);
            false
        }
    }
}

#[no_mangle]
pub extern "C" fn stone_ui_probe_refuse(kind: *const c_char, detail: *const c_char) -> bool {
    if !enabled() {
        return true;
    }
    let parsed = unsafe {
        (|| {
            Ok::<_, String>((
                string(kind)?,
                serde_json::from_str::<Value>(string(detail)?).map_err(|e| e.to_string())?,
            ))
        })()
    };
    match parsed {
        Ok((kind, detail)) => {
            PROBE.with(|p| {
                let mut p = p.borrow_mut();
                p.event += 1;
                let event = p.event;
                p.refusals
                    .push(json!({"event": event, "kind": kind, "detail": detail}));
            });
            true
        }
        Err(e) => {
            reject(e);
            false
        }
    }
}

#[no_mangle]
pub extern "C" fn stone_ui_probe_check_population(
    accepted: c_int,
    refused: c_int,
    observed: c_int,
) -> bool {
    if accepted < 0
        || refused < 0
        || observed < 0
        || accepted as i64 + refused as i64 + observed as i64 == 0
    {
        return false;
    }
    PROBE.with(|p| {
        let p = p.borrow();
        if p.refusals.len() != refused as usize
            || p.observations.len() != observed as usize
            || !p.rejected.is_empty()
        {
            return false;
        }
        if accepted == 0 {
            return p.semantic.is_empty() && p.host.is_empty();
        }
        population_error(&p, accepted as usize).is_none()
    })
}

#[no_mangle]
pub extern "C" fn stone_ui_probe_check(expected: c_int) -> bool {
    PROBE.with(|p| population_error(&p.borrow(), expected.max(0) as usize).is_none())
}
#[no_mangle]
pub extern "C" fn stone_ui_probe_accepted() -> c_int {
    PROBE.with(|p| p.borrow().semantic.len() as c_int)
}
#[no_mangle]
pub extern "C" fn stone_ui_probe_consumed() -> c_int {
    PROBE.with(|p| p.borrow().host.len() as c_int)
}
#[no_mangle]
pub extern "C" fn stone_ui_probe_z(index: c_int) -> c_int {
    PROBE.with(|p| {
        p.borrow()
            .semantic
            .get(index as usize)
            .and_then(|v| v["detail"]["z"].as_i64())
            .unwrap_or(i32::MIN as i64) as c_int
    })
}
#[no_mangle]
pub extern "C" fn stone_ui_probe_vertex(index: c_int, lane: c_int) -> f64 {
    PROBE.with(|p| {
        p.borrow()
            .semantic
            .get(index as usize)
            .and_then(|v| v["vertices"][0]["values"][lane as usize].as_f64())
            .unwrap_or(f64::NAN)
    })
}
#[no_mangle]
pub extern "C" fn stone_ui_probe_write(path: *const c_char) -> bool {
    let result = unsafe { string(path) }.and_then(|path| {
        let bytes = serde_json::to_vec_pretty(&snapshot()).map_err(|e| e.to_string())?;
        std::fs::write(path, bytes).map_err(|e| e.to_string())
    });
    if let Err(e) = result {
        crate::log_once!("UI_PROBE", "trace write failed: {e}");
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_probe_hooks_do_not_mutate_capture() {
        stone_ui_probe_begin(false);
        stone_ui_probe_stop();
        let before = snapshot();
        assert!(stone_ui_probe_resource(
            std::ptr::null(),
            7,
            std::ptr::null(),
            0
        ));
        assert!(stone_ui_probe_retire(std::ptr::null(), 7));
        assert_eq!(
            stone_ui_probe_accept(std::ptr::null(), std::ptr::null(), std::ptr::null()),
            -1
        );
        assert!(stone_ui_probe_expect_vertices(0, std::ptr::null(), 0, 0));
        assert_eq!(snapshot(), before);
    }

    #[test]
    fn ui_trace_detects_dropped_host_draw() {
        stone_ui_probe_begin(true);
        accept("batch", "key", json!({"z": 0}));
        assert!(!stone_ui_probe_check(1));
        stone_ui_probe_stop();
    }

    #[test]
    fn ui_trace_detects_wrong_consumed_texture() {
        stone_ui_probe_begin(true);
        let id = accept(
            "batch",
            "key",
            json!({"texture": {"kind": "image", "handle": 7}}),
        );
        let data = [1.25f32, -0.0, 3.5];
        assert!(stone_ui_probe_expect_vertices(
            id as c_int,
            data.as_ptr(),
            1,
            3
        ));
        unsafe {
            mesh_consumed(data.as_ptr(), 1, 3, 0, 9, 0);
        }
        assert!(!stone_ui_probe_check(1));
        stone_ui_probe_stop();
    }

    #[test]
    fn ui_trace_custom_callback_can_consume_its_own_texture() {
        stone_ui_probe_begin(true);
        accept(
            "custom",
            "key",
            json!({"texture": {"kind": "image", "handle": 0}}),
        );
        let data = [1.25f32, -0.0, 3.5];
        unsafe {
            mesh_consumed(data.as_ptr(), 1, 3, 0, 9, 0);
        }
        stone_ui_probe_custom_consumed();
        assert!(stone_ui_probe_check(1));
        stone_ui_probe_end_operation();
        stone_ui_probe_stop();
    }

    #[test]
    fn ui_trace_snapshot_survives_buffer_reuse() {
        stone_ui_probe_begin(true);
        let id = accept("batch", "key", json!({"z": 0}));
        let mut data = [1.25f32, -0.0, 3.5];
        assert!(stone_ui_probe_expect_vertices(
            id as c_int,
            data.as_ptr(),
            1,
            3
        ));
        unsafe {
            mesh_consumed(data.as_ptr(), 1, 3, 0, 0, 0);
        }
        let before = snapshot();
        data.fill(99.0);
        assert_eq!(snapshot(), before);
        assert_eq!(stone_ui_probe_vertex(0, 0), 1.25);
        stone_ui_probe_stop();
    }
}

#[path = "ui_submission_probe_imgui.rs"]
mod imgui_capture;
pub(crate) use imgui_capture::{imgui_consumed, imgui_prepared};

pub(crate) fn device_state(gl: &crate::DebugGl) {
    if !enabled() {
        return;
    }
    let state = unsafe {
        let mut viewport = [0i32; 4];
        let mut scissor = [0i32; 4];
        gl.get_parameter_i32_slice(glow::VIEWPORT, &mut viewport);
        gl.get_parameter_i32_slice(glow::SCISSOR_BOX, &mut scissor);
        let mut color_write = [0i32; 4];
        gl.get_parameter_i32_slice(glow::COLOR_WRITEMASK, &mut color_write);
        let active_texture = gl.get_parameter_i32(glow::ACTIVE_TEXTURE);
        gl.active_texture(glow::TEXTURE0);
        let texture = gl.get_parameter_i32(glow::TEXTURE_BINDING_2D);
        gl.active_texture(active_texture as u32);
        json!({"viewport": viewport, "clip": {"enabled": gl.is_enabled(glow::SCISSOR_TEST), "rect": scissor},
            "color_write": color_write, "unit0_texture": {"kind": "gl_texture", "handle": texture as u32},
            "program": {"kind": "gl_program", "handle": gl.get_parameter_i32(glow::CURRENT_PROGRAM) as u32},
            "target": {"kind": "gl_framebuffer", "handle": gl.get_parameter_i32(glow::DRAW_FRAMEBUFFER_BINDING) as u32},
            "blend": {"enabled": gl.is_enabled(glow::BLEND), "src_rgb": gl.get_parameter_i32(glow::BLEND_SRC_RGB),
                "dst_rgb": gl.get_parameter_i32(glow::BLEND_DST_RGB), "src_alpha": gl.get_parameter_i32(glow::BLEND_SRC_ALPHA),
                "dst_alpha": gl.get_parameter_i32(glow::BLEND_DST_ALPHA), "equation_rgb": gl.get_parameter_i32(glow::BLEND_EQUATION_RGB),
                "equation_alpha": gl.get_parameter_i32(glow::BLEND_EQUATION_ALPHA)},
            "depth": {"enabled": gl.is_enabled(glow::DEPTH_TEST), "function": gl.get_parameter_i32(glow::DEPTH_FUNC),
                "write": gl.get_parameter_i32(glow::DEPTH_WRITEMASK)}})
    };
    PROBE.with(|p| {
        let mut p = p.borrow_mut();
        if let Some(row) = p.host.last_mut() {
            row["payload"]["state"] = state;
        }
    });
}

pub(crate) fn block_uploaded(shader: u64, fingerprint: u64, bytes: &[u8]) {
    if !enabled() {
        return;
    }
    PROBE.with(|p| {
        let mut p = p.borrow_mut();
        p.event += 1;
        let event = p.event;
        let id = p.active;
        p.observations
            .push(json!({"event": event, "kind": "uniform_upload", "detail": {
            "semantic_id": id, "shader": {"kind": "shader", "handle": shader},
            "fingerprint": fingerprint, "bytes": bytes}}));
    });
}
