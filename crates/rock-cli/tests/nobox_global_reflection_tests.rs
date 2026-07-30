//! Mechanism J of `docs/nobox-case-matrix.md`: marking a global `nobox` must not
//! change what reflection shows. Its storage IS its leaf globals, so the registry
//! publishes the logical name and `__ROCK_NOBOX_GLOBALS` carries the
//! rebuild/take-apart pair that `__rock_global_get`/`_set` call.
//!
//! Three properties, all of which a wrong fix breaks differently: the dump reads
//! logically (identical to the unmarked control), the persisted keyframe records
//! the logical name, and the snapshot round-trip still restores the value. The
//! last one passed BEFORE the fix -- physical leaves are real `_G` entries, so the
//! consumers' `_G[name]` reached the storage -- which makes it the property the
//! fix endangered rather than one it repaired.
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
    dump_source_project(
        pkg,
        &format!(
            "{marker}value Probe {{ x: int, y: int }}\n\n\
             global SPOT: Probe = Probe {{ x: 3, y: 4 }}\n\n{extra}"
        ),
        frames,
    )
}

/// As above over a whole `main.rock` body (`main` is appended).
fn dump_source_project(pkg: &str, source: &str, frames: u32) -> serde_json::Value {
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
            "{source}\n\
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

/// The CONTROL: an unmarked value global reflects logically, under its own name,
/// with its fields. The marked case must be indistinguishable from it.
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

/// Marking must not change what reflection shows: a `nobox` global reflects under
/// its logical name with its logical fields, exactly as the unmarked control does.
#[test]
#[ignore = "spawns `rock frame` against a generated project (-P full)"]
fn marked_global_reflects_logically() {
    let pkg = dump_one_global_project("nobox_dump_probe", "nobox ");
    assert_eq!(
        pkg,
        serde_json::json!({ "SPOT": { "__ty": "Probe", "x": 3, "y": 4 } }),
        "a nobox global must reflect logically -- identical to the unmarked control"
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

/// The keyframe stores the registry's names, so this is what reaches persisted
/// replay checkpoints and save files -- the reason a `--dump-state`-only fix would
/// not have been one.
#[test]
#[ignore = "spawns `rock frame` against a generated project (-P full)"]
fn marked_global_keyframe_records_logical_name() {
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
        vec!["SPOT"],
        "the keyframe must record the logical name, not physical leaves -- these \
         keys are persisted into replay checkpoints and saves"
    );
}

/// The reconstruct call is the positional leaf ctor, and which ctor depends on the
/// shape: dotted leaf paths mean the flattened `_lct`, a GC-tier struct means the
/// table ctor `_vt`, and a plain FFI-clean one means `_ct`. Only `_ct` is covered
/// by the flat `Probe` above, so both other tiers are pinned here -- the suffix
/// rule mirrors `skull-codegen-lua`'s pack-site choice and would otherwise be an
/// analytic claim about two paths nothing exercises.
///
/// `Mid` is nested (leaf paths `inner.v`, `m` -> `_lct`); `Opt` carries a nullable
/// nested field, which puts it on the GC tier (paths `inner`, `m` -> `_vt`). Both
/// are checked in both directions: the dump reads logically, and a snapshot
/// round-trip through the accessors restores the value.
#[test]
#[ignore = "spawns `rock frame` against a generated project (-P full)"]
fn nested_and_gc_tier_nobox_globals_reflect_and_round_trip() {
    let pkg = dump_source_project(
        "nobox_tier_probe",
        r#"value Leaf { v: int }
nobox value Mid { inner: Leaf, m: int }
nobox value Opt { inner: Leaf?, m: int }

global NEST: Mid = Mid { inner: Leaf { v: 1 }, m: 2 }
global ONUL: Opt = Opt { inner: Leaf { v: 5 }, m: 6 }
global RT_NEST: int = -1
global RT_ONUL: int = -1

@update
fn on_update(dt: float) {
    if RT_NEST == -1 {
        snapshot_take(1)
        NEST = Mid { inner: Leaf { v: 77 }, m: 88 }
        ONUL = Opt { inner: Leaf { v: 77 }, m: 88 }
        if snapshot_restore(1) {
            RT_NEST = materialize(NEST).m
            RT_ONUL = materialize(ONUL).m
        }
    }
}
"#,
        3,
    );
    assert_eq!(
        pkg.get("NEST"),
        Some(&serde_json::json!({ "__ty": "Mid", "inner": { "__ty": "Leaf", "v": 1 }, "m": 2 })),
        "a nested nobox global must reflect logically (`_lct` tier). Dump: {pkg:#}"
    );
    assert_eq!(
        pkg.get("ONUL"),
        Some(&serde_json::json!({ "__ty": "Opt", "inner": { "__ty": "Leaf", "v": 5 }, "m": 6 })),
        "a GC-tier nobox global must reflect logically (`_vt` tier). Dump: {pkg:#}"
    );
    assert_eq!(
        (pkg.get("RT_NEST"), pkg.get("RT_ONUL")),
        (Some(&serde_json::json!(2)), Some(&serde_json::json!(6))),
        "both tiers must survive snapshot take/restore through the accessors (88 \
         means the restore did not take). Dump: {pkg:#}"
    );
}

/// Emit a project and return its compiled `main.lua`.
fn emit_project_lua(pkg: &str, source: &str) -> String {
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
            "{source}\nfn main() {{\n    \
             stone_run_game(GameConfig {{ title: \"j probe\", width: 64, height: 64 }})\n}}\n"
        ),
    )
    .expect("write main.rock");
    let out = tempfile::tempdir().expect("out dir");
    let status = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args([
            "frame",
            &dir.path().to_string_lossy(),
            "-n",
            "1",
            "--output",
            &out.path().to_string_lossy(),
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "60")
        .current_dir(workspace_root())
        .output()
        .expect("spawn rock frame");
    assert!(
        status.status.success(),
        "rock frame exited {:?}\nstderr:\n{}",
        status.status,
        String::from_utf8_lossy(&status.stderr)
    );
    std::fs::read_to_string(out.path().join("main.lua")).expect("read emitted main.lua")
}

/// A build with NO `nobox` globals must still RESET `__ROCK_NOBOX_GLOBALS`.
///
/// A full hot reload re-executes the chunk in the LIVE process
/// (`stone/engine/hot_reload.rock`, `run_loaded_chunk`), so anything the new
/// chunk does not assign survives from the old one. Emitting nothing when the
/// table would be empty leaves the PREVIOUS build's accessors installed: change
/// the last `nobox` global to an ordinary one and reflection keeps selecting a
/// stale accessor, reading and writing leaf globals that no longer back it.
///
/// Same class as the emitted preservation idioms in `docs/backends.md` -- what a
/// reloaded chunk omits is not absent, it is inherited.
#[test]
#[ignore = "spawns `rock frame` against a generated project (-P full)"]
fn a_build_with_no_nobox_globals_still_resets_the_accessor_table() {
    let lua = emit_project_lua(
        "nobox_reset_probe",
        "value Probe { x: int, y: int }\n\nglobal SPOT: Probe = Probe { x: 3, y: 4 }\n",
    );
    assert!(
        lua.contains("_G.__ROCK_NOBOX_GLOBALS = {"),
        "a build with no nobox globals must still assign __ROCK_NOBOX_GLOBALS, or a \
         reload inherits the previous build's accessors"
    );
}
