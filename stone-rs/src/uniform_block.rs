//! The compiler packs std140 bytes; this module streams them and checks the
//! driver layout. Binding indices are global GL state.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{c_char, c_int, CStr};

use crate::stream_buffer::{Stream, StreamRange};
use crate::{with_gl, SHADERS};

#[derive(Clone)]
pub(crate) struct BlockMember {
    pub name: String,
    pub offset: i32,
    pub kind: i32,
    pub capacity: i32,
    pub stride: i32,
}

/// One of a program's uniform blocks.
struct BlockBuffer {
    /// The program the block belongs to. An upload binds its range at once
    /// only while this program is current; otherwise the next switch binds it.
    program: glow::Program,
    /// Valid for draws of `range.frame` only; a later frame streams `bytes` again.
    range: Option<StreamRange>,
    name: String,
    shader_key: String,
    size: usize,
    /// Last bytes uploaded, including zero initialization. Render scopes
    /// snapshot these without synchronizing the GPU.
    bytes: Vec<u8>,
    uploaded: bool,
    drawn: bool,
    /// Reported once. This is checked every frame; a broken block is broken
    /// every frame.
    reported: bool,
    /// `glBindBufferRange` is global state, so blocks re-bind whenever their program goes
    /// current (`bind_block_for_program`).
    binding: u32,
    /// Packer and shader layout fingerprints must match even at equal size.
    fingerprint: u64,
    /// Registered std140 member layout for native-renderer reflection.
    members: Vec<BlockMember>,
}

thread_local! {
    static BLOCKS: RefCell<HashMap<u64, Vec<BlockBuffer>>> = RefCell::new(HashMap::new());
    static BLOCK_PROGRAMS: RefCell<HashMap<glow::Program, u64>> = RefCell::new(HashMap::new());
}

thread_local! {
    static BINDING_STATE_JSON: RefCell<Option<std::ffi::CString>> = const { RefCell::new(None) };
}

/// Include the engine block's projection, view and bounds as well as the
/// smaller custom fragment groups in the on-demand diagnostic.
const STATE_FLOATS: usize = 40;

fn leading_floats(bytes: &[u8]) -> Vec<f32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .take(STATE_FLOATS)
        .map(|c| f32::from_le_bytes(*c))
        .collect()
}

fn buffer_id(buffer: Option<glow::Buffer>) -> u32 {
    buffer.map_or(0, |b| b.0.get())
}

/// Diagnostic JSON for one program, or all programs when handle is zero.
/// The returned pointer is valid until the next call.
#[no_mangle]
pub extern "C" fn stone_uniform_block_binding_state_json(handle: u64) -> *const c_char {
    let json = binding_state(handle).to_string();
    BINDING_STATE_JSON.with(|slot| match std::ffi::CString::new(json) {
        Ok(c) => {
            let ptr = c.as_ptr();
            *slot.borrow_mut() = Some(c);
            ptr
        }
        Err(e) => {
            ::log::error!(target: "SHADER", "stone_uniform_block_binding_state_json: {e}");
            std::ptr::null()
        }
    })
}

fn binding_state(handle: u64) -> serde_json::Value {
    use serde_json::json;
    with_gl(|gl| unsafe {
        let current = crate::CURRENT_SHADER.with(|c| *c.borrow());
        let gl_current = gl.get_parameter_i32(glow::CURRENT_PROGRAM);
        let previous_generic =
            std::num::NonZeroU32::new(gl.get_parameter_i32(glow::UNIFORM_BUFFER_BINDING) as u32)
                .map(glow::NativeBuffer);
        let frame = crate::stream_buffer::frame();
        let bound_at = |index: u32| -> (Option<glow::Buffer>, i32, i32) {
            let buffer = std::num::NonZeroU32::new(
                gl.get_parameter_indexed_i32(glow::UNIFORM_BUFFER_BINDING, index) as u32,
            )
            .map(glow::NativeBuffer);
            (
                buffer,
                gl.get_parameter_indexed_i32(glow::UNIFORM_BUFFER_START, index),
                gl.get_parameter_indexed_i32(glow::UNIFORM_BUFFER_SIZE, index),
            )
        };
        let read_back = |buffer: glow::Buffer, offset: i32, size: usize| -> Vec<u8> {
            gl.bind_buffer(glow::UNIFORM_BUFFER, Some(buffer));
            let own = gl.get_buffer_parameter_i32(glow::UNIFORM_BUFFER, glow::BUFFER_SIZE).max(0);
            let offset = offset.clamp(0, own);
            let mut out = vec![0u8; size.min((own - offset) as usize)];
            gl.get_buffer_sub_data(glow::UNIFORM_BUFFER, offset, &mut out);
            out
        };
        let mut handles: Vec<u64> = BLOCKS.with(|b| {
            if handle == 0 {
                b.borrow().keys().copied().collect()
            } else {
                vec![handle]
            }
        });
        handles.sort_unstable();
        let programs: Vec<serde_json::Value> = handles
            .iter()
            .map(|&h| {
                let program = program_of(h);
                let blocks: Vec<serde_json::Value> = BLOCKS.with(|b| {
                    let map = b.borrow();
                    let Some(blocks) = map.get(&h) else { return Vec::new() };
                    blocks
                        .iter()
                        .map(|block| {
                            let gl_binding = program.and_then(|p| {
                                gl.get_uniform_block_index(p, &block.name).map(|i| {
                                    gl.get_active_uniform_block_parameter_i32(
                                        p,
                                        i,
                                        glow::UNIFORM_BLOCK_BINDING,
                                    )
                                })
                            });
                            let (bound, bound_offset, bound_size) = bound_at(block.binding);
                            let read_size = if bound_size > 0 { block.size.min(bound_size as usize) } else { block.size };
                            let gpu = bound.map(|buf| read_back(buf, bound_offset, read_size));
                            let live = block.range.filter(|r| r.frame == frame);
                            json!({
                                "name": block.name,
                                "binding": block.binding,
                                "gl_binding": gl_binding,
                                "buffer": buffer_id(block.range.map(|r| r.buffer)),
                                "offset": block.range.map(|r| r.offset),
                                "range_live": live.is_some(),
                                "bound_buffer": buffer_id(bound),
                                "bound_offset": bound_offset,
                                "bound_size": bound_size,
                                "bound_is_own": live.is_some_and(|r| bound == Some(r.buffer)
                                    && bound_offset == r.offset && bound_size == block.size as i32),
                                "size": block.size,
                                "fingerprint": format!("{:#x}", block.fingerprint),
                                "uploaded_ever": block.uploaded,
                                "upload_f32": leading_floats(&block.bytes),
                                "upload_bytes": block.bytes,
                                "gpu_bytes": gpu,
                                "gpu_matches_upload": gpu.as_ref().map(|g| g == &block.bytes),
                                "gpu_f32": gpu.as_ref().map(|g| leading_floats(g)),
                            })
                        })
                        .collect()
                });
                json!({
                    "handle": h,
                    "key": BLOCKS.with(|b| b.borrow().get(&h)
                        .and_then(|bs| bs.first().map(|x| x.shader_key.clone()))),
                    "program": program.map_or(0, |p| p.0.get()),
                    "current": program.is_some() && program == current,
                    "blocks": blocks,
                })
            })
            .collect();
        let max = gl.get_parameter_i32(glow::MAX_UNIFORM_BUFFER_BINDINGS).max(0) as u32;
        let points: Vec<serde_json::Value> = if handle == 0 {
            (0..max.min(16))
                .map(|i| {
                    let (buffer, offset, size) = bound_at(i);
                    json!({ "index": i, "buffer": buffer_id(buffer), "offset": offset, "size": size })
                })
                .collect()
        } else {
            Vec::new()
        };
        gl.bind_buffer(glow::UNIFORM_BUFFER, previous_generic);
        json!({
            "current_program": current.map_or(0, |p| p.0.get()),
            "gl_current_program": gl_current,
            "max_bindings": max,
            "stream_frame": frame,
            "offset_alignment": gl.get_parameter_i32(glow::UNIFORM_BUFFER_OFFSET_ALIGNMENT),
            "binding_points": points,
            "programs": programs,
        })
    })
    .unwrap_or_else(|| serde_json::json!({ "error": "no GL context" }))
}

/// Stream ranges are reclaimed by frame; remove both program indexes.
pub(crate) fn destroy_blocks_for_shader(handle: u64, program: glow::Program) {
    BLOCKS.with(|b| b.borrow_mut().remove(&handle));
    BLOCK_PROGRAMS.with(|m| m.borrow_mut().remove(&program));
}

fn program_of(handle: u64) -> Option<glow::Program> {
    SHADERS.with(|s| s.borrow().get(&handle).map(|shader| shader.program))
}

pub(crate) fn bind_block_for_program(gl: &crate::DebugGl, handle: u64) {
    BLOCKS.with(|b| {
        if let Some(blocks) = b.borrow_mut().get_mut(&handle) {
            for block in blocks {
                unsafe { bind_block(gl, block) };
            }
        }
    });
}

/// Called at the frame boundary after streams advance: the bound ranges belong to the
/// frame that ended.
pub(crate) fn rebind_current_program(gl: &crate::DebugGl) {
    let Some(program) = crate::CURRENT_SHADER.with(|c| *c.borrow()) else {
        return;
    };
    let Some(handle) = BLOCK_PROGRAMS.with(|m| m.borrow().get(&program).copied()) else {
        return;
    };
    bind_block_for_program(gl, handle);
}

/// `block`'s range for draws of this frame, streaming its bytes again when the
/// range was written in an earlier frame.
unsafe fn frame_range(gl: &crate::DebugGl, block: &mut BlockBuffer) -> Option<StreamRange> {
    match block.range {
        Some(range) if range.frame == crate::stream_buffer::frame() => Some(range),
        _ => stream_block(gl, block, None),
    }
}

unsafe fn bind_block(gl: &crate::DebugGl, block: &mut BlockBuffer) {
    let Some(range) = frame_range(gl, block) else {
        return;
    };
    gl.bind_buffer_range(
        glow::UNIFORM_BUFFER,
        block.binding,
        Some(range.buffer),
        range.offset,
        block.size as i32,
    );
}

/// Write `bytes` (the block's own when `None`) into a fresh range of the
/// uniform stream and record it as the block's range.
unsafe fn stream_block(
    gl: &crate::DebugGl,
    block: &mut BlockBuffer,
    bytes: Option<&[u8]>,
) -> Option<StreamRange> {
    match crate::stream_buffer::upload(gl, Stream::Uniform, bytes.unwrap_or(&block.bytes)) {
        Ok(range) => {
            block.range = Some(range);
            Some(range)
        }
        Err(e) => {
            ::log::error!(target: "SHADER", "uniform block `{}` of `{}`: {e}", block.name, block.shader_key);
            None
        }
    }
}

pub(crate) fn mark_program_drawn(program: glow::Program) {
    let handle = BLOCK_PROGRAMS.with(|m| m.borrow().get(&program).copied());
    let Some(handle) = handle else { return };
    BLOCKS.with(|b| {
        if let Some(blocks) = b.borrow_mut().get_mut(&handle) {
            for block in blocks {
                block.drawn = true;
            }
        }
    });
}

pub(crate) fn report_unfilled_blocks() {
    BLOCKS.with(|b| {
        for blocks in b.borrow_mut().values_mut() {
            for block in blocks.iter_mut() {
                if block.uploaded || !block.drawn || block.reported {
                    continue;
                }
                block.reported = true;
                ::log::error!(
                    target: "SHADER",
                    "shader `{}` was drawn with but nothing ever filled its uniform block `{}` \
                     (binding {}) -- it is reading zeroes. That group needs a `set_uniform_block` \
                     / `pass_block` / `batch_block` / `mesh_block` call site; a by-name setter \
                     cannot reach a block member.",
                    block.shader_key,
                    block.name,
                    block.binding
                );
            }
        }
    });
}

/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_unfilled_uniform_block_count() -> c_int {
    BLOCKS.with(|b| {
        b.borrow()
            .values()
            .flatten()
            .filter(|x| x.drawn && !x.uploaded)
            .count() as c_int
    })
}

/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_shader_bind_uniform_block(
    handle: u64,
    shader_key: *const c_char,
    block_name: *const c_char,
    binding: c_int,
    size: c_int,
    fingerprint: u64,
) -> bool {
    if block_name.is_null() || shader_key.is_null() || size <= 0 {
        ::log::error!(target: "SHADER", "bind_uniform_block: null name or non-positive size {size}");
        return false;
    }
    let name = unsafe { CStr::from_ptr(block_name) }
        .to_string_lossy()
        .into_owned();
    let key = unsafe { CStr::from_ptr(shader_key) }
        .to_string_lossy()
        .into_owned();
    let Some(program) = program_of(handle) else {
        ::log::error!(target: "SHADER", "bind_uniform_block: no shader {handle}");
        return false;
    };

    with_gl(|gl| unsafe {
        let Some(index) = gl.get_uniform_block_index(program, &name) else {
            ::log::error!(
                target: "SHADER",
                "bind_uniform_block: shader {handle} declares no uniform block `{name}`"
            );
            return false;
        };
        gl.uniform_block_binding(program, index, binding as u32);

        // Unwritten members and tails must start at zero on every driver.
        let bytes = vec![0u8; size as usize];

        BLOCK_PROGRAMS.with(|m| m.borrow_mut().insert(program, handle));
        BLOCKS.with(|b| {
            let mut map = b.borrow_mut();
            let blocks = map.entry(handle).or_default();
            // Bindings are sparse GL indices; reload replaces and re-arms the block.
            let fresh = BlockBuffer {
                program,
                range: None,
                name: name.clone(),
                shader_key: key.clone(),
                size: size as usize,
                bytes,
                binding: binding as u32,
                fingerprint,
                uploaded: false,
                drawn: false,
                reported: false,
                members: Vec::new(),
            };
            match blocks.iter_mut().find(|x| x.binding == fresh.binding) {
                Some(existing) => *existing = fresh,
                None => blocks.push(fresh),
            }
        });
        true
    })
    // No GL context is a failure to report, not a quiet success.
    .unwrap_or_else(|| {
        ::log::error!(target: "SHADER", "bind_uniform_block: no GL context");
        false
    })
}

/// Upload `size` bytes into `handle`'s block. The bytes are the compiler's
/// std140 packing; this call does not interpret them.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_shader_upload_uniform_block(
    handle: u64,
    data: *const u8,
    size: c_int,
    fingerprint: u64,
) -> bool {
    if data.is_null() || size <= 0 {
        ::log::error!(target: "SHADER", "upload_uniform_block: null data or non-positive size {size}");
        return false;
    }
    BLOCKS.with(|b| {
        let mut map = b.borrow_mut();
        // Selected by FINGERPRINT, not by handle: a program can have several
        // blocks, and the packed bytes say which layout they are.
        let Some(block) = map.get_mut(&handle).and_then(|bs| {
            match bs.iter().position(|x| x.fingerprint == fingerprint) {
                Some(i) => bs.get_mut(i),
                // No match still selects one, so the mismatch is reported as the
                // layout error it is rather than as a missing block.
                None => bs.first_mut(),
            }
        }) else {
            ::log::error!(
                target: "SHADER",
                "upload_uniform_block: shader {handle} has no bound block -- \
                 stone_shader_bind_uniform_block must run at registration"
            );
            return false;
        };
        // An undersized upload leaves stale bytes; an oversized one is invalid.
        if size as usize != block.size {
            ::log::error!(
                target: "SHADER",
                "upload_uniform_block: shader {handle} upload of {size} bytes does not match its \
                 {} byte block",
                block.size
            );
            return false;
        }
        // Same size is not the same layout: `{float, Vec2}` and `{Vec2, float}`
        // agree on size, on both member offsets and on leaf count, differing
        // only in the kind at each offset.
        if fingerprint != block.fingerprint {
            ::log::error!(
                target: "SHADER",
                "upload_uniform_block: shader {handle} was bound for layout {:#x} but the packed \
                 bytes are laid out as {fingerprint:#x} -- the uniform group does not belong to \
                 this shader",
                block.fingerprint
            );
            return false;
        }
        let bytes = unsafe { std::slice::from_raw_parts(data, size as usize) };
        // Never overwrite a range still read by a queued draw: GL drivers can
        // stall on the write or give the queued draw the replacement storage.
        let current = crate::CURRENT_SHADER.with(|c| *c.borrow()) == Some(block.program);
        let ok = with_gl(|gl| unsafe {
            let streamed = stream_block(gl, block, Some(bytes)).is_some();
            if streamed && current {
                bind_block(gl, block);
            }
            streamed
        })
        .unwrap_or(false);
        if ok {
            block.bytes.copy_from_slice(bytes);
        }
        if ok && crate::ui_submission_probe::enabled() {
            crate::ui_submission_probe::block_uploaded(handle, fingerprint, bytes);
        }
        block.uploaded |= ok;
        ok
    })
}

/// Copy the last successfully uploaded bytes without waiting on the GPU.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_shader_snapshot_uniform_block(
    handle: u64,
    block_name: *const c_char,
    dst: *mut u8,
    size: c_int,
) -> bool {
    block_bytes_into(handle, block_name, dst, size, |block, out| {
        out.copy_from_slice(&block.bytes);
        true
    })
}

/// Read exactly the block's declared size. Only desktop GL exposes the bytes.
/// rock-ffi stone-rendering
#[no_mangle]
pub extern "C" fn stone_shader_read_uniform_block(
    handle: u64,
    block_name: *const c_char,
    dst: *mut u8,
    size: c_int,
) -> bool {
    block_bytes_into(handle, block_name, dst, size, |block, out| {
        with_gl(|gl| unsafe {
            let Some(range) = frame_range(gl, block) else {
                return false;
            };
            let previous = std::num::NonZeroU32::new(
                gl.get_parameter_i32(glow::UNIFORM_BUFFER_BINDING) as u32,
            )
            .map(glow::NativeBuffer);
            gl.bind_buffer(glow::UNIFORM_BUFFER, Some(range.buffer));
            gl.get_buffer_sub_data(glow::UNIFORM_BUFFER, range.offset, out);
            gl.bind_buffer(glow::UNIFORM_BUFFER, previous);
            true
        })
        .unwrap_or(false)
    })
}

fn block_bytes_into(
    handle: u64,
    block_name: *const c_char,
    dst: *mut u8,
    size: c_int,
    copy: impl FnOnce(&mut BlockBuffer, &mut [u8]) -> bool,
) -> bool {
    if block_name.is_null() || dst.is_null() || size <= 0 {
        ::log::error!(target: "SHADER", "uniform_block_bytes: null name or destination, or non-positive size {size}");
        return false;
    }
    let name = unsafe { CStr::from_ptr(block_name) }.to_string_lossy();
    BLOCKS.with(|b| {
        let mut map = b.borrow_mut();
        let Some(block) = map.get_mut(&handle).and_then(|bs| bs.iter_mut().find(|x| x.name == name)) else {
            ::log::error!(target: "SHADER", "uniform_block_bytes: shader {handle} has no bound block `{name}`");
            return false;
        };
        if size as usize != block.size {
            ::log::error!(target: "SHADER", "uniform_block_bytes: {size} byte read of shader {handle}'s {} byte block `{name}`", block.size);
            return false;
        }
        let out = unsafe { std::slice::from_raw_parts_mut(dst, size as usize) };
        copy(block, out)
    })
}

/// Check the driver layout against NUL-separated names and compiler offsets.
/// GL may omit inactive members and size only the active portion, so this
/// check skips absent members and treats the queried size as an upper bound.
/// rock-ffi stone-rendering private
#[no_mangle]
pub extern "C" fn stone_shader_check_uniform_block_layout(
    handle: u64,
    block_name: *const c_char,
    names: *const c_char,
    names_len: c_int,
    offsets: *const c_int,
    count: c_int,
    plan_size: c_int,
) -> bool {
    if block_name.is_null() || names.is_null() || offsets.is_null() || count <= 0 || names_len <= 0
    {
        ::log::error!(target: "SHADER", "check_uniform_block_layout: malformed arguments");
        return false;
    }
    let bname = unsafe { CStr::from_ptr(block_name) }
        .to_string_lossy()
        .into_owned();
    let blob = unsafe { std::slice::from_raw_parts(names as *const u8, names_len as usize) };
    let member_names: Vec<String> = blob
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    let plan_offsets = unsafe { std::slice::from_raw_parts(offsets, count as usize) };

    if member_names.len() != count as usize {
        ::log::error!(
            target: "SHADER",
            "check_uniform_block_layout: {} names for {count} offsets",
            member_names.len()
        );
        return false;
    }
    let Some(program) = program_of(handle) else {
        ::log::error!(target: "SHADER", "check_uniform_block_layout: no shader {handle}");
        return false;
    };

    with_gl(|gl| unsafe {
        let Some(bindex) = gl.get_uniform_block_index(program, &bname) else {
            ::log::error!(
                target: "SHADER",
                "check_uniform_block_layout: shader {handle} declares no block `{bname}`"
            );
            return false;
        };
        let data_size = gl.get_active_uniform_block_parameter_i32(
            program,
            bindex,
            glow::UNIFORM_BLOCK_DATA_SIZE,
        );
        if data_size > plan_size {
            ::log::error!(
                target: "SHADER",
                "check_uniform_block_layout: `{bname}` needs {data_size} bytes, the plan allocates {plan_size}"
            );
            return false;
        }

        let refs: Vec<&str> = member_names.iter().map(|s| s.as_str()).collect();
        let indices = gl.get_uniform_indices(program, &refs);
        let active: Vec<u32> = indices.iter().filter_map(|i| *i).collect();
        if active.is_empty() {
            ::log::error!(
                target: "SHADER",
                "check_uniform_block_layout: `{bname}` has no active members -- nothing to verify"
            );
            return false;
        }
        let driver_offsets =
            gl.get_active_uniforms_parameter(program, &active, glow::UNIFORM_OFFSET);

        let mut ok = true;
        let mut a = 0usize;
        for (i, idx) in indices.iter().enumerate() {
            if idx.is_none() {
                continue; // inactive: optimized out, nothing to compare
            }
            let driver = driver_offsets[a];
            a += 1;
            if driver != plan_offsets[i] {
                ::log::error!(
                    target: "SHADER",
                    "check_uniform_block_layout: `{bname}.{}` is at {driver} on this driver, \
                     the plan packs it at {}",
                    member_names[i],
                    plan_offsets[i]
                );
                ok = false;
            }
        }
        if ok {
            BLOCKS.with(|b| {
                if let Some(block) = b
                    .borrow_mut()
                    .get_mut(&handle)
                    .and_then(|bs| bs.iter_mut().find(|x| x.name == bname))
                {
                    if block.members.is_empty() {
                        block.members = member_names
                            .iter()
                            .zip(plan_offsets.iter())
                            .map(|(m, &off)| BlockMember {
                                name: m.strip_suffix("[0]").unwrap_or(m).to_string(),
                                offset: off,
                                kind: -1,
                                capacity: 0,
                                stride: 0,
                            })
                            .collect();
                    }
                }
            });
        }
        ok
    })
    .unwrap_or_else(|| {
        ::log::error!(target: "SHADER", "check_uniform_block_layout: no GL context");
        false
    })
}

/// Record one uniform-block member's std140 layout for native-renderer queries.
/// rock-ffi stone-rendering private
#[no_mangle]
pub extern "C" fn stone_shader_register_block_member(
    handle: u64,
    binding: c_int,
    name: *const c_char,
    offset: c_int,
    kind: c_int,
    capacity: c_int,
    stride: c_int,
) -> bool {
    if handle == 0 || name.is_null() || binding < 0 || offset < 0 {
        return false;
    }
    let Ok(member_name) = unsafe { CStr::from_ptr(name) }.to_str() else {
        return false;
    };
    if member_name.is_empty() {
        return false;
    }
    BLOCKS.with(|b| {
        let mut map = b.borrow_mut();
        let Some(block) = map
            .get_mut(&handle)
            .and_then(|bs| bs.iter_mut().find(|x| x.binding == binding as u32))
        else {
            return false;
        };
        let entry = BlockMember {
            name: member_name.to_string(),
            offset,
            kind,
            capacity,
            stride,
        };
        if let Some(existing) = block.members.iter_mut().find(|m| m.name == member_name) {
            *existing = entry;
        } else {
            block.members.push(entry);
        }
        true
    })
}

pub(crate) fn block_count(handle: u64) -> Option<usize> {
    program_of(handle)?;
    Some(BLOCKS.with(|b| b.borrow().get(&handle).map_or(0, Vec::len)))
}

pub(crate) fn find_block_by_binding(handle: u64, binding: i32) -> i32 {
    if binding < 0 {
        return -1;
    }
    BLOCKS.with(|b| {
        b.borrow()
            .get(&handle)
            .and_then(|bs| bs.iter().position(|x| x.binding == binding as u32))
            .map_or(-1, |idx| idx as i32)
    })
}

pub(crate) fn find_block_by_name(handle: u64, name: &str) -> i32 {
    BLOCKS.with(|b| {
        b.borrow()
            .get(&handle)
            .and_then(|bs| bs.iter().position(|x| x.name == name))
            .map_or(-1, |idx| idx as i32)
    })
}

pub(crate) fn block_info(handle: u64, index: usize) -> Option<(i32, i32, i32, u64, String)> {
    BLOCKS.with(|b| {
        b.borrow()
            .get(&handle)
            .and_then(|bs| bs.get(index))
            .map(|block| {
                (
                    block.binding as i32,
                    block.size as i32,
                    block.members.len() as i32,
                    block.fingerprint,
                    block.name.clone(),
                )
            })
    })
}

pub(crate) fn block_member_info(
    handle: u64,
    block_index: usize,
    member_index: usize,
) -> Option<(i32, i32, i32, i32, String)> {
    BLOCKS.with(|b| {
        b.borrow()
            .get(&handle)
            .and_then(|bs| bs.get(block_index))
            .and_then(|block| block.members.get(member_index))
            .map(|m| (m.offset, m.kind, m.capacity, m.stride, m.name.clone()))
    })
}

pub(crate) fn block_member_offset(handle: u64, block_index: usize, member_name: &str) -> i32 {
    BLOCKS.with(|b| {
        b.borrow()
            .get(&handle)
            .and_then(|bs| bs.get(block_index))
            .and_then(|block| block.members.iter().find(|m| m.name == member_name))
            .map_or(-1, |m| m.offset)
    })
}
