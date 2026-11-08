//! Exercise Stone's real macOS teardown while task-owned AppKit processes
//! witness which process is frontmost. A standalone example keeps all Cocoa
//! work on the process main thread and does not synthesize input.

fn main() {
    #[cfg(target_os = "macos")]
    macos::run();
    #[cfg(not(target_os = "macos"))]
    panic!("macos_focus_teardown is macOS-only");
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::OsString;
    use std::fs;
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use std::thread;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use stone_rs::{
        stone_activate_window, stone_create_window_ex, stone_init, stone_is_window_hidden,
        stone_teardown,
    };

    #[link(name = "AppKit", kind = "framework")]
    extern "C" {}

    struct RestoreEnvironment(Vec<(&'static str, Option<OsString>)>);

    impl RestoreEnvironment {
        fn new(keys: &[&'static str]) -> Self {
            Self(
                keys.iter()
                    .map(|key| (*key, std::env::var_os(key)))
                    .collect(),
            )
        }
    }

    impl Drop for RestoreEnvironment {
        fn drop(&mut self) {
            for (key, value) in self.0.drain(..) {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    struct TestArtifacts(PathBuf);

    impl TestArtifacts {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock")
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "rock-focus-teardown-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir(&root).expect("create focus test workspace");
            Self(root)
        }

        fn build_witness(&self) -> PathBuf {
            let source =
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/macos_focus_witness.m");
            let executable = self.0.join("focus-witness");
            let output = Command::new("clang")
                .arg("-fobjc-arc")
                .arg(&source)
                .args(["-framework", "AppKit", "-o"])
                .arg(&executable)
                .output()
                .expect("compile task-owned AppKit witness");
            assert!(
                output.status.success(),
                "AppKit witness compile failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            executable
        }
    }

    impl Drop for TestArtifacts {
        fn drop(&mut self) {
            if let Err(error) = fs::remove_dir_all(&self.0) {
                eprintln!("focus test artifact cleanup failed: {error}");
            }
        }
    }

    struct Witness {
        pid: i32,
        child: Child,
        signals: PathBuf,
    }

    struct FocusWindow {
        _window: sdl2::video::Window,
        _video: sdl2::VideoSubsystem,
        _sdl: sdl2::Sdl,
    }

    impl Witness {
        fn spawn(executable: &PathBuf, root: &PathBuf, label: &str) -> Self {
            let bundle = root.join(format!("witness-{label}.app"));
            let contents = bundle.join("Contents");
            let executable_name = format!("focus-witness-{label}");
            let app_executable = contents.join("MacOS").join(&executable_name);
            fs::create_dir_all(app_executable.parent().expect("witness executable parent"))
                .expect("create witness app bundle");
            fs::copy(executable, &app_executable).expect("install witness app executable");
            let bundle_id = format!("org.rock.tests.focus-witness-{}", label.to_lowercase());
            let plist = format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>{executable_name}</string>
<key>CFBundleIdentifier</key><string>{bundle_id}</string>
<key>CFBundleName</key><string>Focus witness {label}</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleVersion</key><string>1</string>
<key>NSPrincipalClass</key><string>NSApplication</string>
</dict></plist>
"#
            );
            fs::write(contents.join("Info.plist"), plist).expect("write witness Info.plist");

            let signals = root.join(label);
            fs::create_dir(&signals).expect("create witness signal directory");
            let child = Command::new(app_executable)
                .arg(label)
                .arg(&signals)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .expect("spawn task-owned AppKit witness");
            let pid = child.id() as i32;
            let mut witness = Self {
                pid,
                child,
                signals,
            };
            witness.wait_until_ready();
            witness
        }

        fn wait_until_frontmost(&self, label: &str) {
            wait_for_frontmost(self.pid, label);
        }

        fn wait_until_ready(&mut self) {
            let ready = self.signals.join(format!("{}.ready", self.pid));
            let deadline = Instant::now() + Duration::from_secs(8);
            loop {
                if ready.exists() {
                    let value = fs::read_to_string(&ready).expect("read witness pid");
                    let reported_pid = value.trim().parse::<i32>().expect("parse witness pid");
                    assert_eq!(reported_pid, self.pid);
                    return;
                }
                if let Some(status) = self.child.try_wait().expect("check witness process") {
                    panic!("witness exited before readiness: {status}");
                }
                assert!(Instant::now() < deadline, "witness readiness timed out");
                thread::sleep(Duration::from_millis(20));
            }
        }
    }

    impl Drop for Witness {
        fn drop(&mut self) {
            let stop = self.signals.join("stop");
            if let Err(error) = fs::write(&stop, b"stop\n") {
                eprintln!("witness stop signal failed: {error}");
            }
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match self.child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(20));
                    }
                    Ok(None) => {
                        let _ = self.child.kill();
                        let _ = self.child.wait();
                        break;
                    }
                    Err(error) => {
                        eprintln!("witness shutdown check failed: {error}");
                        let _ = self.child.kill();
                        let _ = self.child.wait();
                        break;
                    }
                }
            }
            if let Err(error) = fs::remove_dir_all(&self.signals) {
                eprintln!("witness signal cleanup failed: {error}");
            }
        }
    }

    pub fn run() {
        let _restore_environment = RestoreEnvironment::new(&[
            "SDL_VIDEODRIVER",
            "STONE_HEADLESS",
            "STONE_FOCUS_ON_SHOW",
            "STONE_COCOA_POLICY_OFF",
        ]);
        std::env::set_var("SDL_VIDEODRIVER", "cocoa");
        std::env::set_var("STONE_HEADLESS", "1");
        std::env::remove_var("STONE_FOCUS_ON_SHOW");
        std::env::remove_var("STONE_COCOA_POLICY_OFF");

        let artifacts = TestArtifacts::new();
        let witness_executable = artifacts.build_witness();
        let witness_a = Witness::spawn(&witness_executable, &artifacts.0, "A");
        witness_a.wait_until_frontmost("initial witness");

        create_stone_window("focus-steal-red", true);

        let witness_b = Witness::spawn(&witness_executable, &artifacts.0, "B");
        witness_b.wait_until_frontmost("current witness");
        assert_eq!(frontmost_pid(), witness_b.pid);
        println!(
            "focus witnesses: startup pid={}, current pid={}, Stone pid={}",
            witness_a.pid,
            witness_b.pid,
            std::process::id()
        );

        assert!(stone_teardown(), "production Stone teardown failed");
        assert_frontmost_remains(witness_b.pid, "non-owning Stone teardown");

        std::env::remove_var("STONE_HEADLESS");
        let window = create_stone_window("focus-restore-green", false);
        stone_activate_window(window);
        wait_for_frontmost(std::process::id() as i32, "Stone process");
        assert!(stone_teardown(), "production Stone teardown failed");
        wait_for_frontmost(witness_b.pid, "captured witness after owned teardown");

        let _focus_window = create_focus_window();
        activate_current_process();
        wait_for_frontmost(std::process::id() as i32, "Stone process after teardown");
        assert!(stone_teardown(), "idempotent Stone teardown failed");
        assert_frontmost_remains(
            std::process::id() as i32,
            "idempotent teardown after consuming the saved PID",
        );

        println!("macos_focus_teardown: hidden-window ownership checks passed");
    }

    fn create_stone_window(title: &str, hidden: bool) -> *mut stone_rs::StoneWindow {
        assert!(stone_init(), "Stone SDL init failed");
        let title = std::ffi::CString::new(title).expect("window title");
        let window = stone_create_window_ex(title.as_ptr(), 0, 0, 64, 64, -1, false, 0);
        assert!(!window.is_null(), "Stone could not create its Cocoa window");
        assert_eq!(
            stone_is_window_hidden(),
            hidden,
            "Stone window visibility did not match its setup"
        );
        window
    }

    fn frontmost_pid() -> i32 {
        let front = Command::new("lsappinfo")
            .arg("front")
            .output()
            .expect("query WindowServer frontmost app");
        assert!(
            front.status.success(),
            "lsappinfo front failed: {}",
            String::from_utf8_lossy(&front.stderr)
        );
        let serial = String::from_utf8_lossy(&front.stdout);
        let info = Command::new("lsappinfo")
            .args(["info", serial.trim()])
            .output()
            .expect("read WindowServer frontmost app info");
        assert!(
            info.status.success(),
            "lsappinfo info failed: {}",
            String::from_utf8_lossy(&info.stderr)
        );
        let details = String::from_utf8_lossy(&info.stdout);
        details
            .lines()
            .find_map(|line| {
                line.split_once("pid = ")
                    .and_then(|(_, value)| value.split_whitespace().next())
                    .and_then(|pid| pid.parse().ok())
            })
            .unwrap_or_else(|| panic!("frontmost app info has no pid: {details}"))
    }

    fn activate_current_process() {
        unsafe {
            let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
            assert!(!app.is_null(), "Stone NSApplication unavailable");
            let _: () = msg_send![app, activateIgnoringOtherApps: true];
        }
    }

    fn create_focus_window() -> FocusWindow {
        let sdl = sdl2::init().expect("SDL init for task-owned focus window");
        let video = sdl.video().expect("SDL video for task-owned focus window");
        let mut window = video
            .window("Stone focus teardown witness", 64, 64)
            .build()
            .expect("create task-owned focus window");
        window.raise();
        FocusWindow {
            _window: window,
            _video: video,
            _sdl: sdl,
        }
    }

    fn assert_frontmost_remains(expected: i32, label: &str) {
        // AppKit activation requests can settle after the call returns.
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let actual = frontmost_pid();
            assert_eq!(actual, expected, "{label} changed the frontmost process");
            if Instant::now() >= deadline {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_for_frontmost(expected: i32, label: &str) {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            let actual = frontmost_pid();
            if actual == expected {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{label} did not become frontmost: expected pid {expected}, got {actual}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
}
