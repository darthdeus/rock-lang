//! Host facts for a player's system report: OS and version, CPU, GL driver
//! strings, the displays and which one the window is on. Rock has no portable
//! way to read these, so the engine assembles the lines; the game adds its own
//! sections around them. Parsing is pure and unit-tested below; the queries
//! that can fail say so in the report and log once.

use std::cell::RefCell;
use std::ffi::{c_char, CStr, CString};
use std::sync::Mutex;

/// GL renderer, vendor and version strings, captured once when the context
/// is created (the only moment the engine is certain it is current).
static GL_STRINGS: Mutex<Option<(String, String, String)>> = Mutex::new(None);

pub(crate) fn record_gl_strings(renderer: String, vendor: String, version: String) {
    match GL_STRINGS.lock() {
        Ok(mut slot) => *slot = Some((renderer, vendor, version)),
        Err(_) => {
            ::log::error!(target: "STONE", "GL strings lock poisoned; the system report will omit them")
        }
    }
}

fn gl_strings() -> Option<(String, String, String)> {
    GL_STRINGS.lock().ok().and_then(|slot| slot.clone())
}

pub struct DisplayLine {
    pub index: i32,
    pub name: String,
    pub bounds: (i32, i32, i32, i32),
    pub mode: Option<(i32, i32, i32)>,
}

// Linux production parsing plus the unit tests below; other platforms read
// neither /etc/os-release nor /proc/cpuinfo.
#[cfg(any(target_os = "linux", test))]
/// `PRETTY_NAME` from an os-release file.
pub fn os_release_pretty_name(contents: &str) -> Option<String> {
    contents.lines().find_map(|line| {
        let value = line.strip_prefix("PRETTY_NAME=")?;
        Some(value.trim().trim_matches('"').to_string())
    })
}

#[cfg(any(target_os = "linux", test))]
/// The first `model name` from /proc/cpuinfo.
pub fn cpuinfo_model_name(contents: &str) -> Option<String> {
    contents.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        (key.trim() == "model name").then(|| value.trim().to_string())
    })
}

#[cfg(not(target_os = "linux"))]
fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(program)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

#[cfg(target_os = "linux")]
fn os_version() -> Option<String> {
    let name = std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|c| os_release_pretty_name(&c));
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease").ok();
    match (name, kernel) {
        (Some(n), Some(k)) => Some(format!("{n} (kernel {})", k.trim())),
        (Some(n), None) => Some(n),
        (None, Some(k)) => Some(format!("kernel {}", k.trim())),
        (None, None) => None,
    }
}

#[cfg(target_os = "macos")]
fn os_version() -> Option<String> {
    command_output("sw_vers", &["-productVersion"]).map(|v| format!("macOS {v}"))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn os_version() -> Option<String> {
    command_output("cmd", &["/C", "ver"])
}

#[cfg(target_os = "linux")]
fn cpu_name() -> Option<String> {
    std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|c| cpuinfo_model_name(&c))
}

#[cfg(target_os = "macos")]
fn cpu_name() -> Option<String> {
    command_output("sysctl", &["-n", "machdep.cpu.brand_string"])
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn cpu_name() -> Option<String> {
    std::env::var("PROCESSOR_IDENTIFIER").ok()
}

fn or_unknown(what: &str, value: Option<String>) -> String {
    value.unwrap_or_else(|| {
        crate::log_once!("STONE", "system report: {} unknown on this host", what);
        "unknown".to_string()
    })
}

/// The engine's section of the report, one `key: value` per line.
pub fn render(displays: &[DisplayLine], window_display: Option<i32>, video_driver: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "os: {} {}\n",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    out.push_str(&format!(
        "os_version: {}\n",
        or_unknown("OS version", os_version())
    ));
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0);
    out.push_str(&format!(
        "cpu: {} ({} threads)\n",
        or_unknown("CPU", cpu_name()),
        threads
    ));
    match gl_strings() {
        Some((renderer, vendor, version)) => {
            out.push_str(&format!("gl_renderer: {renderer}\n"));
            out.push_str(&format!("gl_vendor: {vendor}\n"));
            out.push_str(&format!("gl_version: {version}\n"));
        }
        None => out.push_str("gl_renderer: unknown (no GL context)\n"),
    }
    out.push_str(&format!("video_driver: {video_driver}\n"));
    for d in displays {
        let mode = match d.mode {
            Some((w, h, hz)) => format!("{w}x{h} @ {hz} Hz"),
            None => "mode unknown".to_string(),
        };
        let here = if window_display == Some(d.index) {
            " (window)"
        } else {
            ""
        };
        out.push_str(&format!(
            "display {}: {} {} at {},{} size {}x{}{}\n",
            d.index, d.name, mode, d.bounds.0, d.bounds.1, d.bounds.2, d.bounds.3, here
        ));
    }
    if displays.is_empty() {
        out.push_str("displays: none reported\n");
    }
    out
}

thread_local! {
    static REPORT: RefCell<CString> = RefCell::new(CString::default());
}

/// The engine section of the system report. The pointer stays valid until the
/// next call on this thread; do not free.
#[no_mangle]
pub extern "C" fn stone_system_report() -> *const c_char {
    let (displays, window_display) = crate::WINDOW.with(|w| {
        let w = w.borrow();
        let Some(window) = w.as_ref() else {
            return (Vec::new(), None);
        };
        let video = &window._video;
        let n = video.num_video_displays().unwrap_or(0);
        let mut lines = Vec::new();
        for i in 0..n {
            lines.push(DisplayLine {
                index: i,
                name: video.display_name(i).unwrap_or_else(|_| "?".to_string()),
                bounds: video
                    .display_bounds(i)
                    .map(|r| (r.x, r.y, r.width() as i32, r.height() as i32))
                    .unwrap_or((0, 0, 0, 0)),
                mode: video
                    .current_display_mode(i)
                    .ok()
                    .map(|m| (m.w, m.h, m.refresh_rate)),
            });
        }
        (lines, window.window.display_index().ok())
    });
    let driver = unsafe {
        let p = sdl2::sys::SDL_GetCurrentVideoDriver();
        if p.is_null() {
            "none".to_string()
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    };
    let text = render(&displays, window_display, &driver);
    REPORT.with(|r| {
        *r.borrow_mut() = CString::new(text.replace('\0', "")).unwrap_or_default();
        r.borrow().as_ptr()
    })
}

/// Puts `text` on the system clipboard through SDL, which owns the selection
/// for the life of the window (an arboard handle would drop it on Linux).
/// Returns 1 on success; a failure is logged with SDL's reason.
#[no_mangle]
pub extern "C" fn stone_set_clipboard_text(text: *const c_char) -> u8 {
    if text.is_null() {
        ::log::error!(target: "STONE", "stone_set_clipboard_text: null text");
        return 0;
    }
    let rc = unsafe { sdl2::sys::SDL_SetClipboardText(text) };
    if rc != 0 {
        ::log::error!(target: "STONE", "SDL_SetClipboardText failed: {}", sdl2::get_error());
        return 0;
    }
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_release_pretty_name_strips_quotes() {
        let c = "NAME=NixOS\nPRETTY_NAME=\"NixOS 25.11 (Xantusia)\"\nID=nixos\n";
        assert_eq!(
            os_release_pretty_name(c).as_deref(),
            Some("NixOS 25.11 (Xantusia)")
        );
        assert_eq!(os_release_pretty_name("ID=x\n"), None);
    }

    #[test]
    fn cpuinfo_model_name_takes_the_first_core() {
        let c = "processor\t: 0\nmodel name\t: AMD Ryzen 9 7950X 16-Core Processor\n\nprocessor\t: 1\nmodel name\t: other\n";
        assert_eq!(
            cpuinfo_model_name(c).as_deref(),
            Some("AMD Ryzen 9 7950X 16-Core Processor")
        );
        assert_eq!(cpuinfo_model_name("processor : 0\n"), None);
    }

    #[test]
    fn render_marks_the_window_display_and_unknown_modes() {
        let displays = vec![
            DisplayLine {
                index: 0,
                name: "A".into(),
                bounds: (0, 0, 3840, 2160),
                mode: Some((3840, 2160, 60)),
            },
            DisplayLine {
                index: 1,
                name: "B".into(),
                bounds: (3840, 0, 1920, 1080),
                mode: None,
            },
        ];
        let text = render(&displays, Some(1), "x11");
        assert!(
            text.contains("display 0: A 3840x2160 @ 60 Hz at 0,0 size 3840x2160\n"),
            "{text}"
        );
        assert!(
            text.contains("display 1: B mode unknown at 3840,0 size 1920x1080 (window)\n"),
            "{text}"
        );
        assert!(text.contains("video_driver: x11\n"));
        assert!(text.starts_with("os: "));
    }
}
