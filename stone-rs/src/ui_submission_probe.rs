//! Opt-in UI submission snapshots. No pointer or borrowed buffer survives a hook.
use serde_json::{json, Value};
use std::cell::RefCell;
use std::ffi::{c_char, c_int, CStr};

#[path = "ui_submission_probe_imgui.rs"]
mod imgui_capture;
pub(crate) use imgui_capture::{imgui_consumed, imgui_prepared};

#[derive(Default)]
struct Probe {
    on: bool,
    windowless: bool,
    event: u64,
    open: Vec<usize>,
    current_pass: Option<Value>,
    context: Option<Value>,
    imgui_pending: Option<usize>,
    semantic: Vec<Value>,
    host: Vec<Value>,
    rejected: Vec<String>,
    refusals: Vec<Value>,
    resources: Vec<Value>,
    observations: Vec<Value>,
}

thread_local! { static PROBE: RefCell<Probe> = RefCell::new(Probe::default()); }

impl Probe {
    fn tick(&mut self) -> u64 {
        self.event += 1;
        self.event
    }

    fn accept(&mut self, kind: &str, key: &str, mut detail: Value) -> usize {
        match (kind, &self.context, &self.current_pass) {
            ("batch" | "custom", Some(context), _) => {
                for key in ["phase", "pass", "surface", "space"] {
                    detail[key] = context[key].clone();
                }
            }
            ("fullscreen", _, Some(pass)) => detail["pass"] = pass.clone(),
            _ => {}
        }
        let id = self.semantic.len();
        let (parent_id, event) = (self.open.last().copied(), self.tick());
        self.semantic
            .push(json!({"id": id, "parent_id": parent_id, "event": event,
            "kind": kind, "key": key, "detail": detail}));
        self.open.push(id);
        id
    }

    fn consume(&mut self, kind: &str, payload: Value) {
        let mode = if self.windowless {
            "recording"
        } else {
            "device"
        };
        let (semantic_id, event) = (self.open.last().copied(), self.tick());
        self.host
            .push(json!({"event": event, "semantic_id": semantic_id,
            "kind": kind, "mode": mode, "payload": payload}));
    }

    fn note(&mut self, kind: &str, detail: Value) -> Value {
        json!({"event": self.tick(), "kind": kind, "detail": detail})
    }

    fn population_ok(&self, expected: usize) -> bool {
        let rows = &self.semantic;
        if rows.len() != expected || !self.rejected.is_empty() {
            return false;
        }
        let mut last = 0;
        for host in &self.host {
            let Some(id) = host["semantic_id"].as_u64().map(|id| id as usize) else {
                return false;
            };
            if id >= rows.len() || id < last && !is_ancestor(rows, id, last) {
                return false;
            }
            last = id;
        }
        rows.iter().enumerate().all(|(id, row)| {
            let draws: Vec<&Value> = self
                .host
                .iter()
                .filter(|h| h["semantic_id"] == id)
                .collect();
            let kind = &row["kind"];
            if draws.is_empty() {
                return false;
            }
            if let (true, Some(texture)) = (kind == "batch", row["detail"].get("texture")) {
                if draws
                    .iter()
                    .any(|h| h["kind"] == "mesh" && h["payload"]["texture"] != *texture)
                {
                    return false;
                }
            }
            if let Some(want) = row.get("vertices") {
                let mut first = 0;
                for draw in &draws {
                    let payload = &draw["payload"];
                    let (Some(start), Some(count)) =
                        (payload["first"].as_u64(), payload["count"].as_u64())
                    else {
                        return false;
                    };
                    if start != first {
                        return false;
                    }
                    first += count;
                }
                let actual: Vec<Value> = draws
                    .iter()
                    .flat_map(|h| {
                        h["payload"]["vertices"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .cloned()
                    })
                    .collect();
                want.as_array() == Some(&actual)
            } else if kind == "custom" {
                draws
                    .iter()
                    .filter(|h| h["kind"] == "custom_complete")
                    .count()
                    == 1
            } else if kind == "fullscreen" {
                let bits: Option<Vec<u32>> = row["detail"]["uv"].as_array().and_then(|uv| {
                    uv.iter()
                        .map(|v| v.as_f64().map(|v| (v as f32).to_bits()))
                        .collect()
                });
                draws.len() == 1
                    && bits.is_some_and(|bits| json!(bits) == draws[0]["payload"]["uv_f32_bits"])
            } else if kind == "imgui" {
                draws.len() == 1 && draws[0]["payload"] == row["detail"]
            } else {
                true
            }
        })
    }
}

fn is_ancestor(rows: &[Value], ancestor: usize, mut child: usize) -> bool {
    while let Some(parent) = rows[child]["parent_id"].as_u64().map(|p| p as usize) {
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

/// Runs `f` on an active probe. A disabled probe returns `off`; an error is
/// recorded as a rejection and returns `failed`.
fn hook<T>(off: T, failed: T, f: impl FnOnce(&mut Probe) -> Result<T, String>) -> T {
    PROBE.with(|p| {
        let p = &mut *p.borrow_mut();
        if !p.on {
            return off;
        }
        f(p).unwrap_or_else(|e| {
            p.rejected.push(e);
            failed
        })
    })
}

pub(crate) fn enabled() -> bool {
    PROBE.with(|p| p.borrow().on)
}

pub(crate) fn windowless() -> bool {
    PROBE.with(|p| {
        let p = p.borrow();
        p.on && p.windowless
    })
}

#[cfg(test)]
fn snapshot() -> Value {
    PROBE.with(|p| {
        let p = p.borrow();
        json!({"schema": "rock-ui-submission/1", "resources": p.resources,
            "semantic": p.semantic, "host": p.host, "rejected": p.rejected, "refusals": p.refusals,
            "observations": p.observations})
    })
}

unsafe fn text<'a>(p: *const c_char) -> Result<&'a str, String> {
    if p.is_null() {
        return Err("null trace string".into());
    }
    unsafe { CStr::from_ptr(p) }
        .to_str()
        .map_err(|e| e.to_string())
}

unsafe fn json_text(p: *const c_char) -> Result<Value, String> {
    serde_json::from_str(unsafe { text(p) }?).map_err(|e| e.to_string())
}

/// JSON numbers keep the exact f32 values; the bit lanes keep -0 and the GL
/// wire without decimal formatting tolerances.
fn lanes(xs: &[f32]) -> Value {
    json!({"values": xs, "f32_bits": xs.iter().map(|v| v.to_bits()).collect::<Vec<_>>()})
}

// The caller supplies the authoritative layout, in f32 lanes. Vertex's lanes
// are never restated here.
unsafe fn vertices(
    data: *const f32,
    stride: usize,
    rows: impl ExactSizeIterator<Item = usize>,
) -> Result<Value, String> {
    if data.is_null() || rows.len() == 0 || stride == 0 || stride > 1024 {
        return Err("invalid or empty trace vertex payload".into());
    }
    let rows: Vec<&[f32]> = rows
        .map(|row| unsafe { std::slice::from_raw_parts(data.add(row * stride), stride) })
        .collect();
    if rows.iter().copied().flatten().any(|f| !f.is_finite()) {
        return Err("nonfinite trace geometry".into());
    }
    Ok(Value::Array(rows.into_iter().map(lanes).collect()))
}

unsafe fn record_mesh(
    data: *const f32,
    stride: usize,
    rows: impl ExactSizeIterator<Item = usize>,
    texture_kind: c_int,
    texture: u64,
    first: usize,
) -> bool {
    hook(true, false, |p| {
        let count = rows.len();
        let vertices = unsafe { vertices(data, stride, rows) }?;
        let kind = if texture_kind == 1 { "canvas" } else { "image" };
        p.consume(
            "mesh",
            json!({"vertices": vertices, "first": first, "count": count,
            "stride_f32": stride, "texture": {"kind": kind, "handle": texture as u32}}),
        );
        Ok(true)
    })
}

pub(crate) unsafe fn mesh_consumed(
    data: *const f32,
    count: usize,
    stride: usize,
    texture_kind: c_int,
    texture: u64,
    first: usize,
) -> bool {
    unsafe { record_mesh(data, stride, 0..count, texture_kind, texture, first) }
}

pub(crate) unsafe fn indexed_mesh_consumed(
    data: *const f32,
    stride: usize,
    indices: &[u32],
    texture_kind: c_int,
    texture: u64,
    first: usize,
) -> bool {
    let rows = indices.iter().map(|&i| i as usize);
    unsafe { record_mesh(data, stride, rows, texture_kind, texture, first) }
}

pub(crate) fn fullscreen_consumed(uv: [f64; 4]) {
    hook((), (), |p| {
        if uv.iter().any(|v| !v.is_finite()) {
            return Err("nonfinite fullscreen UV".into());
        }
        p.consume(
            "fullscreen",
            json!({"uv": uv, "uv_f32_bits": uv.map(|v| (v as f32).to_bits())}),
        );
        Ok(())
    })
}

pub(crate) fn block_uploaded(shader: u64, fingerprint: u64, bytes: &[u8]) {
    hook((), (), |p| {
        let detail = json!({"semantic_id": p.open.last(), "shader": {"kind": "shader", "handle": shader},
            "fingerprint": fingerprint, "bytes": bytes});
        let row = p.note("uniform_upload", detail);
        p.observations.push(row);
        Ok(())
    })
}

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
        if let Some(row) = p.borrow_mut().host.last_mut() {
            row["payload"]["state"] = state;
        }
    });
}

const BUILTIN_RESOURCES: [(&str, &str); 5] = [
    ("image", "builtin:none"),
    ("window", "builtin:window"),
    ("gl_program", "builtin:none"),
    ("gl_framebuffer", "builtin:window"),
    ("gl_texture", "builtin:none"),
];

#[no_mangle]
pub extern "C" fn stone_ui_probe_begin(windowless: bool) {
    let resources = BUILTIN_RESOURCES
        .iter()
        .map(|(kind, source)| {
            json!({"kind": kind, "handle": 0, "source": source, "generation": 0, "born": 0, "retired": null})
        })
        .collect();
    PROBE.with(|p| {
        *p.borrow_mut() = Probe {
            on: true,
            windowless,
            resources,
            ..Probe::default()
        }
    });
}

#[no_mangle]
pub extern "C" fn stone_ui_probe_stop() {
    PROBE.with(|p| p.borrow_mut().on = false);
}

#[no_mangle]
pub extern "C" fn stone_ui_probe_context(
    phase: *const c_char,
    pass: *const c_char,
    surface: *const c_char,
    space: *const c_char,
) {
    hook((), (), |p| {
        let [phase, pass, surface, space] =
            unsafe { [text(phase)?, text(pass)?, text(surface)?, text(space)?] };
        p.context = Some(json!({"phase": phase, "pass": pass, "surface": surface, "space": space}));
        Ok(())
    })
}

#[no_mangle]
pub extern "C" fn stone_ui_probe_accept(
    kind: *const c_char,
    key: *const c_char,
    detail: *const c_char,
) -> c_int {
    hook(-1, -1, |p| {
        let (kind, key, detail) = unsafe { (text(kind)?, text(key)?, json_text(detail)?) };
        Ok(p.accept(kind, key, detail) as c_int)
    })
}

#[no_mangle]
pub extern "C" fn stone_ui_probe_end_operation() {
    hook((), (), |p| {
        p.open.pop();
        Ok(())
    })
}

#[no_mangle]
pub extern "C" fn stone_ui_probe_custom_consumed() {
    hook((), (), |p| {
        p.consume("custom_complete", json!({"restored": true}));
        Ok(())
    });
    crate::with_gl(device_state);
}

#[no_mangle]
pub extern "C" fn stone_ui_probe_expect_vertices(
    id: c_int,
    data: *const f32,
    count: c_int,
    stride: c_int,
) -> bool {
    hook(true, false, |p| {
        if id < 0 || count <= 0 || stride <= 0 {
            return Err("invalid expected vertex range".into());
        }
        let vertices = unsafe { vertices(data, stride as usize, 0..count as usize) }?;
        let Some(row) = p.semantic.get_mut(id as usize) else {
            return Ok(false);
        };
        row["vertices"] = vertices;
        Ok(true)
    })
}

#[no_mangle]
pub extern "C" fn stone_ui_probe_resource(
    kind: *const c_char,
    handle: u64,
    source: *const c_char,
    generation: c_int,
) -> bool {
    hook(true, false, |p| {
        let (kind, source) = unsafe { (text(kind)?, text(source)?) };
        if generation < 0 || source.is_empty() {
            return Err("invalid resource identity".into());
        }
        for old in &p.resources {
            let live = old["retired"].is_null();
            let same_handle = old["kind"] == kind && old["handle"] == handle;
            let same_source =
                old["kind"] == kind && old["source"] == source && old["generation"] == generation;
            if same_handle && same_source && live {
                return Ok(true);
            }
            if same_handle && live || same_source {
                return Err(
                    "resource mapping is not bijective; retire the old lifetime explicitly".into(),
                );
            }
        }
        let born = p.tick();
        p.resources
            .push(json!({"kind": kind, "handle": handle, "source": source,
            "generation": generation, "born": born, "retired": null}));
        Ok(true)
    })
}

#[no_mangle]
pub extern "C" fn stone_ui_probe_retire(kind: *const c_char, handle: u64) -> bool {
    hook(true, false, |p| {
        let kind = unsafe { text(kind) }?;
        let retired = p.tick();
        let row = p
            .resources
            .iter_mut()
            .find(|v| v["kind"] == kind && v["handle"] == handle && v["retired"].is_null())
            .ok_or("retiring an unknown resource lifetime")?;
        row["retired"] = json!(retired);
        Ok(true)
    })
}

#[no_mangle]
pub extern "C" fn stone_ui_probe_observe(kind: *const c_char, detail: *const c_char) -> bool {
    hook(true, false, |p| {
        let (kind, detail) = unsafe { (text(kind)?, json_text(detail)?) };
        if kind == "semantic_pass" {
            p.current_pass = Some(detail.clone());
        }
        let row = p.note(kind, detail);
        p.observations.push(row);
        Ok(true)
    })
}

#[no_mangle]
pub extern "C" fn stone_ui_probe_refuse(kind: *const c_char, detail: *const c_char) -> bool {
    hook(true, false, |p| {
        let (kind, detail) = unsafe { (text(kind)?, json_text(detail)?) };
        let row = p.note(kind, detail);
        p.refusals.push(row);
        Ok(true)
    })
}

#[no_mangle]
pub extern "C" fn stone_ui_probe_check_population(
    accepted: c_int,
    refused: c_int,
    observed: c_int,
) -> bool {
    if accepted.min(refused).min(observed) < 0 || (accepted, refused, observed) == (0, 0, 0) {
        return false;
    }
    PROBE.with(|p| {
        let p = p.borrow();
        p.refusals.len() == refused as usize
            && p.observations.len() == observed as usize
            && p.population_ok(accepted as usize)
    })
}

#[no_mangle]
pub extern "C" fn stone_ui_probe_check(expected: c_int) -> bool {
    expected > 0 && PROBE.with(|p| p.borrow().population_ok(expected as usize))
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
    let z = PROBE.with(|p| p.borrow().semantic.get(index as usize)?["detail"]["z"].as_i64());
    z.unwrap_or(i32::MIN as i64) as c_int
}

#[no_mangle]
pub extern "C" fn stone_ui_probe_vertex(index: c_int, lane: c_int) -> f64 {
    let semantic = |p: &Probe| {
        p.semantic.get(index as usize)?["vertices"][0]["values"][lane as usize].as_f64()
    };
    PROBE.with(|p| semantic(&p.borrow())).unwrap_or(f64::NAN)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accept(kind: &str, key: &str, detail: Value) -> usize {
        PROBE.with(|p| p.borrow_mut().accept(kind, key, detail))
    }

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
    fn ui_trace_resource_lifetimes_and_population_counts() {
        let c = |s: &str| std::ffi::CString::new(s).unwrap();
        let (image, a, b) = (c("image"), c("a.png"), c("b.png"));
        let resource = |handle, source: &std::ffi::CString, generation| {
            stone_ui_probe_resource(image.as_ptr(), handle, source.as_ptr(), generation)
        };
        stone_ui_probe_begin(true);
        assert!(resource(7, &a, 0));
        assert!(
            resource(7, &a, 0),
            "restating a live lifetime is idempotent"
        );
        assert!(stone_ui_probe_observe(
            c("semantic_pass").as_ptr(),
            c("{}").as_ptr()
        ));
        assert!(stone_ui_probe_refuse(
            c("present").as_ptr(),
            c("{}").as_ptr()
        ));
        assert!(stone_ui_probe_check_population(0, 1, 1));
        assert!(!stone_ui_probe_check_population(0, 0, 1));
        assert!(stone_ui_probe_retire(image.as_ptr(), 7));
        assert!(
            !resource(7, &a, 0),
            "a retired source generation is never reborn"
        );
        assert!(resource(7, &a, 1));
        assert!(!resource(7, &b, 0), "a live handle maps to one source");
        assert!(!stone_ui_probe_retire(image.as_ptr(), 8));
        assert!(
            !stone_ui_probe_check_population(0, 1, 1),
            "rejections fail the population"
        );
        let trace = snapshot();
        assert_eq!(trace["resources"][5]["retired"], 4);
        assert_eq!(trace["resources"][6]["born"], 5);
        assert_eq!(trace["rejected"].as_array().unwrap().len(), 3);
        stone_ui_probe_stop();
    }

    #[test]
    fn ui_trace_checks_fullscreen_uv_and_nested_consumption_order() {
        for (consumed_v0, ok) in [(0.5, true), (0.25, false)] {
            stone_ui_probe_begin(true);
            accept("custom", "outer", json!({}));
            accept("fullscreen", "inner", json!({"uv": [0, 0.5, 1, 1]}));
            fullscreen_consumed([0.0, consumed_v0, 1.0, 1.0]);
            stone_ui_probe_end_operation();
            stone_ui_probe_custom_consumed();
            stone_ui_probe_end_operation();
            assert_eq!(stone_ui_probe_check(2), ok);
        }
        stone_ui_probe_stop();
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
