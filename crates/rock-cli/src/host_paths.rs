use anyhow::Result;
use std::path::{Path, PathBuf};

pub(crate) fn local_work_root(home: &Path) -> Result<PathBuf> {
    gamelibs::local_work_root(home).map_err(anyhow::Error::msg)
}

#[cfg(test)]
pub(crate) fn assert_cache_default(
    test: &str,
    cache: &str,
    root: impl Fn() -> Result<Option<PathBuf>>,
) {
    if let Ok(expected) = std::env::var("ROCK_STORAGE_TEST_EXPECTED") {
        assert_eq!(root().unwrap().unwrap(), PathBuf::from(expected));
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let cases = [
        (home.path().to_path_buf(), None),
        (home.path().join("volume"), Some(home.path().join("volume"))),
    ];
    std::fs::create_dir(home.path().join("volume")).unwrap();
    for (base, work_root) in cases {
        let expected = base.join("tmp").join(cache);
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", test, "--nocapture"])
            .env("HOME", home.path())
            .env("ROCK_STORAGE_TEST_EXPECTED", &expected)
            .env_remove("ROCK_FRAME_COMPILE_CACHE")
            .env_remove("ROCK_FRAME_COMPILE_CACHE_DIR")
            .env_remove("ROCK_SIM_COMPILE_CACHE")
            .env_remove("ROCK_SIM_COMPILE_CACHE_DIR")
            .env_remove("SKULL_ALLOCATION_REPORT")
            .env_remove("SKULL_ALLOCATION_ACTIVATION");
        // The home-default case must not inherit an override from this process.
        match work_root {
            Some(dir) => {
                command.env("ROCK_WORK_ROOT", dir);
            }
            None => {
                command.env_remove("ROCK_WORK_ROOT");
            }
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rule is an explicit absolute, existing `ROCK_WORK_ROOT`, else home;
    /// a missing directory is an error, never created (an unmounted volume
    /// must not fill the system disk). Each case
    /// runs in a spawned copy of this test with an explicit environment, so no
    /// test mutates this process's environment.
    #[test]
    fn work_root_rule() {
        if let Ok(expected) = std::env::var("ROCK_WORK_ROOT_TEST_EXPECTED") {
            assert_eq!(
                local_work_root(Path::new("/author")).unwrap(),
                PathBuf::from(expected)
            );
            return;
        }
        if std::env::var("ROCK_WORK_ROOT_TEST_REJECTED").is_ok() {
            assert!(local_work_root(Path::new("/author")).is_err());
            return;
        }
        let exe = std::env::current_exe().unwrap();
        let volume = tempfile::tempdir().unwrap();
        let volume = volume.path().to_str().unwrap();
        for (work_root, expected) in [(None, "/author"), (Some(volume), volume)] {
            let mut command = std::process::Command::new(&exe);
            command
                .args([
                    "--exact",
                    "host_paths::tests::work_root_rule",
                    "--nocapture",
                ])
                .env("ROCK_WORK_ROOT_TEST_EXPECTED", expected);
            match work_root {
                Some(dir) => {
                    command.env("ROCK_WORK_ROOT", dir);
                }
                None => {
                    command.env_remove("ROCK_WORK_ROOT");
                }
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let missing = format!("{volume}/unmounted");
        for rejected in ["relative/root", missing.as_str()] {
            let output = std::process::Command::new(&exe)
                .args([
                    "--exact",
                    "host_paths::tests::work_root_rule",
                    "--nocapture",
                ])
                .env("ROCK_WORK_ROOT", rejected)
                .env("ROCK_WORK_ROOT_TEST_REJECTED", "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{rejected}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}
