//! A game command builds and stages the engine cdylibs it is about to `ffi.load`.
//!
//! Nothing else guards this. `build_engine_libs` sits at the top of every
//! sim/frame/run entry point, and if a refactor dropped one the loss would be
//! silent in the worst way: the game still runs, against the PREVIOUS binary, and
//! reports a clean pass. That is the shape `CLAUDE.md` warns about under Running
//! Things -- a mutation that "did not reproduce" is a stale-build suspect first --
//! and a whole backlog entry (shaders 38) was filed on the belief that it had
//! already happened. It had not; what was missing was anything that would notice
//! if it did.
//!
//! The OPTIONAL lib is the subject, not `stone-rs`. Core is loaded by every game,
//! so an unbuilt one fails loudly everywhere at once; an optional cdylib is
//! selected through `[engine] needs` and `OPTIONAL_ENGINE_LIBS`, the path with
//! somewhere to go wrong. This gate uses a minimal temporary game fixture.
//!
//! Proving the BUILD ran and not merely the stage takes deleting both ends: a
//! stage-only survivor would copy a months-old `target/debug` artifact into
//! `tmp/` and satisfy any check that only looked at `tmp/`.
//!
//! `#[ignore]`d, and it must stay that way. It deletes artifacts in the shared
//! workspace `target/debug` and `tmp/`, so a neighbour staging the same cdylib
//! concurrently would race it -- and rebuilding stone-wgpu from scratch is ~25s,
//! which is not a default-profile cost. Run it deliberately:
//!
//!   cargo test -p rock-cli --test rock_cli_heavy -- --ignored --nocapture engine_cdylib

use std::path::{Path, PathBuf};

/// `libfoo.so` / `libfoo.dylib` / `foo.dll`, matching `stage_cdylib`'s naming.
/// Restated here on purpose: a helper shared with the code under test would
/// agree with it by construction, including about a name neither one produces.
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

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap().to_path_buf()
}

#[test]
#[ignore = "deletes shared workspace artifacts and rebuilds a crate; run deliberately"]
fn a_game_command_rebuilds_and_stages_the_optional_engine_cdylib_it_declares() {
    let root = workspace_root();
    let fixture = tempfile::Builder::new()
        .prefix("engine-cdylib-fixture-")
        .tempdir()
        .expect("temporary game fixture");
    let manifest = format!(
        "[package]\nname = \"engine_cdylib_fixture\"\nversion = \"0.1.0\"\ntype = \"bin\"\n\n\
         [dependencies]\nstone = {{ path = \"{}\" }}\n\n\
         [engine]\nneeds = [\"wgpu\"]\n\n\
         [build.vars]\nGIT_SHA = \"test\"\n",
        root.join("stone").display(),
    );
    std::fs::write(fixture.path().join("stick.toml"), manifest)
        .expect("write fixture manifest");
    std::fs::write(
        fixture.path().join("main.rock"),
        concat!(
            "fn main() {\n",
            "    stone_run_game(GameConfig { title: \"engine cdylib fixture\", ",
            "width: 64, height: 64 })\n",
            "}\n"
        ),
    )
    .expect("write fixture main");

    let name = cdylib_name("stone_wgpu");
    let staged = root.join("tmp").join(&name);
    let built = root.join("target/debug").join(&name);

    // Both ends. Deleting only the staged copy would leave a check that a
    // build-less stage passes, and the stale artifact it copied is exactly the
    // thing this exists to catch.
    for p in [&staged, &built] {
        if p.exists() {
            fs_err::remove_file(p).expect("clear artifact");
        }
    }
    assert!(!built.exists() && !staged.exists(), "the fixture starts with neither artifact");

    // `frame` rather than `sim`: it is the cheapest command that goes through
    // the same `build_engine_libs`, and one frame is enough -- what is being
    // measured happens before the game starts.
    let project = fixture.path().to_string_lossy().into_owned();
    let out = crate::command::rock(root.join("target/debug/rock"))
        .args(["frame", project.as_str(), "-n", "1"])
        .current_dir(&root)
        .output()
        .expect("spawn rock frame");

    assert!(
        built.exists(),
        "`rock frame` did not BUILD the declared cdylib: {} is still absent\n{}",
        built.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        staged.exists(),
        "`rock frame` built the cdylib but did not STAGE it into tmp/, which is \
         where ffi.load looks: {} is absent\n{}",
        staged.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success(),
        "and the frame itself must run, so the staged lib is one the game can \
         actually load\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
