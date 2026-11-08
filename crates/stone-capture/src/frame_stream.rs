//! A recording of presented frames as one raw RGBA stream: tightly packed
//! top-down rows, frame after frame, all the same size. The caller reads a
//! frame back into a recycled buffer and submits it; a writer thread orders
//! the rows and writes them, so the frame pays only the readback. The target
//! is usually a FIFO an encoder reads (tools/video), or a plain file.
//!
//! Memory is bounded by the buffer pool: when the writer falls behind,
//! `next_buffer` waits for it. A failed write stops the writer; the failure
//! is returned by the next `next_buffer`/`submit` and by `finish`.

use std::fs::File;
use std::io::Write;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::JoinHandle;

use crate::RowOrigin;

struct Frame {
    rgba: Vec<u8>,
    row_bytes: usize,
    origin: RowOrigin,
}

pub struct FrameStream {
    path: String,
    frames_tx: Option<SyncSender<Frame>>,
    free_rx: Receiver<Vec<u8>>,
    writer: Option<JoinHandle<Result<(), String>>>,
    size: Option<(u32, u32)>,
    frames: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameStreamSummary {
    pub frames: u64,
    pub width: u32,
    pub height: u32,
}

impl FrameStream {
    /// Opens `path` for writing and starts the writer with `buffers` frame
    /// buffers. A FIFO must already have a reader: the open does not wait
    /// for one, so a missing encoder fails here instead of hanging the frame.
    pub fn create(path: &str, buffers: usize) -> Result<Self, String> {
        if path.is_empty() {
            return Err("frame stream path is empty".to_string());
        }
        let file = open_for_stream(path)?;
        let buffers = buffers.max(1);
        let (frames_tx, frames_rx) = mpsc::sync_channel::<Frame>(buffers);
        let (free_tx, free_rx) = mpsc::sync_channel::<Vec<u8>>(buffers);
        for _ in 0..buffers {
            free_tx
                .send(Vec::new())
                .expect("the pool channel holds every buffer");
        }
        let thread_path = path.to_string();
        let writer = std::thread::Builder::new()
            .name("stone-frame-stream".to_string())
            .spawn(move || write_frames(file, &thread_path, frames_rx, free_tx))
            .map_err(|e| format!("failed to start the frame stream writer for {path}: {e}"))?;
        Ok(FrameStream {
            path: path.to_string(),
            frames_tx: Some(frames_tx),
            free_rx,
            writer: Some(writer),
            size: None,
            frames: 0,
        })
    }

    /// A buffer of `width * height * 4` bytes to read the next frame into.
    /// Waits while every buffer is queued for the writer.
    pub fn next_buffer(&mut self, width: u32, height: u32) -> Result<Vec<u8>, String> {
        if width == 0 || height == 0 {
            return Err(format!(
                "frame stream {}: frame is zero-sized ({width}x{height})",
                self.path
            ));
        }
        match self.size {
            Some(size) if size != (width, height) => {
                return Err(format!(
                    "frame stream {}: frame is {width}x{height}, the stream is {}x{}",
                    self.path, size.0, size.1
                ));
            }
            _ => {}
        }
        let len = (width as usize)
            .checked_mul(height as usize)
            .and_then(|px| px.checked_mul(4))
            .ok_or_else(|| {
                format!(
                    "frame stream {}: frame size overflows ({width}x{height})",
                    self.path
                )
            })?;
        let Ok(mut buffer) = self.free_rx.recv() else {
            return Err(self.writer_failure());
        };
        buffer.resize(len, 0);
        self.size = Some((width, height));
        Ok(buffer)
    }

    /// Queues a frame read into a buffer from `next_buffer`.
    pub fn submit(&mut self, rgba: Vec<u8>, origin: RowOrigin) -> Result<(), String> {
        let (width, height) = self
            .size
            .ok_or_else(|| format!("frame stream {}: no frame buffer taken", self.path))?;
        if rgba.len() != width as usize * height as usize * 4 {
            return Err(format!(
                "frame stream {}: {} bytes submitted for {width}x{height}",
                self.path,
                rgba.len()
            ));
        }
        let tx = self
            .frames_tx
            .as_ref()
            .expect("frames_tx lives until finish");
        let row_bytes = width as usize * 4;
        if tx
            .send(Frame {
                rgba,
                row_bytes,
                origin,
            })
            .is_err()
        {
            return Err(self.writer_failure());
        }
        self.frames += 1;
        Ok(())
    }

    /// Writes every queued frame, closes the stream and reports what it holds.
    pub fn finish(mut self) -> Result<FrameStreamSummary, String> {
        self.frames_tx = None;
        self.join_writer()?;
        let (width, height) = self.size.unwrap_or((0, 0));
        Ok(FrameStreamSummary {
            frames: self.frames,
            width,
            height,
        })
    }

    // The writer stopped early, which it does only on a failed write.
    fn writer_failure(&mut self) -> String {
        self.frames_tx = None;
        match self.join_writer() {
            Err(e) => e,
            Ok(()) => format!(
                "frame stream {}: the writer stopped without an error",
                self.path
            ),
        }
    }

    fn join_writer(&mut self) -> Result<(), String> {
        let Some(writer) = self.writer.take() else {
            return Err(format!(
                "frame stream {}: already stopped after an earlier failure",
                self.path
            ));
        };
        writer
            .join()
            .map_err(|_| format!("frame stream {}: the writer panicked", self.path))?
    }
}

impl Drop for FrameStream {
    fn drop(&mut self) {
        self.frames_tx = None;
        if let Some(writer) = self.writer.take() {
            if let Ok(Err(e)) = writer.join() {
                ::log::error!(target: "SCREENSHOT", "{e}");
            }
        }
    }
}

fn write_frames(
    mut file: File,
    path: &str,
    frames_rx: Receiver<Frame>,
    free_tx: SyncSender<Vec<u8>>,
) -> Result<(), String> {
    ignore_sigpipe_on_this_thread();
    let mut ordered = Vec::new();
    for Frame {
        rgba,
        row_bytes,
        origin,
    } in frames_rx
    {
        let bytes = match origin {
            RowOrigin::TopDown => &rgba,
            RowOrigin::BottomUp => {
                ordered.clear();
                for row in rgba.chunks_exact(row_bytes).rev() {
                    ordered.extend_from_slice(row);
                }
                &ordered
            }
        };
        file.write_all(bytes)
            .map_err(|e| format!("frame stream {path}: write failed: {e}"))?;
        // The receiver is gone only when the stream is being dropped.
        let _ = free_tx.send(rgba);
    }
    file.flush()
        .map_err(|e| format!("frame stream {path}: flush failed: {e}"))
}

// Opens without waiting for a FIFO reader (ENXIO when there is none), then
// makes the descriptor blocking so the writer waits on a slow reader.
#[cfg(unix)]
fn open_for_stream(path: &str) -> Result<File, String> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| format!("failed to open frame stream {path}: {e}"))?;
    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();
    // SAFETY: fcntl on a descriptor this function owns.
    let cleared = unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        flags >= 0 && libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) >= 0
    };
    if !cleared {
        return Err(format!(
            "failed to make frame stream {path} blocking: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(file)
}

#[cfg(not(unix))]
fn open_for_stream(path: &str) -> Result<File, String> {
    File::create(path).map_err(|e| format!("failed to open frame stream {path}: {e}"))
}

// A reader that exits turns the next write into EPIPE here instead of a
// SIGPIPE that would kill the whole game process. SIGPIPE from write() is
// sent to the writing thread, so blocking it on this thread is enough.
#[cfg(unix)]
fn ignore_sigpipe_on_this_thread() {
    // SAFETY: plain signal-mask calls on the current thread.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGPIPE);
        let rc = libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        if rc != 0 {
            ::log::error!(target: "SCREENSHOT", "frame stream: could not block SIGPIPE on the writer ({rc})");
        }
    }
}

#[cfg(not(unix))]
fn ignore_sigpipe_on_this_thread() {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("stone_frame_stream_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // Three columns, two rows, bottom row first as GL reads them; every
    // byte distinct so a flipped or shifted row cannot compare equal.
    fn bottom_up_frame(seed: u8) -> Vec<u8> {
        (0..3 * 2 * 4)
            .map(|i| seed.wrapping_add(i as u8 * 7))
            .collect()
    }

    #[test]
    fn a_streamed_frame_is_the_pixels_the_screenshot_saves() {
        let dir = temp_dir("pixels");
        let stream_path = dir.join("frames.rgba");
        let mut stream = FrameStream::create(stream_path.to_str().unwrap(), 2).unwrap();
        let mut pngs = Vec::new();
        for seed in [1u8, 90] {
            let png = dir.join(format!("frame_{seed}.png"));
            crate::save_rgba_png(
                png.to_str().unwrap(),
                3,
                2,
                RowOrigin::BottomUp,
                bottom_up_frame(seed),
                0,
            )
            .unwrap();
            pngs.push(png);
            let mut buffer = stream.next_buffer(3, 2).unwrap();
            buffer.copy_from_slice(&bottom_up_frame(seed));
            stream.submit(buffer, RowOrigin::BottomUp).unwrap();
        }
        let summary = stream.finish().unwrap();
        assert_eq!(
            summary,
            FrameStreamSummary {
                frames: 2,
                width: 3,
                height: 2
            }
        );

        let raw = std::fs::read(&stream_path).unwrap();
        assert_eq!(raw.len(), 2 * 3 * 2 * 4);
        for (i, png) in pngs.iter().enumerate() {
            let decoded = image::open(png).unwrap().to_rgba8().into_raw();
            assert_eq!(&raw[i * 24..(i + 1) * 24], decoded.as_slice(), "frame {i}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_frame_of_another_size_is_refused() {
        let dir = temp_dir("size");
        let path = dir.join("frames.rgba");
        let mut stream = FrameStream::create(path.to_str().unwrap(), 1).unwrap();
        let buffer = stream.next_buffer(3, 2).unwrap();
        stream.submit(buffer, RowOrigin::TopDown).unwrap();
        let err = stream.next_buffer(4, 2).unwrap_err();
        assert!(err.contains("4x2") && err.contains("3x2"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    fn make_fifo(path: &std::path::Path) {
        let c = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        // SAFETY: mkfifo on a NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_without_a_reader_fails_at_create_instead_of_hanging() {
        let dir = temp_dir("noreader");
        let path = dir.join("frames.rgba");
        make_fifo(&path);
        let err = FrameStream::create(path.to_str().unwrap(), 1)
            .err()
            .expect("no reader must fail");
        assert!(err.contains("failed to open frame stream"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_reader_that_exits_fails_the_recording() {
        use std::os::unix::fs::OpenOptionsExt;
        let dir = temp_dir("reader_exits");
        let path = dir.join("frames.rgba");
        make_fifo(&path);
        let reader = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .unwrap();
        let mut stream = FrameStream::create(path.to_str().unwrap(), 2).unwrap();
        drop(reader);
        let mut failure = None;
        for _ in 0..8 {
            let step = stream
                .next_buffer(64, 64)
                .and_then(|buffer| stream.submit(buffer, RowOrigin::BottomUp));
            if let Err(e) = step {
                failure = Some(e);
                break;
            }
        }
        let err = failure.expect("writing to a closed FIFO must fail a later frame");
        assert!(err.contains("write failed"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
