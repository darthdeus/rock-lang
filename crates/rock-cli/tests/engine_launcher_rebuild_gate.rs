//! A no-change game command must not relink the engine cdylibs.
//!
//! A game command is two cargo builds: `rock.sh` builds the CLI, then
//! `build_engine_libs` builds each engine cdylib with its own `-p` selection.
//! A cdylib is uplifted to one unhashed path and cargo keeps one fingerprint
//! for it, so if the launcher's selection also builds that cdylib, the two
//! selections unify dependency features differently (`log/std`,
//! `serde/derive`, ...) and each invocation dirties the other's artifact with
//! `UnitDependencyInfoChanged`. Every sim then relinked `stone-rs` and
//! `stone-agent` twice before running.
//!
//! Drives the real launcher and the production `build_engine_libs` twice and
//! asserts the second round leaves each cdylib's mtime alone.
//!
//! `#[ignore]`d: it runs real cargo builds against the shared workspace
//! `target/debug` and `tmp/`. Run deliberately:
//!
//!   cargo test -p rock-cli --test rock_cli_heavy -- --ignored --nocapture \
//!     engine_launcher_rebuild

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap().to_path_buf()
}

fn cdylib_name(stem: &str) -> String {
    #[cfg(target_os = "windows")]
    {
        format!("{stem}.dll")
    }
    #[cfg(target_os = "macos")]
    {
        format!("lib{stem}.dylib")
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        format!("lib{stem}.so")
    }
}

/// `rock.sh frame --help`: the launcher's engine-command build, then an exec
/// of the CLI that exits without touching the engine.
fn launcher_build(root: &Path) {
    let out = Command::new("bash")
        .arg(root.join("rock.sh"))
        .args(["frame", "--help"])
        .current_dir(root)
        .output()
        .expect("spawn rock.sh");
    assert!(
        out.status.success(),
        "rock.sh frame --help failed\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn mtimes(paths: &[PathBuf]) -> Vec<SystemTime> {
    paths
        .iter()
        .map(|path| {
            fs_err::metadata(path)
                .and_then(|meta| meta.modified())
                .unwrap_or_else(|error| panic!("{} has no mtime: {error}", path.display()))
        })
        .collect()
}

#[test]
#[ignore = "runs real cargo builds against shared workspace artifacts; run deliberately"]
fn a_no_change_game_command_does_not_relink_the_engine_cdylibs() {
    let root = workspace_root();
    let manifest = fs_err::read_to_string(root.join("mag/stick.toml")).expect("mag stick.toml");
    assert!(
        manifest.contains("\"agent\""),
        "this gate covers an optional default-member cdylib through mag's \
         `[engine] needs = [\"agent\"]`; point it at whichever game declares one"
    );
    let target = root.join("target/debug");
    let built = [target.join(cdylib_name("stone_rs")), target.join(cdylib_name("stone_agent"))];

    // The harness exports CARGO_MANIFEST_DIR, CARGO_PKG_NAME etc. at runtime.
    // ring's build script reruns on them, so building with them set would
    // leave ring (and stone-rs) to rebuild on the user's next plain-shell
    // command. Build as that shell does.
    let saved: Vec<_> = std::env::vars_os()
        .filter(|(name, _)| {
            let name = name.to_string_lossy();
            name.starts_with("CARGO_PKG_")
                || [
                    "CARGO_MANIFEST_DIR",
                    "CARGO_MANIFEST_PATH",
                    "CARGO_CRATE_NAME",
                    "CARGO_PRIMARY_PACKAGE",
                    "CARGO_TARGET_TMPDIR",
                    "CARGO_TARGET_DIR",
                    "ROCK_ENGINE_BUILD_RECEIPT",
                ]
                .contains(&name.as_ref())
        })
        .collect();
    for (name, _) in &saved {
        unsafe { std::env::remove_var(name) };
    }
    let round = || {
        launcher_build(&root);
        rock_host::engine_build::build_engine_libs(&root.join("mag").to_string_lossy())
    };
    let first = round();
    let before = first.as_ref().ok().map(|_| mtimes(&built));
    let second = round();
    for (name, value) in saved {
        unsafe { std::env::set_var(name, value) };
    }
    first.expect("first build_engine_libs");
    second.expect("second build_engine_libs");

    let after = mtimes(&built);
    for ((path, before), after) in built.iter().zip(before.unwrap()).zip(after) {
        assert_eq!(
            before,
            after,
            "{} was relinked by a no-change launcher + build_engine_libs round",
            path.display()
        );
    }
}
