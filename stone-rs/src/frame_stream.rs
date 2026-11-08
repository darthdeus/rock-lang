//! GL front end of stone_capture::FrameStream: one recording at a time, fed
//! from the presented frame. The frame pays the glReadPixels; row order and
//! the write run on the stream's writer thread.

use std::ffi::{c_char, c_int, CStr, CString};
use std::sync::Mutex;

use stone_capture::{FrameStream, RowOrigin};

use crate::with_gl;

// Frames a recording may hold in memory: one being read back, the rest
// queued behind a slow reader (a 4K frame is 33 MB).
const FRAME_STREAM_BUFFERS: usize = 4;

enum Recording {
    Idle,
    Live(FrameStream),
    // A frame failed; later frames are refused until finish reports it.
    Failed(String),
}

static RECORDING: Mutex<Recording> = Mutex::new(Recording::Idle);
static LAST_ERROR: Mutex<Option<CString>> = Mutex::new(None);

fn set_last_error(message: &str) {
    ::log::error!(target: "SCREENSHOT", "{message}");
    *LAST_ERROR.lock().unwrap_or_else(|e| e.into_inner()) =
        Some(CString::new(message.replace('\0', " ")).expect("NULs replaced"));
}

/// Starts recording presented frames to `path` (a FIFO with a reader, or a
/// file). Fails when a recording is already live.
#[no_mangle]
pub extern "C" fn stone_frame_stream_begin(path: *const c_char) -> bool {
    if path.is_null() {
        set_last_error("frame stream: null path");
        return false;
    }
    let path = unsafe { CStr::from_ptr(path) }
        .to_string_lossy()
        .into_owned();
    let mut recording = RECORDING.lock().unwrap_or_else(|e| e.into_inner());
    if !matches!(*recording, Recording::Idle) {
        set_last_error(&format!("frame stream {path}: a recording is already live"));
        return false;
    }
    match FrameStream::create(&path, FRAME_STREAM_BUFFERS) {
        Ok(stream) => {
            *recording = Recording::Live(stream);
            true
        }
        Err(e) => {
            set_last_error(&e);
            false
        }
    }
}

/// Reads the presented `w` x `h` frame back and queues it. Waits only when
/// every buffer is queued behind the reader.
#[no_mangle]
pub extern "C" fn stone_frame_stream_record(w: c_int, h: c_int) -> bool {
    let mut recording = RECORDING.lock().unwrap_or_else(|e| e.into_inner());
    let stream = match &mut *recording {
        Recording::Live(stream) => stream,
        Recording::Idle => {
            set_last_error("frame stream: no recording is live");
            return false;
        }
        Recording::Failed(_) => return false,
    };
    let result = record_into(stream, w, h);
    if let Err(e) = result {
        set_last_error(&e);
        *recording = Recording::Failed(e);
        return false;
    }
    true
}

fn record_into(stream: &mut FrameStream, w: c_int, h: c_int) -> Result<(), String> {
    if w <= 0 || h <= 0 {
        return Err(format!("frame stream: presented frame is {w}x{h}"));
    }
    let mut buffer = stream.next_buffer(w as u32, h as u32)?;
    with_gl(|gl| unsafe {
        gl.read_pixels(
            0,
            0,
            w,
            h,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelPackData::Slice(Some(buffer.as_mut_slice())),
        );
    })
    .ok_or_else(|| "frame stream: no GL window to read the frame from".to_string())?;
    stream.submit(buffer, RowOrigin::BottomUp)
}

/// Ends the recording: writes every queued frame and closes the stream.
/// False when any frame failed; the frame count and size are what the
/// stream holds.
#[no_mangle]
pub extern "C" fn stone_frame_stream_finish(
    out_frames: *mut c_int,
    out_width: *mut c_int,
    out_height: *mut c_int,
) -> bool {
    let mut recording = RECORDING.lock().unwrap_or_else(|e| e.into_inner());
    let summary = match std::mem::replace(&mut *recording, Recording::Idle) {
        Recording::Idle => Err("frame stream: no recording is live".to_string()),
        Recording::Failed(e) => Err(e),
        Recording::Live(stream) => stream.finish(),
    };
    let (frames, width, height, ok) = match summary {
        Ok(s) => (s.frames as c_int, s.width as c_int, s.height as c_int, true),
        Err(e) => {
            set_last_error(&e);
            (0, 0, 0, false)
        }
    };
    unsafe {
        if !out_frames.is_null() {
            *out_frames = frames;
        }
        if !out_width.is_null() {
            *out_width = width;
        }
        if !out_height.is_null() {
            *out_height = height;
        }
    }
    ok
}

/// The last frame stream failure, "" when there was none.
#[no_mangle]
pub extern "C" fn stone_frame_stream_last_error() -> *const c_char {
    let last = LAST_ERROR.lock().unwrap_or_else(|e| e.into_inner());
    match last.as_ref() {
        // The CString lives in the static until the next failure replaces it.
        Some(s) => s.as_ptr(),
        None => c"".as_ptr(),
    }
}
