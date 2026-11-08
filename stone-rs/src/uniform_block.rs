//! Uniform blocks: the slot-bound replacement for the by-name uniform setters.
//!
//! A block is registered once (`stone_shader_bind_uniform_block`) and fed whole
//! (`stone_shader_upload_uniform_block`). Each upload lands in its own range of
//! the per-frame uniform stream (stream_buffer.rs), and the range is bound at
//! the block's binding per program switch, since binding indices are global
//! state. No
//! `glGetUniformLocation` per field and no name after link time -- which is the
//! point, since WGSL/Vulkan/Metal have no CPU-visible uniform name to look up.
//!
//! The bytes are packed by the compiler from its std140 `UniformBlockPlan`.
//! Nothing here knows that layout; this file moves an opaque span of bytes and
//! then asks the driver whether it agrees
//! (`stone_shader_check_uniform_block_layout`), which is the only independent
//! std140 implementation in reach.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{c_char, c_int, CStr};

use crate::stream_buffer::{Stream, StreamRange};
use crate::{with_gl, SHADERS};

/// One of a program's uniform blocks.
struct BlockBuffer {
    /// The program the block belongs to. An upload binds its range at once
    /// only while this program is current; otherwise the next switch binds it.
    program: glow::Program,
    /// Where `bytes` were last streamed. Readable by draws of `range.frame`
    /// only: a later frame reuses that region, so binding the block in a later
    /// frame streams `bytes` again ([`bind_block`]).
    range: Option<StreamRange>,
    /// The GLSL block name and the Rock-side shader key, for diagnostics only.
    /// Binding is by slot. The key is needed because a block name alone does not
    /// identify a block: `SpriteFragU` is declared both by the sprite batch and
    /// by any custom pipeline that reuses it.
    name: String,
    shader_key: String,
    size: usize,
    /// Last bytes uploaded, including zero initialization. Render scopes
    /// snapshot these without synchronizing the GPU.
    bytes: Vec<u8>,
    /// Whether anything has ever packed bytes into this block, and whether a
    /// DRAW has ever issued against its program. A block drawn with while
    /// nothing fills it reads zeroes in the shader forever, silently.
    ///
    /// `drawn` is stamped by [`mark_program_drawn`] at each real draw, with the
    /// program that draw goes through -- see there for why an ambient
    /// "currently bound" tracker misattributes. Both flags are sticky, so
    /// [`report_unfilled_blocks`] fires only for a block never filled across the
    /// WHOLE run, never for the ordinary set-then-draw ordering within a frame.
    uploaded: bool,
    drawn: bool,
    /// Reported once. This is checked every frame; a broken block is broken
    /// every frame.
    reported: bool,
    /// The binding point this block was pointed at. Two programs may both use
    /// index 0, which is exactly why a range cannot be bound there once and
    /// left: `glBindBufferRange` is GLOBAL state, so the last program registered
    /// would own the index for all of them. [`bind_block_for_program`] re-binds
    /// every block of a program whenever that program goes current.
    binding: u32,
    /// The compiler's `UniformBlockPlan::fingerprint` for the layout this
    /// block was sized and bound for. An upload carries its own, from the plan
    /// the PACKER used, and the two must agree: the packer derives offsets from
    /// the uniform type while the destination comes from the shader, so
    /// `set_uniform_block(blur, HeatShimmerU { .. })` is well typed on both
    /// sides and writes one layout's bytes into another's block.
    fingerprint: u64,
}

thread_local! {
    /// Per program, its blocks, one per binding. A LIST, not one entry: an A2
    /// pipeline declares one block per STAGE, so keying by handle alone made
    /// the second registration evict the first.
    static BLOCKS: RefCell<HashMap<u64, Vec<BlockBuffer>>> = RefCell::new(HashMap::new());
    /// Block-owning programs only, so a draw through a program with no blocks
    /// costs one miss. Populated at registration, where both the program object
    /// and its shader handle are in hand.
    static BLOCK_PROGRAMS: RefCell<HashMap<glow::Program, u64>> = RefCell::new(HashMap::new());
}

/// Test-only injection: drop the `uploaded` stamp so every block looks unfilled,
/// which is what a real missing call site produces. Mirrors
/// `STONE_SHADER_FORCE_FAIL` -- the point is to drive the REAL reporting path
/// rather than hand-fake a count, so `uniform_block_gate.rs` can assert that a
/// bounded run actually exits nonzero.
///
/// Read once from `STONE_UNIFORM_BLOCK_FORCE_UNFILLED`.
fn force_unfilled_blocks() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("STONE_UNIFORM_BLOCK_FORCE_UNFILLED").as_deref() == Ok("1"))
}

/// Test-only injection: refuse to bind the block with this NAME. Scoped by name
/// rather than by a call counter so one sim fixture can fail its own block while
/// every other shader in the process registers normally -- a global counter
/// would break the whole frame and the test would prove nothing about cleanup.
///
/// The point is to reach the partial-failure path for real: a candidate that
/// binds its engine block, allocates that buffer, and only then fails. Sibling
/// of `STONE_UNIFORM_BLOCK_FORCE_UNFILLED`.
fn fail_bind_for_block() -> Option<&'static str> {
    static NAME: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    NAME.get_or_init(|| std::env::var("STONE_UNIFORM_BLOCK_FAIL_NAME").ok())
        .as_deref()
}

/// Registered blocks across every program. The leak oracle: a rejected
/// candidate that had already bound a block must leave this where it found it.
#[no_mangle]
pub extern "C" fn stone_uniform_block_buffer_count() -> c_int {
    BLOCKS.with(|b| b.borrow().values().map(|v| v.len()).sum::<usize>() as c_int)
}

/// Programs the draw-attribution map still knows about. A SECOND view, because
/// the block count above cannot see this one: `BLOCKS` and `BLOCK_PROGRAMS` are
/// dropped by the same call but are separate maps, and a stale entry here is not
/// a leaked buffer -- it is a destroyed program's id still claiming draws, which
/// misattributes as soon as GL hands that id out again.
#[no_mangle]
pub extern "C" fn stone_block_program_count() -> c_int {
    BLOCK_PROGRAMS.with(|m| m.borrow().len() as c_int)
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

/// The binding state GL itself reports, as JSON, for the on-demand uniform
/// block dump (`uniform_block_dump_arm`, stone/engine/rendering/uniform_block_dump.rock).
/// Diagnostic only: it queries GL and reads buffers back, so nothing calls it
/// unless a dump was requested.
///
/// `handle == 0` describes every block-owning program and the buffer bound at
/// each indexed binding point; a non-zero handle describes that program's
/// blocks at this instant, which the dump takes immediately before a draw.
/// Per block: the binding the registry recorded, the binding GL reports for the
/// program (`GL_UNIFORM_BLOCK_BINDING`), the block's stream range (buffer,
/// offset, and whether it was written this frame), the buffer range actually
/// bound at that index, the last uploaded bytes, and -- on desktop GL -- the
/// bytes at the BOUND range. A bound range that is not the block's own, or GPU
/// bytes that differ from the upload, is the defect class the dump exists to
/// catch. The pointer is valid until the next call.
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
        // (buffer, offset, size) at an indexed binding point; size 0 is a
        // whole-buffer `glBindBufferBase`.
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
        // A foreign range at the index may be shorter than this block, so the
        // read is clamped to the buffer.
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
                            // Read the BOUND range, not the block's own: a draw
                            // reads whatever is at the index.
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

/// Drop every bookkeeping entry belonging to `handle`. Its ranges stay in the
/// stream, which reclaims them by frame.
///
/// Called from `stone_destroy_shader`, which owns a program's whole lifetime.
/// Deleting the program alone left the blocks registered and the
/// program -> handle entry live, so a rejected candidate leaked every block it
/// had already bound -- once per reload, for the process lifetime --
/// and a later program handed the same GL id inherited the stale attribution
/// that `mark_program_drawn` reads.
pub(crate) fn destroy_blocks_for_shader(handle: u64, program: glow::Program) {
    BLOCKS.with(|b| b.borrow_mut().remove(&handle));
    BLOCK_PROGRAMS.with(|m| m.borrow_mut().remove(&program));
}

fn program_of(handle: u64) -> Option<glow::Program> {
    SHADERS.with(|s| s.borrow().get(&handle).map(|shader| shader.program))
}

/// Point each uniform binding of `handle` at its block's range. Called from
/// `stone_set_shader` on every program switch, because `glBindBufferRange`
/// binds to a GLOBAL binding index rather than to the program: binding once at
/// registration leaves whichever program registered LAST owning the index, and
/// every other program then reads that program's bytes. The block belongs to
/// the program, so it is re-bound wherever the program goes current.
pub(crate) fn bind_block_for_program(gl: &crate::DebugGl, handle: u64) {
    BLOCKS.with(|b| {
        if let Some(blocks) = b.borrow_mut().get_mut(&handle) {
            let frame = crate::stream_buffer::frame();
            for block in blocks {
                unsafe { bind_block(gl, block, frame) };
            }
        }
    });
}

/// Re-bind the current program's blocks for a new frame. Called at the frame
/// boundary once the streams have advanced: the ranges bound now were written
/// for the frame that ended, and only that frame's fence covers them.
pub(crate) fn rebind_current_program(gl: &crate::DebugGl) {
    let Some(program) = crate::CURRENT_SHADER.with(|c| *c.borrow()) else {
        return;
    };
    let Some(handle) = BLOCK_PROGRAMS.with(|m| m.borrow().get(&program).copied()) else {
        return;
    };
    bind_block_for_program(gl, handle);
}

/// Bind `block`'s range at its binding, streaming its bytes again first when
/// the range was written in an earlier frame.
unsafe fn bind_block(gl: &crate::DebugGl, block: &mut BlockBuffer, frame: u64) {
    let range = match block.range {
        Some(range) if range.frame == frame => range,
        _ => match stream_block(gl, block, None) {
            Some(range) => range,
            None => return,
        },
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

/// Stamp `program`'s blocks as drawn. Called at each real draw WITH the program
/// that draw goes through.
///
/// Deliberately not an ambient "currently bound" tracker, which misattributes
/// three ways: `track_text_draw_call` runs at the top of
/// `draw_text_line_internal`, before the text program is bound and before the
/// draw (so it credited whichever program came before, and fired even when the
/// font was missing and nothing drew); `stone_set_shader(0)` leaves nothing
/// bound, so a later unrelated draw would credit the stale entry; and
/// `stone_sprite_draw_texture` falls back to a passed handle when no program is
/// current, which an ambient tracker cannot see at all.
pub(crate) fn mark_program_drawn(program: glow::Program) {
    // One hash lookup on a map that holds ONLY block-owning programs, so the
    // overwhelmingly common case -- a draw through a program with no blocks --
    // misses immediately. This runs per draw call; scanning the shader registry
    // here would put an O(shaders) walk on every draw in the frame.
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

/// Report every block a draw has issued against but which nothing has ever
/// filled. Called at the frame boundary.
///
/// A block member has no `glGetUniformLocation`, so a by-name setter aimed at
/// one writes nothing and the shader reads zeroes -- which looks like wrong game
/// state, not like a wiring bug, and so survives every pixel oracle that does
/// not happen to sample the affected pixels. This is the mechanism that makes
/// the class loud: the engine knows every registered block, so a block never
/// uploaded is decidable without anyone looking at the screen.
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

/// How many blocks were drawn with while nothing filled them. The gate's
/// enforcement point, not only its diagnostic: `check_unfilled_uniform_blocks`
/// (stone/engine/rendering/uniform_block.rock) fails a sim on a non-zero count,
/// and `rock frame`'s bounded run exits non-zero.
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

/// Register `size` zero bytes for `handle`'s uniform block and point the
/// program's block at `binding`.
///
/// Returns false and logs when the program declares no block of that name --
/// never silently, because the symptom of a missed bind is a shader reading
/// zeroes forever, which looks like wrong game state rather than a wiring bug.
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

    if fail_bind_for_block() == Some(name.as_str()) {
        ::log::error!(
            target: "SHADER",
            "bind_uniform_block: refusing `{name}` (STONE_UNIFORM_BLOCK_FAIL_NAME)"
        );
        return false;
    }

    with_gl(|gl| unsafe {
        let Some(index) = gl.get_uniform_block_index(program, &name) else {
            ::log::error!(
                target: "SHADER",
                "bind_uniform_block: shader {handle} declares no uniform block `{name}`"
            );
            return false;
        };
        gl.uniform_block_binding(program, index, binding as u32);

        // Zero bytes, streamed when the block is first bound. A block member
        // the packer never writes -- an inactive field, a stale tail after a
        // hot reload shrinks the plan, or a whole block nothing fills -- must
        // read 0 rather than whatever the driver handed back, or the same
        // source renders differently per driver and per run.
        let bytes = vec![0u8; size as usize];
        // Deliberately NOT bound here: see `binding` above.

        BLOCK_PROGRAMS.with(|m| m.borrow_mut().insert(program, handle));
        BLOCKS.with(|b| {
            let mut map = b.borrow_mut();
            let blocks = map.entry(handle).or_default();
            // A re-register (hot reload) replaces the block at this BINDING;
            // the old one would otherwise leak for the process lifetime.
            // A binding is a GL index, not a position in this list: a
            // program's bindings need not run densely from 0.
            // A fresh block has never been filled, so a hot reload re-arms the
            // unfilled check rather than inheriting the old block's history.
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
        // EQUALITY, not a ceiling. Overrunning is UB in GL, but an undersized
        // upload is equally a mismatch: it leaves the tail holding the previous
        // frame's bytes, which reads as a stale uniform rather than as a wiring
        // bug. The sizes come from two different compiler artifacts (the plan at
        // pack time, the plan at registration time) that a stale hot reload can
        // desynchronize.
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
        // A fresh stream range, never storage a queued draw reads: an in-place
        // `glBufferSubData` into such storage makes Apple's GL-on-Metal flush,
        // resubmit and wait (~139 uploads per MAG frame put half the main
        // thread in that stall), and re-specifying one buffer per block with
        // `glBufferData` let NVIDIA 595.104 serve a draw the storage its own
        // upload replaced (#883).
        let current = crate::CURRENT_SHADER.with(|c| *c.borrow()) == Some(block.program);
        let ok = with_gl(|gl| unsafe {
            let Some(range) = stream_block(gl, block, Some(bytes)) else {
                return false;
            };
            if current {
                gl.bind_buffer_range(
                    glow::UNIFORM_BUFFER,
                    block.binding,
                    Some(range.buffer),
                    range.offset,
                    size,
                );
            }
            true
        })
        .unwrap_or(false);
        if ok {
            block.bytes.copy_from_slice(bytes);
        }
        if ok && crate::ui_submission_probe::enabled() {
            crate::ui_submission_probe::block_uploaded(handle, fingerprint, bytes);
        }
        block.uploaded |= ok && !force_unfilled_blocks();
        ok
    })
}

/// Copy the last successfully uploaded bytes without waiting on the GPU.
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

/// Read actual GPU bytes for independent validation of uploads and snapshots:
/// the block's range in this frame, as a draw would bind it (streamed again
/// first when it was written in an earlier frame).
#[no_mangle]
pub extern "C" fn stone_shader_read_uniform_block(
    handle: u64,
    block_name: *const c_char,
    dst: *mut u8,
    size: c_int,
) -> bool {
    block_bytes_into(handle, block_name, dst, size, |block, out| {
        with_gl(|gl| unsafe {
            let range = match block.range {
                Some(range) if range.frame == crate::stream_buffer::frame() => range,
                _ => match stream_block(gl, block, None) {
                    Some(range) => range,
                    None => return false,
                },
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

/// Assert the driver's own std140 layout against the compiler's plan.
///
/// `names` is a NUL-separated run of `count` member names; `offsets` is the
/// compiler's byte offset for each. A mismatch means the shader is reading
/// different bytes than the packer is writing -- silent garbage pixels -- so it
/// is reported eagerly at registration rather than discovered by looking at the
/// screen.
///
/// **Only ACTIVE members can be checked.** GL is free to optimize an unused
/// block member away entirely, and then reports no index for it; such a member
/// is skipped, not failed. So this is a partial oracle by construction, and
/// full coverage needs a shader that reads every field. For the same reason the
/// block's queried DATA_SIZE is compared as an upper bound (the driver may size
/// for the active members only), not for equality.
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
        ok
    })
    .unwrap_or_else(|| {
        ::log::error!(target: "SHADER", "check_uniform_block_layout: no GL context");
        false
    })
}
