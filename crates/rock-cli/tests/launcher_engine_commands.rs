#[cfg(unix)]
mod unix {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("rock-cli crate has a grandparent")
            .to_path_buf()
    }

    fn executable(path: &Path, source: &str) {
        fs::write(path, source).expect("write executable fixture");
        let mut permissions = fs::metadata(path).expect("fixture metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).expect("make fixture executable");
    }

    #[test]
    fn replay_builds_engine_and_cli_together_before_direct_exec() {
        let fixture = tempfile::tempdir().expect("launcher fixture");
        let root = fixture.path();
        let bin = root.join("bin");
        let target = root.join("target/debug");
        fs::create_dir_all(&bin).expect("fake bin directory");
        fs::create_dir_all(&target).expect("fake target directory");
        fs::write(root.join("Cargo.toml"), "[workspace]\n").expect("fixture manifest");

        let log = root.join("launch.log");
        executable(
            &bin.join("cargo"),
            "#!/bin/sh\nprintf 'cargo:%s\\n' \"$*\" >> \"$ROCK_LAUNCH_TEST_LOG\"\n",
        );
        executable(
            &target.join("rock"),
            "#!/bin/sh\nprintf 'rock:%s\\n' \"$*\" >> \"$ROCK_LAUNCH_TEST_LOG\"\n",
        );

        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").expect("test PATH")
        );
        let status = Command::new("bash")
            .arg(workspace_root().join("rock.sh"))
            .args(["replay", "mag", "mag/replays/last.replay"])
            .current_dir(root)
            .env("PATH", path)
            .env("CARGO_TARGET_DIR", root.join("target"))
            .env("ROCK_LAUNCH_TEST_LOG", &log)
            .status()
            .expect("run rock.sh replay fixture");

        assert!(status.success(), "rock.sh replay fixture failed: {status}");
        assert_eq!(
            fs::read_to_string(log).expect("launcher log"),
            "cargo:build --lib --bin rock\nrock:replay mag mag/replays/last.replay\n"
        );
    }
}
