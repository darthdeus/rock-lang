//! Per-frame streaming storage for data a draw reads once: mesh vertices and
//! indices, and uniform block bytes. One stream per kind of data, each one
//! buffer object split into `REGIONS` equal regions; frame N writes region
//! N % REGIONS, appending every upload at a fresh (aligned) offset. So every
//! upload has its own range, and no draw re-specifies storage another draw of
//! the frame reads: the binding names the bytes.
//!
//! Re-specifying one buffer per draw (`glBufferData`) left telling a frame's
//! storages apart to the driver; NVIDIA 595.104 intermittently served a draw
//! the storage its own upload had replaced (#883).
//!
//! A region is written again only once the fence that closed its frame has
//! signaled. The check never waits: a blocking wait caps frames in flight
//! below the driver's own queue, so a GPU pause the driver would absorb became
//! a 100-200 ms CPU stall. A stream whose next region the GPU still
//! reads, or that outgrows its region mid-frame, moves to a new buffer object;
//! the old one is never written again and is deleted once a fence covering its
//! last draw signals.

use std::cell::RefCell;

use crate::DebugGl;

const REGIONS: usize = 3;

/// The kinds of streamed data, one stream each.
#[derive(Clone, Copy)]
pub(crate) enum Stream {
    Vertex = 0,
    Index = 1,
    Uniform = 2,
}

/// Where an upload landed. `frame` is the stream frame it was written in; the
/// bytes stay there for that frame's draws only (see [`frame`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct StreamRange {
    pub buffer: glow::Buffer,
    pub offset: i32,
    pub frame: u64,
    /// Whether this upload needed new storage.
    pub replaced: bool,
}

#[derive(Default)]
struct FrameStream {
    buffer: Option<glow::Buffer>,
    /// Bytes per region; kept when the buffer is retired so the next one
    /// starts at the size the stream has grown to.
    capacity: usize,
    cursor: usize,
    /// Upload offset alignment; resolved on first use.
    alignment: usize,
}

#[derive(Default)]
struct Streams {
    frame: u64,
    region: usize,
    /// The fence closing the last frame that wrote each region.
    fences: [Option<glow::Fence>; REGIONS],
    streams: [FrameStream; 3],
    /// Buffers no upload writes again, with a fence covering their last draw.
    retired: Vec<(glow::Fence, Vec<glow::Buffer>)>,
    fence_error_reported: bool,
}

thread_local! {
    static STREAMS: RefCell<Streams> = RefCell::new(Streams::default());
}

fn minimum_capacity(stream: Stream) -> usize {
    match stream {
        Stream::Vertex => 4 << 20,
        Stream::Index => 1 << 20,
        Stream::Uniform => 256 << 10,
    }
}

fn align_up(value: usize, alignment: usize) -> Option<usize> {
    value
        .checked_add(alignment - 1)
        .map(|v| v / alignment * alignment)
}

/// The current stream frame. A range is readable by draws of the frame it was
/// written in; a draw in a later frame needs a fresh upload.
pub(crate) fn frame() -> u64 {
    STREAMS.with(|s| s.borrow().frame)
}

/// Close the frame: fence it, move every stream to the next region, and
/// delete retired buffers the GPU is done with. Called once per frame from
/// `stone_reset_frame_stats`.
pub(crate) fn begin_frame(gl: &DebugGl) {
    STREAMS.with(|s| unsafe { s.borrow_mut().begin_frame(gl) });
}

/// Write `data` into `stream`'s current region. The buffer is left bound to
/// `COPY_WRITE_BUFFER` only; the caller binds the range where its draw reads it.
pub(crate) fn upload(gl: &DebugGl, stream: Stream, data: &[u8]) -> Result<StreamRange, String> {
    STREAMS.with(|s| unsafe { s.borrow_mut().upload(gl, stream, data) })
}

/// Delete every stream buffer and fence. Called at GL teardown.
pub(crate) fn release_all(gl: &DebugGl) {
    STREAMS.with(|s| unsafe {
        let mut s = s.borrow_mut();
        for fence in s.fences.iter_mut().filter_map(Option::take) {
            gl.delete_sync(fence);
        }
        for stream in &mut s.streams {
            if let Some(buffer) = stream.buffer.take() {
                gl.delete_buffer(buffer);
            }
        }
        for (fence, buffers) in s.retired.drain(..) {
            gl.delete_sync(fence);
            for buffer in buffers {
                gl.delete_buffer(buffer);
            }
        }
        *s = Streams::default();
    });
}

impl Streams {
    /// Whether `fence` has signaled, without waiting. The flush bit makes a
    /// frame that never swaps (sims) still submit the fence.
    unsafe fn signaled(&mut self, gl: &DebugGl, fence: glow::Fence) -> bool {
        match gl.client_wait_sync(fence, glow::SYNC_FLUSH_COMMANDS_BIT, 0) {
            glow::ALREADY_SIGNALED | glow::CONDITION_SATISFIED => true,
            glow::TIMEOUT_EXPIRED => false,
            _ => {
                if !self.fence_error_reported {
                    self.fence_error_reported = true;
                    ::log::error!(target: "STONE", "stream buffers: client_wait_sync failed; treating the fence as busy");
                }
                false
            }
        }
    }

    unsafe fn fence(&mut self, gl: &DebugGl) -> Option<glow::Fence> {
        match gl.fence_sync(glow::SYNC_GPU_COMMANDS_COMPLETE, 0) {
            Ok(fence) => Some(fence),
            Err(e) => {
                if !self.fence_error_reported {
                    self.fence_error_reported = true;
                    ::log::error!(target: "STONE", "stream buffers: fence_sync failed: {e}");
                }
                None
            }
        }
    }

    /// Stop writing `buffers`; they are deleted once a fence issued now signals.
    unsafe fn retire(&mut self, gl: &DebugGl, buffers: Vec<glow::Buffer>) {
        if buffers.is_empty() {
            return;
        }
        match self.fence(gl) {
            Some(fence) => self.retired.push((fence, buffers)),
            // No fence to wait for: GL keeps deleted storage alive for queued
            // draws, so deleting now is still correct.
            None => {
                for buffer in buffers {
                    gl.delete_buffer(buffer);
                }
            }
        }
    }

    unsafe fn begin_frame(&mut self, gl: &DebugGl) {
        self.frame += 1;
        let mut i = 0;
        while i < self.retired.len() {
            if self.signaled(gl, self.retired[i].0) {
                let (fence, buffers) = self.retired.swap_remove(i);
                gl.delete_sync(fence);
                for buffer in buffers {
                    gl.delete_buffer(buffer);
                }
            } else {
                i += 1;
            }
        }
        if self.streams.iter().all(|s| s.buffer.is_none()) {
            return;
        }
        let closing = self.fence(gl);
        if let Some(old) = std::mem::replace(&mut self.fences[self.region], closing) {
            gl.delete_sync(old);
        }
        self.region = (self.region + 1) % REGIONS;
        for stream in &mut self.streams {
            stream.cursor = 0;
        }
        let Some(fence) = self.fences[self.region].take() else {
            return;
        };
        let idle = self.signaled(gl, fence);
        gl.delete_sync(fence);
        if idle {
            return;
        }
        // The GPU still reads this region: every stream moves to new storage.
        // The other regions' fences guarded the retired buffers only.
        let buffers: Vec<glow::Buffer> = self
            .streams
            .iter_mut()
            .filter_map(|s| s.buffer.take())
            .collect();
        self.retire(gl, buffers);
        for fence in self.fences.iter_mut().filter_map(Option::take) {
            gl.delete_sync(fence);
        }
    }

    unsafe fn upload(
        &mut self,
        gl: &DebugGl,
        kind: Stream,
        data: &[u8],
    ) -> Result<StreamRange, String> {
        let region = self.region;
        let frame = self.frame;
        let stream = &mut self.streams[kind as usize];
        if stream.alignment == 0 {
            stream.alignment = match kind {
                Stream::Uniform => gl
                    .get_parameter_i32(glow::UNIFORM_BUFFER_OFFSET_ALIGNMENT)
                    .max(4) as usize,
                Stream::Vertex | Stream::Index => 4,
            };
        }
        let alignment = stream.alignment;
        let mut start = align_up(stream.cursor, alignment).ok_or("stream size overflow")?;
        let mut end = start
            .checked_add(data.len())
            .ok_or("stream size overflow")?;
        let replaced = stream.buffer.is_none() || end > stream.capacity;
        if replaced {
            let capacity = end
                .max(stream.capacity)
                .max(minimum_capacity(kind))
                .checked_next_power_of_two()
                .and_then(|c| align_up(c, alignment))
                .ok_or("stream size overflow")?;
            let total = capacity
                .checked_mul(REGIONS)
                .filter(|n| *n <= i32::MAX as usize)
                .ok_or("stream exceeds GL offset range")?;
            let buffer = gl
                .create_buffer()
                .map_err(|e| format!("stream create_buffer failed: {e}"))?;
            gl.bind_buffer(glow::COPY_WRITE_BUFFER, Some(buffer));
            if let Err(error) =
                gl.buffer_data_size(glow::COPY_WRITE_BUFFER, total as i32, glow::STREAM_DRAW)
            {
                gl.delete_buffer(buffer);
                return Err(error);
            }
            let old = stream.buffer.replace(buffer);
            stream.capacity = capacity;
            stream.cursor = 0;
            start = 0;
            end = data.len();
            // Outgrown mid-frame: this frame's earlier draws still read the
            // old buffer, so it is retired, not re-specified.
            self.retire(gl, old.into_iter().collect());
        }
        let stream = &mut self.streams[kind as usize];
        let buffer = stream.buffer.ok_or("stream has no storage")?;
        let offset = region * stream.capacity + start;
        let offset = i32::try_from(offset).map_err(|_| "stream exceeds GL offset range")?;
        if !data.is_empty() {
            if !replaced {
                gl.bind_buffer(glow::COPY_WRITE_BUFFER, Some(buffer));
            }
            // Unsynchronized: no queued draw references this range (see the
            // module comment), so the driver need not track or wait for it.
            let mapped = gl.map_buffer_range(
                glow::COPY_WRITE_BUFFER,
                offset,
                data.len() as i32,
                glow::MAP_WRITE_BIT | glow::MAP_UNSYNCHRONIZED_BIT | glow::MAP_INVALIDATE_RANGE_BIT,
            );
            if mapped.is_null() {
                return Err("stream range mapping failed".into());
            }
            std::ptr::copy_nonoverlapping(data.as_ptr(), mapped, data.len());
            gl.unmap_buffer(glow::COPY_WRITE_BUFFER);
        }
        stream.cursor = end;
        Ok(StreamRange {
            buffer,
            offset,
            frame,
            replaced,
        })
    }
}
