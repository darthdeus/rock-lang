//! Mechanism J of `docs/nobox-case-matrix.md`: a `nobox` global is published to
//! the reflection registry by its PHYSICAL leaf names, so every consumer of
//! `_G.__ROCK_GLOBALS` sees `__nbg_SPOT_0` / `__nbg_SPOT_1` where an unmarked
//! global gives the logical `SPOT`.
//!
//! SENTINEL, per `docs/test-sentinels.md`: the assertions below encode today's
//! WRONG behaviour so they pass now and go RED when J lands. WHEN FIXED: flip
//! `marked_global_leaks_physical_leaf_names` to assert the logical shape (the
//! control already spells what that looks like) and drop the `_leaks` suffix.
//!
//! Four consumers read that registry as raw name strings, so this is not a
//! dump-state cosmetic -- `stone/engine/serialization.rock:434`,
//! `stone/engine/snapshot.rock:104` + `:138`, and
//! `stone/engine/replay.rock:545` + `:595` all do `_G[name]`. A dump-only patch
//! would leave physical leaves in saves and replay checkpoints.
//!
//! Heavy: spawns `rock frame` against a generated project. Excluded from the
//! default nextest profile; runs under `-P full`.

use std::path::Path;
use std::process::Command;

/// The REPO root, two levels up from `crates/rock-cli` -- `stick.toml` dependency
/// paths and workspace discovery are both resolved against it. One `.parent()`
/// lands in `crates/` and the run fails with "Dependency 'stone' ... does not
/// contain a stick.toml".
fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("repo root")
}

/// A one-global project. `marker` prefixes the decl: "" or "nobox ". The registry is keyed by PACKAGE and
/// `build_rock_globals_registry` bails when the package map is empty, so a real
/// `stick.toml` is required -- a single `.rock` file reproduces nothing. The run
/// must also start the engine (`stone_run_game`), or no dump is produced at all.
fn dump_one_global_project(pkg: &str, marker: &str) -> serde_json::Value {
    dump_project(pkg, marker, "", 1)
}

/// As above, plus `extra` Rock decls and a frame count. `snapshot_take` reaches
/// engine FFI, so anything calling it must run from a frame hook (`@update`),
/// never from `main` before `stone_run_game` -- that fails with "attempt to
/// index global 'stone' (a nil value)".
fn dump_project(pkg: &str, marker: &str, extra: &str, frames: u32) -> serde_json::Value {
    let dir = tempfile::tempdir().expect("project dir");
    let stone = workspace_root().join("stone");
    std::fs::write(
        dir.path().join("stick.toml"),
        format!(
            "[package]\nname = \"{pkg}\"\nversion = \"0.1.0\"\ntype = \"bin\"\n\n\
             [dependencies]\nstone = {{ path = \"{}\" }}\n",
            stone.display()
        ),
    )
    .expect("write stick.toml");
    std::fs::write(
        dir.path().join("main.rock"),
        format!(
            "{marker}value Probe {{ x: int, y: int }}\n\n\
             global SPOT: Probe = Probe {{ x: 3, y: 4 }}\n\n\
             {extra}\n\
             fn main() {{\n    \
             stone_run_game(GameConfig {{ title: \"j probe\", width: 64, height: 64 }})\n\
             }}\n"
        ),
    )
    .expect("write main.rock");
    let frames = frames.to_string();

    let scratch = tempfile::tempdir().expect("scratch dir");
    let output = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame",
            &dir.path().to_string_lossy(),
            "-n",
            &frames,
            "--dump-state",
            "--output",
            &scratch.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        // Workspace discovery walks up from CWD, so the spawn must run from the
        // repo root even though the project lives in a tempdir.
        .current_dir(workspace_root())
        .output()
        .expect("spawn rock frame");
    assert!(
        output.status.success(),
        "rock frame exited {:?}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let all: serde_json::Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|e| panic!("dump-state was not JSON ({e})"));
    all.get(pkg)
        .unwrap_or_else(|| panic!("no `{pkg}` package in the dump: {all:#}"))
        .clone()
}

/// The CONTROL, and the shape the sentinel should be flipped to: an unmarked
/// value global reflects logically, under its own name, with its fields.
#[test]
#[ignore = "spawns `rock frame` against a generated project (-P full)"]
fn unmarked_global_reflects_logically() {
    let pkg = dump_one_global_project("value_dump_control", "");
    assert_eq!(
        pkg,
        serde_json::json!({ "SPOT": { "__ty": "Probe", "x": 3, "y": 4 } }),
        "an unmarked value global must reflect under its logical name"
    );
}

#[test]
#[ignore = "spawns `rock frame` against a generated project (-P full)"]
fn marked_global_leaks_physical_leaf_names() {
    let pkg = dump_one_global_project("nobox_dump_probe", "nobox ");
    // PINNED BUG. `skull_backend.rs` extends the registry with
    // `nobox_global_leaves(..)` rather than the base name; its comment argues the
    // base name "would dump nil and restore nothing", which is true and is
    // exactly the unbuilt alternative -- RECONSTRUCT the logical value. If this
    // assertion fails, J landed: flip it to the control's shape above.
    assert_eq!(
        pkg,
        serde_json::json!({ "__nbg_SPOT_0": 3, "__nbg_SPOT_1": 4 }),
        "PINNED BEHAVIOUR (BUG): a nobox global reflects as physical leaves. If \
         this now reads {{\"SPOT\": {{..}}}}, the fix landed -- flip the assertion."
    );
}

/// A snapshot round-trip over a `nobox` global, driven from a frame hook: take,
/// clobber, restore, read the logical field back.
///
/// This is the property J's fix ENDANGERS, not a bug -- it passes today and must
/// keep passing. Restore works right now precisely BECAUSE the registry publishes
/// physical leaves: they are real `_G` entries, so the consumers' `_G[name]` read
/// and write hit the actual storage. Moving the registry to the logical name makes
/// the reconstruct/destructure accessors load-bearing for exactly this path, so a
/// fix that only relabels the dump would break it.
#[test]
#[ignore = "spawns `rock frame` against a generated project (-P full)"]
fn marked_global_snapshot_round_trips() {
    let pkg = dump_project(
        "nobox_snap_probe",
        "nobox ",
        r#"global WITNESS: int = -1

@update
fn on_update(dt: float) {
    if WITNESS == -1 {
        snapshot_take(1)
        SPOT = Probe { x: 99, y: 99 }
        if snapshot_restore(1) {
            WITNESS = materialize(SPOT).x
        } else {
            WITNESS = -2
        }
    }
}
"#,
        3,
    );
    assert_eq!(
        pkg.get("WITNESS"),
        Some(&serde_json::json!(3)),
        "a nobox global must survive snapshot take/restore: expected the restored \
         x == 3 (-2 means restore reported no keyframe; 99 means it silently did \
         not restore). Got package dump: {pkg:#}"
    );
}

/// The keyframe itself stores the registry's names, so the leak reaches persisted
/// replay checkpoints and saves -- not just `--dump-state`.
///
/// SENTINEL: pins today's physical-leaf keys. WHEN J LANDS: expect `SPOT` here
/// instead of the two `__nbg_SPOT_*` entries, and drop the `_leaks` suffix.
#[test]
#[ignore = "spawns `rock frame` against a generated project (-P full)"]
fn marked_global_keyframe_leaks_physical_leaf_names() {
    let pkg = dump_project(
        "nobox_kf_probe",
        "nobox ",
        r#"global KEYNAMES: string = "unset"

@nomangle inline(lua) fn __probe_keyframe_keys(tick: int) -> string = """
    local kf = _G.__stone_keyframes and _G.__stone_keyframes[tick]
    if not kf then return "no-keyframe" end
    local ks = {}
    for k, _ in pairs(kf.globals) do ks[#ks+1] = k end
    table.sort(ks)
    return table.concat(ks, ",")
"""

@update
fn on_update(dt: float) {
    if KEYNAMES == "unset" {
        snapshot_take(1)
        KEYNAMES = __probe_keyframe_keys(1)
    }
}
"#,
        3,
    );
    let keys = pkg
        .get("KEYNAMES")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("no KEYNAMES in the dump: {pkg:#}"))
        .split(',')
        .filter(|k| k.contains("SPOT"))
        .collect::<Vec<_>>();
    assert_eq!(
        keys,
        vec!["__nbg_SPOT_0", "__nbg_SPOT_1"],
        "PINNED BEHAVIOUR (BUG): the keyframe records physical leaf names, so the \
         leak is persisted. If this now reads [\"SPOT\"], J landed -- flip it."
    );
}
