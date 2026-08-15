use crate::web_test_process;

#[cfg(target_os = "linux")]
fn wait_until(mut predicate: impl FnMut() -> bool, timeout: std::time::Duration, subject: &str) {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if predicate() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("timed out waiting for {subject}");
}

#[test]
fn chrome_session_cleans_up_on_sigterm() {
    if web_test_process::node()
        .arg("--version")
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
    {
        eprintln!("SKIP chrome_session_cleans_up_on_sigterm: node not on PATH");
        return;
    }

    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = manifest_dir.parent().unwrap().parent().unwrap();
    let output = web_test_process::node()
        .args([
            "--test",
            "--test-reporter=tap",
            "tools/web/chrome_session.test.js",
        ])
        .current_dir(root)
        .output()
        .expect("run chrome session tests");
    assert!(
        output.status.success()
            && String::from_utf8_lossy(&output.stdout)
                .contains("SIGTERM kills Chrome and removes its profile")
            && String::from_utf8_lossy(&output.stdout).contains("# tests 3"),
        "chrome session tests failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[cfg(target_os = "linux")]
#[test]
fn web_child_dies_with_its_test_parent() {
    use std::os::unix::process::ExitStatusExt;

    let dir = tempfile::tempdir().expect("tempdir");
    let ready = dir.path().join("ready");
    let mut parent = web_test_process::command(std::env::current_exe().unwrap())
        .args(["--exact", "web_driver_tests::pdeath_fixture", "--ignored"])
        .env("ROCK_PDEATH_READY", &ready)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn fixture parent");
    let mut child_pid = None;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        wait_until(
            || ready.is_file(),
            std::time::Duration::from_secs(3),
            "fixture pid",
        );
        child_pid = Some(
            std::fs::read_to_string(&ready)
                .unwrap()
                .trim()
                .parse::<i32>()
                .unwrap(),
        );
        unsafe { libc::kill(parent.id() as i32, libc::SIGKILL) };
        let status = parent.wait().expect("wait fixture parent");
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        wait_until(
            || unsafe { libc::kill(child_pid.unwrap(), 0) } == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH),
            std::time::Duration::from_secs(3),
            "fixture child death",
        );
    }));
    if result.is_err() {
        if let Some(pid) = child_pid {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
    if parent.try_wait().ok().flatten().is_none() {
        let _ = parent.kill();
        let _ = parent.wait();
    }
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[cfg(target_os = "linux")]
#[test]
#[ignore]
fn pdeath_fixture() {
    let Ok(ready) = std::env::var("ROCK_PDEATH_READY") else {
        return;
    };
    let child = web_test_process::command("sleep")
        .arg("30")
        .spawn()
        .expect("spawn child");
    let temporary = format!("{ready}.tmp");
    std::fs::write(&temporary, child.id().to_string()).unwrap();
    std::fs::rename(temporary, ready).unwrap();
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}
