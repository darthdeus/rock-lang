//! Async pixel readback via double-buffered Pixel Pack Buffers.
//!
//! A "slot" owns 2 PBOs in a ping-pong. Each `read` call:
//!   1. Submits glReadPixels into pbos[write_idx]. With a
//!      PBO bound as PIXEL_PACK_BUFFER this is async — the driver queues a
//!      DMA after prior render commands and returns immediately.
//!   2. Maps pbos[1 - write_idx] (filled by last frame's submit), copies out
//!      the bytes, unmaps. By now the DMA is complete; the map doesn't stall.
//!   3. Swaps write_idx.
//!
//! `stone_pixel_readback_poll_id` is the non-blocking read. A polled slot
//! keeps up to POLL_RING_DEPTH fenced reads in flight and a poll takes the
//! newest one whose fence has signaled. On a GPU-bound machine the blocking
//! map in step 2 waits for the GPU to drain the frame that wrote the PBO,
//! which serializes CPU and GPU (about 40% of the frame on a Steam Deck).
//!
//! The PBO stores no coordinates; the slot handle is the caller's persistent
//! identity across frames. Two call sites sharing one slot will stomp each
//! other's in-flight reads — one slot per logical reader.
//!

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::ffi::{c_float, c_int};
use std::num::NonZeroU32;
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::canvas::{current_fbo, lookup_canvas};
use crate::handles::Class;
use crate::with_gl;

#[derive(Clone, Copy)]
enum PixelFmt {
    F32,
    Bytes,
    Ints,
}

#[derive(Clone, Copy)]
enum Channel {
    Color,
    Depth,
}

#[derive(Clone, Copy)]
enum ReadShape {
    Pixel,
    NearestId { max_side: i32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReadRect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

impl Channel {
    fn bytes(self) -> usize {
        match self {
            Channel::Color => 16, // RGBA32F
            Channel::Depth => 4,  // DEPTH_COMPONENT f32
        }
    }

    fn gl_format(self) -> u32 {
        match self {
            Channel::Color => glow::RGBA,
            Channel::Depth => glow::DEPTH_COMPONENT,
        }
    }

    fn sentinel(self) -> f32 {
        match self {
            Channel::Color => 0.0,
            Channel::Depth => 1.0, // far plane
        }
    }
}

// Two-PBO ping-pong: staging (newest submit, DMA may still be in flight)
// and ready (prior submit, DMA complete, awaiting consume). submit()
// promotes staging → ready before writing a new staging, which gives
// consume() a value at least one frame old regardless of whether submit
// and consume happen at the same call site or are split across update
// and draw phases.
//
// A 3-PBO ring did not reduce submit-time stalls in GPU-bound scenarios, where the
// driver syncs on the source-FBO state rather than PBO back-pressure.
// Reads a polled slot may have in flight. A GPU this many frames behind the
// CPU still answers every frame, once per frame of lag.
const POLL_RING_DEPTH: usize = 4;

// One polled read: the ring PBO it wrote, or none for a skipped submit.
struct InFlight {
    idx: usize,
    fence: Option<glow::Fence>,
    garbage: bool,
    fmt: PixelFmt,
    rect: ReadRect,
    center: (i32, i32),
}

struct Slot {
    // POLL_RING_DEPTH buffers; the blocking ping-pong uses the first two.
    pbos: Vec<glow::Buffer>,
    ready_idx: usize,
    staging_idx: usize,
    ready_primed: bool,
    staging_primed: bool,
    ready_garbage: bool,
    staging_garbage: bool,
    // What the corresponding PBO holds: f32 (float canvas / depth), an RGBA8
    // byte pixel (color read from a downgraded non-float canvas -- the only
    // combo GLES allows there), or i32 ids (an R32I canvas, read as
    // RGBA_INTEGER/INT). consume() converts to the f32 return.
    ready_fmt: PixelFmt,
    staging_fmt: PixelFmt,
    ready_rect: ReadRect,
    staging_rect: ReadRect,
    ready_center: (i32, i32),
    staging_center: (i32, i32),
    // Set by the first poll_id: submits then go to `inflight` instead of the
    // ready/staging ping-pong.
    polled: bool,
    inflight: VecDeque<InFlight>,
    // The pixel the last poll_id answer was submitted for.
    answer_center: (i32, i32),
    channel: Channel,
    shape: ReadShape,
    bytes: usize,
}

const CANVAS_CAPTURE_MAX_SIDE: i32 = 128;
const CANVAS_CAPTURE_MAX_SLOTS: usize = 8;
const CANVAS_CAPTURE_MAX_BYTES: usize = CANVAS_CAPTURE_MAX_SLOTS
    * CANVAS_CAPTURE_MAX_SIDE as usize
    * CANVAS_CAPTURE_MAX_SIDE as usize
    * 4;
const CANVAS_CAPTURE_ID_BASE: u64 = 1 << 48;

#[derive(Clone, Copy, PartialEq, Eq)]
enum CanvasCaptureState {
    Pending,
    Ready,
    Failed,
}

struct CanvasCapture {
    pbo: glow::Buffer,
    fence: Option<glow::Fence>,
    width: i32,
    height: i32,
    bytes: usize,
    state: CanvasCaptureState,
}

thread_local! {
    static SLOTS: RefCell<HashMap<u64, Slot>> = RefCell::new(HashMap::new());
    static CANVAS_CAPTURES: RefCell<HashMap<u64, CanvasCapture>> = RefCell::new(HashMap::new());
}

static NEXT_CANVAS_CAPTURE_ID: AtomicU64 = AtomicU64::new(CANVAS_CAPTURE_ID_BASE);

fn nearest_int_id(
    bytes: &[u8],
    width: usize,
    height: usize,
    bytes_per_pixel: usize,
    read_x: i32,
    read_y: i32,
    center_x: i32,
    center_y: i32,
    rows_bottom_up: bool,
) -> i32 {
    let Some(expected) = width
        .checked_mul(height)
        .and_then(|n| n.checked_mul(bytes_per_pixel))
    else {
        return 0;
    };
    if bytes_per_pixel < 4 || bytes.len() < expected {
        return 0;
    }

    let mut nearest = 0;
    let mut nearest_dist = i64::MAX;
    for row in 0..height {
        for col in 0..width {
            let offset = (row * width + col) * bytes_per_pixel;
            let id = i32::from_ne_bytes([
                bytes[offset],
                bytes[offset + 1],
                bytes[offset + 2],
                bytes[offset + 3],
            ]);
            if id <= 0 {
                continue;
            }
            let dx = i64::from(read_x + col as i32 - center_x);
            let top_row = if rows_bottom_up {
                height - 1 - row
            } else {
                row
            };
            let dy = i64::from(read_y + top_row as i32 - center_y);
            let dist = dx * dx + dy * dy;
            if dist < nearest_dist || (dist == nearest_dist && (nearest == 0 || id < nearest)) {
                nearest = id;
                nearest_dist = dist;
            }
        }
    }
    nearest
}

fn centered_read_rect(
    center_x: i32,
    center_y: i32,
    side: i32,
    active_width: i32,
    active_height: i32,
) -> Option<ReadRect> {
    if side <= 0 || active_width <= 0 || active_height <= 0 {
        return None;
    }
    if center_x < 0 || center_y < 0 || center_x >= active_width || center_y >= active_height {
        return None;
    }
    let before = side / 2;
    let after = side - before;
    let x = (center_x - before).max(0);
    let y = (center_y - before).max(0);
    let right = center_x.saturating_add(after).min(active_width);
    let bottom = center_y.saturating_add(after).min(active_height);
    Some(ReadRect {
        x,
        y,
        width: right - x,
        height: bottom - y,
    })
}

fn requested_side(shape: ReadShape, requested: Option<i32>) -> Option<i32> {
    match shape {
        ReadShape::Pixel => {
            if requested.is_none() {
                Some(1)
            } else {
                None
            }
        }
        ReadShape::NearestId { max_side } => {
            let side = requested.unwrap_or(max_side);
            if side > 0 && side <= max_side {
                Some(side)
            } else {
                None
            }
        }
    }
}

#[no_mangle]
pub extern "C" fn stone_read_canvas_nearest_id(
    canvas_handle: u64,
    x: c_int,
    y: c_int,
    side: c_int,
) -> c_int {
    let Some(info) = lookup_canvas(canvas_handle) else {
        return 0;
    };
    if !info.int_format {
        return 0;
    }
    let Some(rect) = centered_read_rect(x, y, side, info.active_w, info.active_h) else {
        return 0;
    };
    let Some(values) = (rect.width as usize)
        .checked_mul(rect.height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
    else {
        return 0;
    };
    let mut pixels = vec![0i32; values];
    with_gl(|gl| unsafe {
        let prev_fbo = current_fbo();
        gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(info.fbo));
        gl.pixel_store_i32(glow::PACK_ALIGNMENT, 1);
        gl.read_pixels(
            rect.x,
            info.active_h - rect.y - rect.height,
            rect.width,
            rect.height,
            glow::RGBA_INTEGER,
            glow::INT,
            glow::PixelPackData::Slice(Some(bytemuck::cast_slice_mut(&mut pixels))),
        );
        gl.bind_framebuffer(glow::FRAMEBUFFER, crate::canvas::resolve_target(prev_fbo));
    });
    nearest_int_id(
        bytemuck::cast_slice(&pixels),
        rect.width as usize,
        rect.height as usize,
        16,
        rect.x,
        rect.y,
        x,
        y,
        true,
    )
}

fn create_slot(channel: Channel, shape: ReadShape) -> Option<Slot> {
    with_gl(|gl| unsafe {
        let mut pbos = Vec::with_capacity(POLL_RING_DEPTH);
        for _ in 0..POLL_RING_DEPTH {
            match gl.create_buffer() {
                Ok(pbo) => pbos.push(pbo),
                Err(_) => {
                    for pbo in pbos {
                        gl.delete_buffer(pbo);
                    }
                    return None;
                }
            }
        }
        let pixels = match shape {
            ReadShape::Pixel => 1,
            ReadShape::NearestId { max_side } => usize::try_from(max_side).ok()?.checked_pow(2)?,
        };
        let bytes = channel.bytes().checked_mul(pixels)?;
        let gl_bytes = i32::try_from(bytes).ok()?;

        // Preallocate the PBOs so the driver can park them in GPU-visible
        // memory. Usage STREAM_READ signals "written by GL, read by CPU once."
        for &pbo in &pbos {
            gl.bind_buffer(glow::PIXEL_PACK_BUFFER, Some(pbo));
            if gl
                .buffer_data_size(glow::PIXEL_PACK_BUFFER, gl_bytes, glow::STREAM_READ)
                .is_err()
            {
                gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
                for pbo in pbos {
                    gl.delete_buffer(pbo);
                }
                return None;
            }
        }
        gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);

        Some(Slot {
            pbos,
            ready_idx: 0,
            staging_idx: 1,
            ready_primed: false,
            staging_primed: false,
            ready_garbage: false,
            staging_garbage: false,
            ready_fmt: PixelFmt::F32,
            staging_fmt: PixelFmt::F32,
            ready_rect: ReadRect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            staging_rect: ReadRect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            ready_center: (0, 0),
            staging_center: (0, 0),
            polled: false,
            inflight: VecDeque::new(),
            answer_center: (0, 0),
            channel,
            shape,
            bytes,
        })
    })
    .flatten()
}

fn readback_fence_ready(status: u32) -> bool {
    matches!(status, glow::ALREADY_SIGNALED | glow::CONDITION_SATISFIED)
}

/// Whether a polled read has landed. `fence_status` is a zero-timeout wait on
/// its fence, None when it has none. Only a pending fence defers: a failed
/// wait maps anyway, because a blocking read is correct and a slot that never
/// answers is not.
fn poll_answer_landed(garbage: bool, fence_status: Option<u32>) -> bool {
    garbage || fence_status != Some(glow::TIMEOUT_EXPIRED)
}

/// How many of the oldest reads have landed. Fences signal in submission
/// order, so the last of them is the newest answer.
fn landed_prefix(landed: impl IntoIterator<Item = bool>) -> usize {
    landed.into_iter().take_while(|&l| l).count()
}

/// A ring PBO no read in flight is using, or None when the ring is full.
fn poll_ring_free_index(in_flight: &[usize]) -> Option<usize> {
    (0..POLL_RING_DEPTH).find(|idx| !in_flight.contains(idx))
}

// Startup may poll without swapping. VMware's Windows driver can keep a
// read queued despite a zero-timeout wait's flush bit; flush explicitly here.
// This submits work without waiting for GPU completion.
fn fence_wait_status(fence: Option<glow::Fence>) -> Option<u32> {
    let fence = fence?;
    with_gl(|gl| unsafe {
        gl.flush();
        gl.client_wait_sync(fence, 0, 0)
    })
}

fn delete_fence(fence: Option<glow::Fence>) {
    if let Some(fence) = fence {
        with_gl(|gl| unsafe { gl.delete_sync(fence) });
    }
}

/// Promote the staging submit to ready. The previous ready (if any and
/// unconsumed) is dropped.
fn promote_staging(slot: &mut Slot) {
    std::mem::swap(&mut slot.ready_idx, &mut slot.staging_idx);
    slot.ready_primed = true;
    slot.ready_garbage = slot.staging_garbage;
    slot.ready_fmt = slot.staging_fmt;
    slot.ready_rect = slot.staging_rect;
    slot.ready_center = slot.staging_center;
    slot.staging_primed = false;
}

fn canvas_capture_size(width: i32, height: i32) -> Option<usize> {
    if width <= 0
        || height <= 0
        || width > CANVAS_CAPTURE_MAX_SIDE
        || height > CANVAS_CAPTURE_MAX_SIDE
    {
        return None;
    }
    (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
}

fn canvas_capture_can_allocate(bytes: usize) -> bool {
    CANVAS_CAPTURES.with(|captures| {
        let captures = captures.borrow();
        if captures.len() >= CANVAS_CAPTURE_MAX_SLOTS {
            return false;
        }
        let used = captures
            .values()
            .try_fold(0usize, |used, capture| used.checked_add(capture.bytes));
        used.is_some_and(|used| {
            used.checked_add(bytes)
                .is_some_and(|total| total <= CANVAS_CAPTURE_MAX_BYTES)
        })
    })
}

fn next_canvas_capture_id() -> Option<u64> {
    NEXT_CANVAS_CAPTURE_ID
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
        .ok()
}

fn framebuffer_from_name(name: i32) -> Option<glow::Framebuffer> {
    u32::try_from(name)
        .ok()
        .and_then(NonZeroU32::new)
        .map(glow::NativeFramebuffer)
}

fn buffer_from_name(name: i32) -> Option<glow::Buffer> {
    u32::try_from(name)
        .ok()
        .and_then(NonZeroU32::new)
        .map(glow::NativeBuffer)
}

unsafe fn copy_rgba_rows_top_down(src: *const u8, dst: *mut u8, width: usize, height: usize) {
    let row = width * 4;
    for y in 0..height {
        let source = src.add((height - 1 - y) * row);
        ptr::copy_nonoverlapping(source, dst.add(y * row), row);
    }
}

fn destroy_canvas_capture(capture: CanvasCapture) {
    with_gl(|gl| unsafe {
        if let Some(fence) = capture.fence {
            gl.delete_sync(fence);
        }
        gl.delete_buffer(capture.pbo);
    });
}

fn drain_canvas_captures() -> Vec<CanvasCapture> {
    CANVAS_CAPTURES.with(|captures| {
        captures
            .borrow_mut()
            .drain()
            .map(|(_, capture)| capture)
            .collect()
    })
}

pub(crate) fn release_all() {
    for capture in drain_canvas_captures() {
        destroy_canvas_capture(capture);
    }
    let ids: Vec<u64> = SLOTS.with(|s| s.borrow().keys().copied().collect());
    for id in ids {
        stone_pixel_readback_destroy(id);
    }
}

/// Queue a full RGBA8 canvas readback into a bounded PBO.
///
/// The caller must pass a resident canvas no larger than 128x128 pixels. The
/// returned id remains pending until its GPU fence signals; poll with
/// `stone_canvas_capture_poll` and release it explicitly afterwards.
#[no_mangle]
pub extern "C" fn stone_canvas_capture_begin(canvas_handle: u64) -> u64 {
    if canvas_handle == 0 {
        return 0;
    }
    let Some(info) = lookup_canvas(canvas_handle) else {
        crate::log_once!(
            "CANVAS",
            "canvas capture requested for unknown handle {}",
            canvas_handle
        );
        return 0;
    };
    if info.int_format {
        crate::log_once!(
            "CANVAS",
            "canvas capture refuses integer canvas {}",
            canvas_handle
        );
        return 0;
    }
    let Some(bytes) = canvas_capture_size(info.width, info.height) else {
        crate::log_once!(
            "CANVAS",
            "canvas capture refuses {}x{} canvas {}; maximum is {}x{}",
            info.width,
            info.height,
            canvas_handle,
            CANVAS_CAPTURE_MAX_SIDE,
            CANVAS_CAPTURE_MAX_SIDE
        );
        return 0;
    };
    if !canvas_capture_can_allocate(bytes) {
        crate::log_once!(
            "CANVAS",
            "canvas capture queue is full ({} slots, {} bytes)",
            CANVAS_CAPTURE_MAX_SLOTS,
            CANVAS_CAPTURE_MAX_BYTES
        );
        return 0;
    }
    let Ok(gl_bytes) = i32::try_from(bytes) else {
        return 0;
    };

    let allocated = with_gl(|gl| unsafe {
        let pbo = match gl.create_buffer() {
            Ok(pbo) => pbo,
            Err(error) => {
                crate::log_once!("CANVAS", "canvas capture PBO allocation failed: {}", error);
                return None;
            }
        };

        let previous_read = gl.get_parameter_i32(glow::READ_FRAMEBUFFER_BINDING);
        let previous_draw = gl.get_parameter_i32(glow::DRAW_FRAMEBUFFER_BINDING);
        let previous_pack = gl.get_parameter_i32(glow::PIXEL_PACK_BUFFER_BINDING);
        let previous_alignment = gl.get_parameter_i32(glow::PACK_ALIGNMENT);
        let previous_row_length = gl.get_parameter_i32(glow::PACK_ROW_LENGTH);
        let previous_skip_rows = gl.get_parameter_i32(glow::PACK_SKIP_ROWS);
        let previous_skip_pixels = gl.get_parameter_i32(glow::PACK_SKIP_PIXELS);

        gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(info.fbo));
        gl.bind_buffer(glow::PIXEL_PACK_BUFFER, Some(pbo));
        if gl
            .buffer_data_size(glow::PIXEL_PACK_BUFFER, gl_bytes, glow::STREAM_READ)
            .is_err()
        {
            gl.bind_buffer(glow::PIXEL_PACK_BUFFER, buffer_from_name(previous_pack));
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, framebuffer_from_name(previous_read));
            gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, framebuffer_from_name(previous_draw));
            gl.delete_buffer(pbo);
            return None;
        }
        gl.pixel_store_i32(glow::PACK_ALIGNMENT, 1);
        gl.pixel_store_i32(glow::PACK_ROW_LENGTH, 0);
        gl.pixel_store_i32(glow::PACK_SKIP_ROWS, 0);
        gl.pixel_store_i32(glow::PACK_SKIP_PIXELS, 0);
        gl.read_pixels(
            0,
            0,
            info.width,
            info.height,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelPackData::BufferOffset(0),
        );
        let fence = gl.fence_sync(glow::SYNC_GPU_COMMANDS_COMPLETE, 0);

        gl.bind_buffer(glow::PIXEL_PACK_BUFFER, buffer_from_name(previous_pack));
        gl.pixel_store_i32(glow::PACK_ALIGNMENT, previous_alignment);
        gl.pixel_store_i32(glow::PACK_ROW_LENGTH, previous_row_length);
        gl.pixel_store_i32(glow::PACK_SKIP_ROWS, previous_skip_rows);
        gl.pixel_store_i32(glow::PACK_SKIP_PIXELS, previous_skip_pixels);
        gl.bind_framebuffer(glow::READ_FRAMEBUFFER, framebuffer_from_name(previous_read));
        gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, framebuffer_from_name(previous_draw));

        match fence {
            Ok(fence) => Some((pbo, fence)),
            Err(error) => {
                crate::log_once!("CANVAS", "canvas capture fence creation failed: {}", error);
                gl.delete_buffer(pbo);
                None
            }
        }
    })
    .flatten();
    let Some((pbo, fence)) = allocated else {
        return 0;
    };
    let Some(id) = next_canvas_capture_id() else {
        destroy_canvas_capture(CanvasCapture {
            pbo,
            fence: Some(fence),
            width: info.width,
            height: info.height,
            bytes,
            state: CanvasCaptureState::Failed,
        });
        return 0;
    };
    CANVAS_CAPTURES.with(|captures| {
        captures.borrow_mut().insert(
            id,
            CanvasCapture {
                pbo,
                fence: Some(fence),
                width: info.width,
                height: info.height,
                bytes,
                state: CanvasCaptureState::Pending,
            },
        );
    });
    id
}

/// Poll a queued canvas capture and copy it top-down into `out` when ready.
/// Returns 0 while its zero-timeout fence wait reports pending, the byte count
/// when copied, or -1 for an unknown, failed, or invalid request.
#[no_mangle]
pub extern "C" fn stone_canvas_capture_poll(id: u64, out: *mut u8, capacity: c_int) -> c_int {
    if id == 0 || out.is_null() || capacity < 0 {
        return -1;
    }
    CANVAS_CAPTURES.with(|captures| {
        let mut captures = captures.borrow_mut();
        let Some(capture) = captures.get_mut(&id) else {
            return -1;
        };
        if capture.state == CanvasCaptureState::Failed {
            return -1;
        }
        if (capacity as usize) < capture.bytes {
            return -1;
        }

        if capture.state == CanvasCaptureState::Pending {
            let Some(fence) = capture.fence else {
                capture.state = CanvasCaptureState::Failed;
                return -1;
            };
            let Some(status) = with_gl(|gl| unsafe {
                gl.client_wait_sync(fence, glow::SYNC_FLUSH_COMMANDS_BIT, 0)
            }) else {
                capture.state = CanvasCaptureState::Failed;
                return -1;
            };
            if status == glow::TIMEOUT_EXPIRED {
                return 0;
            }
            if !readback_fence_ready(status) {
                crate::log_once!("CANVAS", "canvas capture fence wait failed for id {}", id);
                capture.state = CanvasCaptureState::Failed;
                return -1;
            }
            if let Some(fence) = capture.fence.take() {
                if with_gl(|gl| unsafe { gl.delete_sync(fence) }).is_none() {
                    capture.state = CanvasCaptureState::Failed;
                    return -1;
                }
            }
            capture.state = CanvasCaptureState::Ready;
        }

        let pbo = capture.pbo;
        let width = capture.width as usize;
        let height = capture.height as usize;
        let bytes = capture.bytes;
        let copied = with_gl(|gl| unsafe {
            let previous_pack = gl.get_parameter_i32(glow::PIXEL_PACK_BUFFER_BINDING);
            gl.bind_buffer(glow::PIXEL_PACK_BUFFER, Some(pbo));
            let ptr = gl.map_buffer_range(
                glow::PIXEL_PACK_BUFFER,
                0,
                i32::try_from(bytes).unwrap_or(0),
                glow::MAP_READ_BIT,
            );
            if ptr.is_null() {
                gl.bind_buffer(glow::PIXEL_PACK_BUFFER, buffer_from_name(previous_pack));
                return false;
            }
            copy_rgba_rows_top_down(ptr, out, width, height);
            gl.unmap_buffer(glow::PIXEL_PACK_BUFFER);
            gl.bind_buffer(glow::PIXEL_PACK_BUFFER, buffer_from_name(previous_pack));
            true
        })
        .unwrap_or(false);
        if copied {
            bytes as c_int
        } else {
            crate::log_once!("CANVAS", "canvas capture PBO map failed for id {}", id);
            capture.state = CanvasCaptureState::Failed;
            -1
        }
    })
}

/// Release a capture and all of its GL resources. Releasing a pending capture
/// drops its PBO and fence; OpenGL keeps the submitted readback ordered.
#[no_mangle]
pub extern "C" fn stone_canvas_capture_release(id: u64) -> bool {
    if id == 0 {
        return false;
    }
    let capture = CANVAS_CAPTURES.with(|captures| captures.borrow_mut().remove(&id));
    let Some(capture) = capture else { return false };
    destroy_canvas_capture(capture);
    true
}

/// Alias for callers that abort a save before its capture is ready.
#[no_mangle]
pub extern "C" fn stone_canvas_capture_cancel(id: u64) -> bool {
    stone_canvas_capture_release(id)
}

/// Cancel every capture owned by this GL thread and return the number retired.
///
/// Captures are deliberately transient rather than native-ledger handles: a
/// save snapshot is never part of a worker's restored game world. The replay
/// boundary and final teardown must still close them before their GL context is
/// replaced, including when an entry aborts while a capture is pending.
#[no_mangle]
pub extern "C" fn stone_canvas_capture_cancel_all() -> c_int {
    let captures = drain_canvas_captures();
    let count = captures.len();
    for capture in captures {
        destroy_canvas_capture(capture);
    }
    c_int::try_from(count).unwrap_or(c_int::MAX)
}

/// Create a new pixel-readback slot. `is_depth = 0` for color (RGBA32F red
/// channel), non-zero for depth. Returns 0 on GL failure.
#[no_mangle]
pub extern "C" fn stone_pixel_readback_new(is_depth: c_int) -> u64 {
    let channel = if is_depth != 0 {
        Channel::Depth
    } else {
        Channel::Color
    };
    let Some(slot) = create_slot(channel, ReadShape::Pixel) else {
        return 0;
    };
    let id = crate::handles::mint(Class::PixelReadback);
    SLOTS.with(|s| s.borrow_mut().insert(id, slot));
    id
}

/// Create an integer entity-id readback whose submit coordinate is the center
/// of a square up to `max_side` x `max_side`. Consume returns the nonzero id whose
/// visible pixel is nearest that center.
#[no_mangle]
pub extern "C" fn stone_pixel_readback_area_new(max_side: c_int) -> u64 {
    if max_side <= 0 {
        return 0;
    }
    let Some(slot) = create_slot(Channel::Color, ReadShape::NearestId { max_side }) else {
        return 0;
    };
    let id = crate::handles::mint(Class::PixelReadback);
    SLOTS.with(|s| s.borrow_mut().insert(id, slot));
    id
}

/// Live pixel-readback slot ids (`native_ledger`).
pub(crate) fn live_readback_ids() -> Vec<u64> {
    SLOTS.with(|s| s.borrow().keys().copied().collect())
}

/// Return a slot to CREATION's state: nothing promoted, nothing pending.
///
/// The slot's cross-entry state is these flags, not the PBO bytes -- consume()
/// returns the channel sentinel whenever `ready_primed` is false, so clearing
/// them is exactly what a freshly created slot answers. The pixels themselves
/// are unreachable until a submit primes them again.
///
/// Returns false for an unknown handle so the caller cannot report a reset it
/// did not perform (the same asymmetry as `stone_canvas_reset_contents`).
#[no_mangle]
pub extern "C" fn stone_pixel_readback_reset_contents(id: u64) -> bool {
    if id == 0 {
        return false;
    }
    SLOTS.with(|slots| {
        let mut slots = slots.borrow_mut();
        let Some(slot) = slots.get_mut(&id) else {
            return false;
        };
        slot.ready_primed = false;
        slot.staging_primed = false;
        slot.ready_garbage = false;
        slot.staging_garbage = false;
        for read in slot.inflight.drain(..) {
            delete_fence(read.fence);
        }
        true
    })
}

#[no_mangle]
pub extern "C" fn stone_pixel_readback_destroy(id: u64) {
    if id == 0 {
        return;
    }
    let removed = SLOTS.with(|s| s.borrow_mut().remove(&id));
    let Some(slot) = removed else { return };
    crate::handles::retire(Class::PixelReadback, id);
    for read in slot.inflight {
        delete_fence(read.fence);
    }
    with_gl(|gl| unsafe {
        for pbo in slot.pbos {
            gl.delete_buffer(pbo);
        }
    });
}

/// Map the "ready" PBO (prior submit, DMA complete) and return the pixel
/// value. Returns the channel sentinel if no prior submit has been promoted
/// to ready or if the prior submit was skipped (OOB / unknown canvas).
#[no_mangle]
pub extern "C" fn stone_pixel_readback_consume(id: u64) -> c_float {
    if id == 0 {
        return 0.0;
    }

    SLOTS.with(|slots| {
        let mut slots = slots.borrow_mut();
        let Some(slot) = slots.get_mut(&id) else {
            return 0.0;
        };
        consume_ready(slot)
    })
}

/// Non-blocking entity-id read: the ready answer as an id (0 for nothing or a
/// skipped submit), or -1 while no answer has landed. Never waits on the GPU.
/// `stone_pixel_readback_answer_x`/`_y` name the pixel an answer is for.
#[no_mangle]
pub extern "C" fn stone_pixel_readback_poll_id(id: u64) -> c_int {
    if id == 0 {
        return -1;
    }
    SLOTS.with(|slots| {
        let mut slots = slots.borrow_mut();
        let Some(slot) = slots.get_mut(&id) else {
            return -1;
        };
        if !slot.polled {
            // Reads queued in the ping-pong carry no fence; start the ring.
            slot.polled = true;
            slot.ready_primed = false;
            slot.staging_primed = false;
            return -1;
        }
        // Lazy: landed_prefix stops waiting at the first pending read.
        let statuses = slot.inflight.iter().map(|read| {
            let status = if read.garbage {
                None
            } else {
                fence_wait_status(read.fence)
            };
            if matches!(status, Some(glow::WAIT_FAILED)) {
                crate::log_once!(
                    "READBACK",
                    "pixel readback fence wait failed; reading without it"
                );
            }
            poll_answer_landed(read.garbage, status)
        });
        let landed = landed_prefix(statuses);
        if landed == 0 {
            return -1;
        }
        let mut newest = None;
        for _ in 0..landed {
            if let Some(read) = slot.inflight.pop_front() {
                delete_fence(read.fence);
                newest = Some(read);
            }
        }
        let Some(read) = newest else { return -1 };
        slot.answer_center = read.center;
        if read.garbage {
            return 0;
        }
        let (value, _) = read_pbo(
            slot,
            slot.pbos[read.idx],
            read.fmt,
            read.rect,
            read.center,
            None,
        );
        (value as c_int).max(0)
    })
}

/// Take the oldest completed read from a polled slot. The first call starts
/// the fenced ring and returns `None`; a failed or missing fence counts as
/// landed, as in `poll_answer_landed`.
fn take_oldest_landed(slot: &mut Slot) -> Option<InFlight> {
    if !slot.polled {
        slot.polled = true;
        slot.ready_primed = false;
        slot.staging_primed = false;
        return None;
    }
    let front = slot.inflight.front()?;
    let status = if front.garbage {
        None
    } else {
        fence_wait_status(front.fence)
    };
    if matches!(status, Some(glow::WAIT_FAILED)) {
        crate::log_once!("READBACK", "pixel readback fence wait failed");
    }
    if !poll_answer_landed(front.garbage, status) {
        return None;
    }
    let read = slot.inflight.pop_front()?;
    delete_fence(read.fence);
    Some(read)
}

/// Non-blocking RGBA read from a polled color pixel slot. Returns 1 after
/// copying all four channels, 0 for a skipped submit, -1 while pending (the
/// first call primes the fenced ring), and -2 for invalid arguments, slot,
/// fence, or map failure. `capacity` is measured in `c_float` elements.
#[no_mangle]
pub extern "C" fn stone_pixel_readback_poll_rgba(
    id: u64,
    out: *mut c_float,
    capacity: c_int,
) -> c_int {
    if id == 0 || out.is_null() || capacity < 4 {
        return -2;
    }
    SLOTS.with(|slots| {
        let mut slots = slots.borrow_mut();
        let Some(slot) = slots.get_mut(&id) else {
            return -2;
        };
        if !matches!(slot.channel, Channel::Color) || !matches!(slot.shape, ReadShape::Pixel) {
            return -2;
        }
        if !slot.polled {
            let _ = take_oldest_landed(slot);
            return -1;
        }
        let Some((garbage, fence)) = slot.inflight.front().map(|read| (read.garbage, read.fence))
        else {
            return -1;
        };
        if garbage {
            let Some(_read) = take_oldest_landed(slot) else {
                return -1;
            };
            return 0;
        }

        let status = fence_wait_status(fence);
        if status == Some(glow::TIMEOUT_EXPIRED) {
            return -1;
        }
        if !status.is_some_and(readback_fence_ready) {
            if status != Some(glow::WAIT_FAILED) {
                crate::log_once!(
                    "READBACK",
                    "pixel RGBA readback fence missing or invalid for id {}",
                    id
                );
            }
            // Retire a broken landed entry so it cannot permanently fill the ring.
            let _ = take_oldest_landed(slot);
            return -2;
        }

        let Some(read) = take_oldest_landed(slot) else {
            return -1;
        };
        if read.garbage {
            return 0;
        }
        let mut rgba = [0.0; 4];
        let (_, mapped) = read_pbo(
            slot,
            slot.pbos[read.idx],
            read.fmt,
            read.rect,
            read.center,
            Some(&mut rgba),
        );
        if !mapped {
            crate::log_once!("READBACK", "pixel RGBA readback map failed for id {}", id);
            return -2;
        }
        unsafe { ptr::copy_nonoverlapping(rgba.as_ptr(), out, rgba.len()) };
        1
    })
}

/// Reads a polled slot has submitted and not yet answered.
#[no_mangle]
pub extern "C" fn stone_pixel_readback_in_flight(id: u64) -> c_int {
    SLOTS.with(|slots| {
        slots
            .borrow()
            .get(&id)
            .map_or(0, |slot| slot.inflight.len() as c_int)
    })
}

#[no_mangle]
pub extern "C" fn stone_pixel_readback_answer_x(id: u64) -> c_int {
    SLOTS.with(|slots| {
        slots
            .borrow()
            .get(&id)
            .map_or(0, |slot| slot.answer_center.0)
    })
}

#[no_mangle]
pub extern "C" fn stone_pixel_readback_answer_y(id: u64) -> c_int {
    SLOTS.with(|slots| {
        slots
            .borrow()
            .get(&id)
            .map_or(0, |slot| slot.answer_center.1)
    })
}

fn consume_ready(slot: &mut Slot) -> c_float {
    let sentinel = slot.channel.sentinel();

    if !slot.ready_primed {
        return sentinel;
    }
    // One-shot: each promoted submit is read at most once. A second
    // consume() without an intervening submit returns sentinel.
    slot.ready_primed = false;

    if slot.ready_garbage {
        slot.ready_garbage = false;
        return sentinel;
    }

    let pbo = slot.pbos[slot.ready_idx];
    read_pbo(
        slot,
        pbo,
        slot.ready_fmt,
        slot.ready_rect,
        slot.ready_center,
        None,
    )
    .0
}

fn pixel_component(bytes: &[u8], fmt: PixelFmt, channel: usize) -> f32 {
    if matches!(fmt, PixelFmt::Bytes) {
        return bytes[channel] as f32 / 255.0;
    }
    let offset = channel * 4;
    let component = [
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ];
    match fmt {
        PixelFmt::F32 => f32::from_ne_bytes(component),
        PixelFmt::Ints => i32::from_ne_bytes(component) as f32,
        PixelFmt::Bytes => unreachable!(),
    }
}

/// Map `pbo` and decode the read it holds for this slot's shape. Returns the
/// value and whether mapping succeeded.
fn read_pbo(
    slot: &Slot,
    pbo: glow::Buffer,
    fmt: PixelFmt,
    rect: ReadRect,
    center: (i32, i32),
    rgba_out: Option<&mut [f32; 4]>,
) -> (c_float, bool) {
    let sentinel = slot.channel.sentinel();
    let mut result = sentinel;
    let mut mapped = false;
    let bytes = slot.bytes;
    let shape = slot.shape;
    with_gl(|gl| unsafe {
        gl.bind_buffer(glow::PIXEL_PACK_BUFFER, Some(pbo));
        let ptr = gl.map_buffer_range(glow::PIXEL_PACK_BUFFER, 0, bytes as i32, glow::MAP_READ_BIT);
        if !ptr.is_null() {
            mapped = true;
            let slice: &[u8] = std::slice::from_raw_parts(ptr as *const u8, bytes);
            result = match (shape, fmt) {
                (ReadShape::NearestId { .. }, PixelFmt::Ints) => nearest_int_id(
                    slice,
                    rect.width as usize,
                    rect.height as usize,
                    slot.channel.bytes(),
                    rect.x,
                    rect.y,
                    center.0,
                    center.1,
                    true,
                ) as f32,
                (ReadShape::NearestId { .. }, _) => sentinel,
                (ReadShape::Pixel, _) => pixel_component(slice, fmt, 0),
            };
            if matches!(shape, ReadShape::Pixel) && matches!(slot.channel, Channel::Color) {
                if let Some(rgba_out) = rgba_out {
                    let mut rgba = [0.0; 4];
                    for (channel, value) in rgba.iter_mut().enumerate() {
                        *value = pixel_component(slice, fmt, channel);
                    }
                    *rgba_out = rgba;
                }
            }
            gl.unmap_buffer(glow::PIXEL_PACK_BUFFER);
        }
        gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
    });
    (result, mapped)
}

/// Submit an async glReadPixels into this slot's staging PBO. A normal slot
/// reads one pixel at `(x, y)`; an area slot reads its configured square
/// centered there. The
/// previous staging (if any) is promoted to "ready" first so consume() can
/// pick it up — guarantees at least one frame of distance between the submit
/// call and the consume that reads it, regardless of whether the two calls
/// happen in the same frame or split across update/draw phases.
///
/// Call when `canvas_handle` is quiescent (no pending writes) to avoid an
/// implicit pipeline flush — otherwise the driver serializes on pending
/// writes and the "async" submit stalls.
#[no_mangle]
pub extern "C" fn stone_pixel_readback_submit(id: u64, canvas_handle: u64, x: c_int, y: c_int) {
    pixel_readback_submit(id, canvas_handle, x, y, None);
}

#[no_mangle]
pub extern "C" fn stone_pixel_readback_submit_area(
    id: u64,
    canvas_handle: u64,
    x: c_int,
    y: c_int,
    side: c_int,
) {
    pixel_readback_submit(id, canvas_handle, x, y, Some(side));
}

fn pixel_readback_submit(id: u64, canvas_handle: u64, x: c_int, y: c_int, requested: Option<i32>) {
    if id == 0 {
        return;
    }

    SLOTS.with(|slots| {
        let mut slots = slots.borrow_mut();
        let Some(slot) = slots.get_mut(&id) else {
            return;
        };

        // A polled read goes to a free ring PBO; with every one in flight the
        // request is dropped and the poll keeps answering the older ones.
        let ring_idx = if slot.polled {
            let in_flight: Vec<usize> = slot.inflight.iter().map(|read| read.idx).collect();
            let Some(idx) = poll_ring_free_index(&in_flight) else {
                return;
            };
            Some(idx)
        } else {
            None
        };
        // Promote prior staging to ready. The previous ready (if any and
        // unconsumed) is dropped — we only keep the freshest primed value.
        if slot.staging_primed && ring_idx.is_none() {
            promote_staging(slot);
        }

        let staging_idx = ring_idx.unwrap_or(slot.staging_idx);
        let channel = slot.channel;

        let canvas = lookup_canvas(canvas_handle);
        let side = requested_side(slot.shape, requested);
        let rect = match (canvas, side) {
            (Some(info), Some(1))
                if matches!(slot.shape, ReadShape::Pixel)
                    && x >= 0
                    && y >= 0
                    && x < info.active_w
                    && y < info.active_h =>
            {
                Some(ReadRect {
                    x,
                    y,
                    width: 1,
                    height: 1,
                })
            }
            (Some(info), Some(side)) if matches!(slot.shape, ReadShape::NearestId { .. }) => {
                centered_read_rect(x, y, side, info.active_w, info.active_h)
            }
            _ => None,
        };
        let submit_ok = match (canvas, rect) {
            (Some(info), Some(_)) => {
                let channel_ok = match channel {
                    Channel::Color => true,
                    Channel::Depth => info.has_depth,
                };
                let shape_ok = match slot.shape {
                    ReadShape::Pixel => true,
                    ReadShape::NearestId { .. } => info.int_format,
                };
                channel_ok && shape_ok
            }
            _ => false,
        };

        if !submit_ok {
            if ring_idx.is_some() {
                slot.inflight.push_back(InFlight {
                    idx: staging_idx,
                    fence: None,
                    garbage: true,
                    fmt: PixelFmt::F32,
                    rect: ReadRect {
                        x: 0,
                        y: 0,
                        width: 1,
                        height: 1,
                    },
                    center: (x, y),
                });
                return;
            }
            slot.staging_primed = true;
            slot.staging_garbage = true;
            return;
        }

        let info = canvas.unwrap();
        let rect = rect.unwrap();
        // RGBA/FLOAT reads are only legal from float FBOs on GLES; a
        // downgraded (RGBA8) canvas reads back as bytes, an R32I id canvas
        // as RGBA_INTEGER/INT (its guaranteed pair).
        let fmt = match channel {
            Channel::Depth => PixelFmt::F32,
            Channel::Color if info.int_format => PixelFmt::Ints,
            Channel::Color if info.float_format => PixelFmt::F32,
            Channel::Color => PixelFmt::Bytes,
        };
        let fence = with_gl(|gl| unsafe {
            // OpenGL origin is bottom-left; callers pass top-left coords in
            // the ACTIVE image, which is anchored at the GL origin.
            let gl_y = info.active_h - rect.y - rect.height;
            let prev_fbo = current_fbo();
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(info.fbo));
            gl.bind_buffer(glow::PIXEL_PACK_BUFFER, Some(slot.pbos[staging_idx]));
            gl.pixel_store_i32(glow::PACK_ALIGNMENT, 1);
            let (gl_format, gl_type) = match fmt {
                PixelFmt::Ints => (glow::RGBA_INTEGER, glow::INT),
                PixelFmt::Bytes => (channel.gl_format(), glow::UNSIGNED_BYTE),
                PixelFmt::F32 => (channel.gl_format(), glow::FLOAT),
            };
            gl.read_pixels(
                rect.x,
                gl_y,
                rect.width,
                rect.height,
                gl_format,
                gl_type,
                glow::PixelPackData::BufferOffset(0),
            );
            gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
            let fence = if ring_idx.is_some() {
                match gl.fence_sync(glow::SYNC_GPU_COMMANDS_COMPLETE, 0) {
                    Ok(fence) => Some(fence),
                    Err(error) => {
                        crate::log_once!(
                            "READBACK",
                            "pixel readback fence creation failed: {}",
                            error
                        );
                        None
                    }
                }
            } else {
                None
            };
            // Restore full FBO binding — READ_FRAMEBUFFER alone leaves the
            // read target dangling for subsequent code, and None means the
            // window framebuffer (a real FBO on iOS, not 0).
            gl.bind_framebuffer(glow::FRAMEBUFFER, crate::canvas::resolve_target(prev_fbo));
            fence
        })
        .flatten();

        if ring_idx.is_some() {
            slot.inflight.push_back(InFlight {
                idx: staging_idx,
                fence,
                garbage: false,
                fmt,
                rect,
                center: (x, y),
            });
            return;
        }
        slot.staging_primed = true;
        slot.staging_garbage = false;
        slot.staging_fmt = fmt;
        slot.staging_rect = rect;
        slot.staging_center = (x, y);
    });
}

#[cfg(test)]
mod tests {
    use super::{
        buffer_from_name, canvas_capture_size, centered_read_rect, copy_rgba_rows_top_down,
        landed_prefix, nearest_int_id, poll_answer_landed, poll_ring_free_index,
        readback_fence_ready, release_all, requested_side, stone_canvas_capture_cancel_all,
        CanvasCapture, CanvasCaptureState, ReadRect, ReadShape, CANVAS_CAPTURES,
    };

    #[test]
    fn a_polled_read_lands_when_its_fence_signals() {
        assert!(!poll_answer_landed(false, Some(glow::TIMEOUT_EXPIRED)));
        assert!(poll_answer_landed(false, Some(glow::ALREADY_SIGNALED)));
        assert!(poll_answer_landed(false, Some(glow::CONDITION_SATISFIED)));
        // A skipped read wrote no PBO, so it lands at once.
        assert!(poll_answer_landed(true, None));
        assert!(poll_answer_landed(true, Some(glow::TIMEOUT_EXPIRED)));
        // A failed wait maps the PBO anyway: blocking is correct, never landing is not.
        assert!(poll_answer_landed(false, Some(glow::WAIT_FAILED)));
        assert!(poll_answer_landed(false, None));
    }

    #[test]
    fn a_poll_takes_the_newest_read_of_the_landed_run() {
        // Fences signal in submission order; a pending read hides newer ones.
        assert_eq!(landed_prefix([true, true, false, true]), 2);
        assert_eq!(landed_prefix([false, true]), 0);
        assert_eq!(landed_prefix([true, true]), 2);
        assert_eq!(landed_prefix([]), 0);
    }

    #[test]
    fn a_polled_submit_waits_only_for_a_full_ring() {
        assert_eq!(poll_ring_free_index(&[]), Some(0));
        assert_eq!(poll_ring_free_index(&[1, 3]), Some(0));
        assert_eq!(poll_ring_free_index(&[0, 2]), Some(1));
        assert_eq!(poll_ring_free_index(&[3, 0, 1, 2]), None);
    }

    fn rgba_i32(ids: &[i32]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(ids.len() * 16);
        for &id in ids {
            bytes.extend_from_slice(&id.to_ne_bytes());
            bytes.extend_from_slice(&[0; 12]);
        }
        bytes
    }

    #[test]
    fn area_pick_keeps_the_exact_center_hit() {
        let bytes = rgba_i32(&[0, 0, 9, 0, 7, 0, 0, 0, 0]);

        assert_eq!(nearest_int_id(&bytes, 3, 3, 16, 4, 8, 5, 9, false), 7);
    }

    #[test]
    fn area_pick_uses_the_nearest_nonzero_pixel() {
        let bytes = rgba_i32(&[
            0, 0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0, 0, 13, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ]);

        assert_eq!(nearest_int_id(&bytes, 5, 5, 16, 10, 20, 12, 22, false), 13);
    }

    #[test]
    fn area_pick_ignores_surface_metadata() {
        let bytes = rgba_i32(&[0, 0, 0, 0, -512, 13, 0, 0, 0]);
        assert_eq!(nearest_int_id(&bytes, 3, 3, 16, 0, 0, 1, 1, false), 13);
    }

    #[test]
    fn area_pick_breaks_distance_ties_by_id() {
        let bytes = rgba_i32(&[0, 0, 0, 9, 0, 3, 0, 0, 0]);

        assert_eq!(nearest_int_id(&bytes, 3, 3, 16, 0, 0, 1, 1, false), 3);
    }

    #[test]
    fn area_pick_handles_a_region_clipped_at_the_canvas_edge() {
        let bytes = rgba_i32(&[0, 21, 0, 0]);

        assert_eq!(nearest_int_id(&bytes, 2, 2, 16, 0, 0, 0, 0, false), 21);
    }

    #[test]
    fn area_pick_accounts_for_gl_rows_being_bottom_up() {
        let bytes = rgba_i32(&[0, 8, 0, 0, 0, 0, 0, 0, 13]);

        assert_eq!(nearest_int_id(&bytes, 3, 3, 16, 0, 0, 1, 0, true), 13);
    }

    #[test]
    fn area_pick_rejects_an_incomplete_readback() {
        assert_eq!(
            nearest_int_id(&[1, 0, 0, 0], 2, 2, 16, 0, 0, 0, 0, false),
            0
        );
    }

    #[test]
    fn configured_area_is_centered_and_clipped() {
        assert_eq!(
            centered_read_rect(10, 20, 9, 100, 80),
            Some(ReadRect {
                x: 6,
                y: 16,
                width: 9,
                height: 9
            }),
        );
        assert_eq!(
            centered_read_rect(0, 0, 9, 100, 80),
            Some(ReadRect {
                x: 0,
                y: 0,
                width: 5,
                height: 5
            }),
        );
        assert_eq!(
            centered_read_rect(10, 20, 6, 100, 80),
            Some(ReadRect {
                x: 7,
                y: 17,
                width: 6,
                height: 6
            }),
        );
    }

    #[test]
    fn configured_area_requires_a_positive_size_and_an_in_bounds_center() {
        assert_eq!(centered_read_rect(10, 20, 0, 100, 80), None);
        assert_eq!(centered_read_rect(-1, 20, 9, 100, 80), None);
        assert_eq!(centered_read_rect(100, 20, 9, 100, 80), None);
    }

    #[test]
    fn area_slot_accepts_each_requested_side_up_to_its_capacity() {
        let shape = ReadShape::NearestId { max_side: 12 };
        assert_eq!(requested_side(shape, Some(6)), Some(6));
        assert_eq!(requested_side(shape, Some(12)), Some(12));
        assert_eq!(requested_side(shape, Some(13)), None);
        assert_eq!(requested_side(shape, Some(0)), None);
    }

    #[test]
    fn canvas_capture_fence_status_only_ready_for_signaled_states() {
        assert!(readback_fence_ready(glow::ALREADY_SIGNALED));
        assert!(readback_fence_ready(glow::CONDITION_SATISFIED));
        assert!(!readback_fence_ready(glow::TIMEOUT_EXPIRED));
        assert!(!readback_fence_ready(glow::WAIT_FAILED));
    }

    #[test]
    fn canvas_capture_size_is_bounded_before_gpu_allocation() {
        assert_eq!(canvas_capture_size(128, 128), Some(128 * 128 * 4));
        assert_eq!(canvas_capture_size(1, 128), Some(128 * 4));
        assert_eq!(canvas_capture_size(129, 128), None);
        assert_eq!(canvas_capture_size(128, 129), None);
        assert_eq!(canvas_capture_size(0, 128), None);
        assert_eq!(canvas_capture_size(-1, 128), None);
    }

    #[test]
    fn canvas_capture_copies_gl_rows_in_the_canvas_readback_order() {
        let source = [10, 11, 12, 13, 20, 21, 22, 23];
        let mut output = [0; 8];
        unsafe { copy_rgba_rows_top_down(source.as_ptr(), output.as_mut_ptr(), 1, 2) };
        assert_eq!(output, [20, 21, 22, 23, 10, 11, 12, 13]);
    }

    #[test]
    fn worker_boundary_cancel_all_drains_transient_captures() {
        CANVAS_CAPTURES.with(|captures| {
            captures.borrow_mut().insert(
                1,
                CanvasCapture {
                    pbo: buffer_from_name(1).unwrap(),
                    fence: None,
                    width: 1,
                    height: 1,
                    bytes: 4,
                    state: CanvasCaptureState::Pending,
                },
            );
        });

        assert_eq!(stone_canvas_capture_cancel_all(), 1);
        assert!(CANVAS_CAPTURES.with(|captures| captures.borrow().is_empty()));
        assert_eq!(stone_canvas_capture_cancel_all(), 0);
    }

    #[test]
    fn teardown_releases_captures() {
        CANVAS_CAPTURES.with(|captures| {
            captures.borrow_mut().insert(
                3,
                CanvasCapture {
                    pbo: buffer_from_name(1).unwrap(),
                    fence: None,
                    width: 1,
                    height: 1,
                    bytes: 4,
                    state: CanvasCaptureState::Pending,
                },
            );
        });

        // The drain runs without a context too: GL deletes no-op via with_gl.
        release_all();

        assert!(CANVAS_CAPTURES.with(|captures| captures.borrow().is_empty()));
    }
}
