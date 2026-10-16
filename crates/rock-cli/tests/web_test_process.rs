use std::ffi::OsStr;
use std::path::Path;
use std::process::Command;

pub fn command(program: impl AsRef<OsStr>) -> Command {
    let mut command = if program.as_ref() == OsStr::new(env!("CARGO_BIN_EXE_rock")) {
        crate::command::rock_bin()
    } else {
        Command::new(program)
    };
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        let parent_pid = unsafe { libc::getpid() };
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != parent_pid {
                    return Err(std::io::Error::other(
                        "test parent exited before child setup",
                    ));
                }
                Ok(())
            });
        }
    }
    command
}

pub fn node() -> Command {
    command("node")
}

#[allow(dead_code)]
pub fn chrome_available(repo_root: &Path) -> bool {
    let output = node()
        .args([
            "-e",
            "const {findChrome}=require('./tools/web/chrome_session'); process.exit(findChrome()?0:3)",
        ])
        .current_dir(repo_root)
        .output()
        .expect("probe chrome through shared driver");
    match output.status.code() {
        Some(0) => true,
        Some(3) => false,
        _ => panic!(
            "Chrome probe failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        ),
    }
}
