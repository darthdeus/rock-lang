//! macOS vsync without SDL's display-link wait.
//!
//! SDL2's Cocoa swap (`Cocoa_GL_SwapWindow` in SDL_cocoaopengl.m, unchanged
//! through the SDL2 branch head) waits for a CVDisplayLink tick on a condition
//! variable with no timeout. The link stops ticking while every display
//! sleeps, so a vsynced game stops in its swap until a display wakes, and a
//! game driven over SSH or by an agent never returns. SDL's own vsync is only
//! that wait followed by an interval-0 flush, so Stone keeps SDL's interval at
//! 0 and waits for the tick here with a timeout instead.

use std::ffi::c_void;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

type CVDisplayLinkRef = *mut c_void;
type CGLContextObj = *mut c_void;
type CGLPixelFormatObj = *mut c_void;
type CVReturn = i32;
type CVOptionFlags = u64;

#[repr(C)]
struct CVTime {
    time_value: i64,
    time_scale: i32,
    flags: i32,
}

const CV_TIME_IS_INDEFINITE: i32 = 1;

type CVDisplayLinkOutputCallback = extern "C" fn(
    CVDisplayLinkRef,
    *const c_void,
    *const c_void,
    CVOptionFlags,
    *mut CVOptionFlags,
    *mut c_void,
) -> CVReturn;

#[link(name = "CoreVideo", kind = "framework")]
extern "C" {
    fn CVDisplayLinkCreateWithActiveCGDisplays(link: *mut CVDisplayLinkRef) -> CVReturn;
    fn CVDisplayLinkSetOutputCallback(
        link: CVDisplayLinkRef,
        callback: CVDisplayLinkOutputCallback,
        context: *mut c_void,
    ) -> CVReturn;
    fn CVDisplayLinkStart(link: CVDisplayLinkRef) -> CVReturn;
    fn CVDisplayLinkStop(link: CVDisplayLinkRef) -> CVReturn;
    fn CVDisplayLinkRelease(link: CVDisplayLinkRef);
    fn CVDisplayLinkSetCurrentCGDisplayFromOpenGLContext(
        link: CVDisplayLinkRef,
        context: CGLContextObj,
        pixel_format: CGLPixelFormatObj,
    ) -> CVReturn;
    fn CVDisplayLinkGetNominalOutputVideoRefreshPeriod(link: CVDisplayLinkRef) -> CVTime;
}

#[link(name = "OpenGL", kind = "framework")]
extern "C" {
    fn CGLGetCurrentContext() -> CGLContextObj;
    fn CGLGetPixelFormat(context: CGLContextObj) -> CGLPixelFormatObj;
}

static TICKS: Mutex<u64> = Mutex::new(0);
static TICKED: Condvar = Condvar::new();

extern "C" fn on_display_tick(
    _link: CVDisplayLinkRef,
    _now: *const c_void,
    _output_time: *const c_void,
    _flags_in: CVOptionFlags,
    _flags_out: *mut CVOptionFlags,
    _context: *mut c_void,
) -> CVReturn {
    let mut ticks = TICKS.lock().unwrap_or_else(|e| e.into_inner());
    *ticks = ticks.wrapping_add(1);
    TICKED.notify_all();
    0
}

fn tick_count() -> u64 {
    *TICKS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Waits until a tick after `seen` arrives or `timeout` passes. Returns false
/// on timeout.
fn wait_for_tick_after(seen: u64, timeout: Duration) -> bool {
    let ticks = TICKS.lock().unwrap_or_else(|e| e.into_inner());
    let (_ticks, result) = TICKED
        .wait_timeout_while(ticks, timeout, |t| *t == seen)
        .unwrap_or_else(|e| e.into_inner());
    !result.timed_out()
}

const FALLBACK_PERIOD: Duration = Duration::from_micros(16_667);
// No active display means no link can be created; try again this often so
// vsync resumes when a display wakes.
const LINK_RETRY: Duration = Duration::from_secs(1);

// Long enough that a live link always ticks first, short enough that a
// sleeping display paces the game at a usable rate instead of stopping it.
fn tick_timeout(link: CVDisplayLinkRef) -> Duration {
    let period = unsafe { CVDisplayLinkGetNominalOutputVideoRefreshPeriod(link) };
    let period = if period.flags & CV_TIME_IS_INDEFINITE != 0
        || period.time_scale <= 0
        || period.time_value <= 0
    {
        FALLBACK_PERIOD
    } else {
        Duration::from_secs_f64(period.time_value as f64 / period.time_scale as f64)
    };
    (period * 2).clamp(Duration::from_millis(8), Duration::from_millis(100))
}

fn create_link() -> Result<CVDisplayLinkRef, String> {
    let mut link: CVDisplayLinkRef = std::ptr::null_mut();
    let status = unsafe { CVDisplayLinkCreateWithActiveCGDisplays(&mut link) };
    if status != 0 || link.is_null() {
        return Err(format!(
            "CVDisplayLinkCreateWithActiveCGDisplays returned {status}"
        ));
    }
    let status =
        unsafe { CVDisplayLinkSetOutputCallback(link, on_display_tick, std::ptr::null_mut()) };
    if status != 0 {
        unsafe { CVDisplayLinkRelease(link) };
        return Err(format!("CVDisplayLinkSetOutputCallback returned {status}"));
    }
    Ok(link)
}

/// Vsync pacing for the one GL window. While vsync is on and no link exists
/// (no display was active when it was last tried), swaps are paced by time.
pub(crate) struct DisplayPacer {
    link: Option<CVDisplayLinkRef>,
    last_link_attempt: Option<Instant>,
    running: bool,
    display_index: Option<i32>,
    timeout: Duration,
    silent: bool,
}

impl DisplayPacer {
    pub(crate) fn new() -> Self {
        Self {
            link: None,
            last_link_attempt: None,
            running: false,
            display_index: None,
            timeout: FALLBACK_PERIOD * 2,
            silent: false,
        }
    }

    pub(crate) fn is_running(&self) -> bool {
        self.running
    }

    pub(crate) fn set_running(&mut self, on: bool) {
        if on == self.running {
            return;
        }
        self.running = on;
        if on {
            self.ensure_link();
        }
        if let Some(link) = self.link {
            let status = unsafe {
                if on {
                    CVDisplayLinkStart(link)
                } else {
                    CVDisplayLinkStop(link)
                }
            };
            if status != 0 {
                ::log::error!(target: "VIDEO", "display link {} returned {status}", if on { "start" } else { "stop" });
            }
        }
    }

    fn ensure_link(&mut self) {
        if self.link.is_some()
            || self
                .last_link_attempt
                .is_some_and(|t| t.elapsed() < LINK_RETRY)
        {
            return;
        }
        self.last_link_attempt = Some(Instant::now());
        match create_link() {
            Ok(link) => {
                self.link = Some(link);
                self.display_index = None;
                if self.running {
                    let status = unsafe { CVDisplayLinkStart(link) };
                    if status != 0 {
                        ::log::error!(target: "VIDEO", "display link start returned {status}");
                    }
                }
                ::log::info!(target: "VIDEO", "display link created; vsync follows the display");
            }
            Err(e) => {
                if !self.silent {
                    ::log::info!(target: "VIDEO", "no display link ({e}); pacing swaps at {} ms until a display is active", FALLBACK_PERIOD.as_millis());
                    self.silent = true;
                }
            }
        }
    }

    /// Blocks until the display the window is on next refreshes, or for the
    /// timeout when no display refreshes. Call just before the swap.
    pub(crate) fn wait(&mut self, display_index: Option<i32>) {
        if !self.running {
            return;
        }
        self.ensure_link();
        let Some(link) = self.link else {
            std::thread::sleep(FALLBACK_PERIOD);
            return;
        };
        if display_index.is_some() && display_index != self.display_index {
            self.follow_current_context_display(link);
            self.display_index = display_index;
        }
        let seen = tick_count();
        let ticked = wait_for_tick_after(seen, self.timeout);
        if ticked == self.silent {
            self.silent = !ticked;
            if self.silent {
                ::log::info!(target: "VIDEO", "display link stopped ticking (displays asleep?); pacing swaps by a {} ms timeout", self.timeout.as_millis());
            } else {
                ::log::info!(target: "VIDEO", "display link ticking");
            }
        }
    }

    fn follow_current_context_display(&mut self, link: CVDisplayLinkRef) {
        let status = unsafe {
            let context = CGLGetCurrentContext();
            if context.is_null() {
                ::log::error!(target: "VIDEO", "display link: no current GL context to follow");
                return;
            }
            CVDisplayLinkSetCurrentCGDisplayFromOpenGLContext(
                link,
                context,
                CGLGetPixelFormat(context),
            )
        };
        if status != 0 {
            ::log::error!(target: "VIDEO", "CVDisplayLinkSetCurrentCGDisplayFromOpenGLContext returned {status}");
        }
        self.timeout = tick_timeout(link);
    }
}

impl Drop for DisplayPacer {
    fn drop(&mut self) {
        if let Some(link) = self.link {
            unsafe {
                CVDisplayLinkStop(link);
                CVDisplayLinkRelease(link);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The tick counter is process-wide; a tick from one test would end the
    // other's wait early.
    static SERIAL: Mutex<()> = Mutex::new(());

    #[test]
    fn wait_returns_after_the_timeout_when_no_tick_comes() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let seen = tick_count();
            tx.send(wait_for_tick_after(seen, Duration::from_millis(30)))
                .ok();
        });
        let ticked = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the wait never returned without a tick");
        assert!(!ticked);
    }

    #[test]
    fn wait_returns_on_the_next_tick() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let seen = tick_count();
        let start = Instant::now();
        let ticker = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(20));
            on_display_tick(
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
        });
        assert!(wait_for_tick_after(seen, Duration::from_secs(5)));
        assert!(start.elapsed() < Duration::from_secs(5));
        ticker.join().unwrap();
    }
}
