//! Expansion contract for the tier-1 comptime-input intrinsics
//!: `build_var("NAME")` inlines the value the host
//! resolved from `[build.vars]`, `embed_string("path")` inlines a tracked
//! file's contents. Both take only string literals, resolve against
//! `Workspace.comptime_inputs`, and reject with a span on every failure --
//! undeclared var, missing file, path escape -- never a silent default.
use bone_semantic::inference::{ComptimeInputs, ComptimeVarValue};

use crate::common::{function_body, try_emit_module_with};

fn inputs_with_var(name: &str, value: &str) -> ComptimeInputs {
    let mut inputs = ComptimeInputs::default();
    inputs.vars.insert(
        ustr::Ustr::from(name),
        ComptimeVarValue {
            value: value.to_string(),
            secret: false,
        },
    );
    inputs
}

#[test]
fn build_var_inlines_the_resolved_value() {
    let src = r#"
fn channel_tag() -> string {
    return build_var("CHANNEL")
}
"#;
    let inputs = inputs_with_var("CHANNEL", "steam-test-value");
    let lua = try_emit_module_with(src, inputs).expect("declared var expands");
    let body = function_body(&lua, "channel_tag");
    assert!(body.contains("steam-test-value"), "value inlined:\n{body}");
    assert!(!body.contains("build_var"), "call fully replaced:\n{body}");
}

#[test]
fn build_var_undeclared_rejects_with_the_name() {
    let src = r#"
fn broken() -> string {
    return build_var("NOPE")
}
"#;
    let err = try_emit_module_with(src, ComptimeInputs::default())
        .expect_err("undeclared build var must reject");
    let msg = err.message.clone();
    assert!(
        msg.contains("NOPE") && msg.contains("build.vars"),
        "error names var and section: {msg}"
    );
}

#[test]
fn build_var_non_literal_arg_rejects() {
    let src = r#"
fn broken(name: string) -> string {
    return build_var(name)
}
"#;
    let err = try_emit_module_with(src, inputs_with_var("CHANNEL", "x"))
        .expect_err("non-literal build_var arg must reject");
    assert!(
        err.message.contains("literal"),
        "error names the literal requirement: {}",
        err.message
    );
}

#[test]
fn build_var_misuse_in_uninstantiated_generic_rejects() {
    // A generic fn with no call sites never lowers, so the expander's own
    // error cannot fire -- only the fatal gate sees this body.
    let src = r#"
fn dead_generic<T>(x: T) -> string {
    return build_var("NOPE")
}
"#;
    let err = try_emit_module_with(src, ComptimeInputs::default())
        .expect_err("misuse in an uninstantiated generic must still reject");
    assert!(
        err.message.contains("NOPE"),
        "error names the var: {}",
        err.message
    );
}

#[test]
fn embed_string_inlines_file_contents() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("greeting.txt"), "hello-from-embed\n").unwrap();
    let src = r#"
fn greeting() -> string {
    return embed_string("greeting.txt")
}
"#;
    let inputs = ComptimeInputs {
        embed_root: Some(dir.path().to_path_buf()),
        ..ComptimeInputs::default()
    };
    let lua = try_emit_module_with(src, inputs).expect("embed expands");
    let body = function_body(&lua, "greeting");
    assert!(
        body.contains("hello-from-embed"),
        "file content inlined:\n{body}"
    );
}

#[test]
fn embed_string_missing_file_rejects_with_path() {
    let dir = tempfile::tempdir().unwrap();
    let src = r#"
fn broken() -> string {
    return embed_string("absent.txt")
}
"#;
    let inputs = ComptimeInputs {
        embed_root: Some(dir.path().to_path_buf()),
        ..ComptimeInputs::default()
    };
    let err = try_emit_module_with(src, inputs).expect_err("missing embed file must reject");
    assert!(
        err.message.contains("absent.txt"),
        "error names the path: {}",
        err.message
    );
}

#[test]
fn embed_string_escaping_root_rejects() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("proj")).unwrap();
    std::fs::write(dir.path().join("secret.txt"), "outside\n").unwrap();
    let src = r#"
fn broken() -> string {
    return embed_string("../secret.txt")
}
"#;
    let inputs = ComptimeInputs {
        embed_root: Some(dir.path().join("proj")),
        ..ComptimeInputs::default()
    };
    let err = try_emit_module_with(src, inputs).expect_err("path escaping the root must reject");
    assert!(
        err.message.contains("project root") || err.message.contains("escapes"),
        "error names the escape: {}",
        err.message
    );
}

#[test]
fn embed_resolves_against_the_calling_files_own_package() {
    // A LIBRARY file (host) embeds relative to its own stick.toml dir, not
    // the consuming project's embed_root -- the cimgui migration's
    // prerequisite. The calling
    // file's dir has a stick.toml; embed_root points somewhere else entirely.
    let pkg = tempfile::tempdir().unwrap();
    std::fs::write(
        pkg.path().join("stick.toml"),
        "[package]\nname = \"libpkg\"\ntype = \"lib\"\n",
    )
    .unwrap();
    std::fs::write(pkg.path().join("payload.txt"), "from-the-library-package").unwrap();
    let lib_file = pkg.path().join("lib.rock");
    std::fs::write(&lib_file, "").unwrap();

    let elsewhere = tempfile::tempdir().unwrap();
    let src = r#"
fn payload() -> string {
    return embed_string("payload.txt")
}
"#;
    let inputs = ComptimeInputs {
        embed_root: Some(elsewhere.path().to_path_buf()),
        ..ComptimeInputs::default()
    };
    let lua = crate::common::try_emit_module_with_file(src, lib_file.to_str().unwrap(), inputs)
        .expect("package-relative embed resolves");
    assert!(
        lua.contains("from-the-library-package"),
        "embed must resolve against the calling file's package root"
    );
}

fn memory_inputs(files: &[(&str, &str)]) -> ComptimeInputs {
    let files = files
        .iter()
        .map(|(path, text)| (path.to_string(), text.to_string()));
    ComptimeInputs {
        embed_files: bone_core::comptime_files::EmbedFiles(std::sync::Arc::new(
            bone_core::comptime_files::MemoryComptimeFiles::new(files),
        )),
        ..ComptimeInputs::default()
    }
}

/// A host without a filesystem supplies the embedded files itself: the read,
/// and the fingerprint's re-read of it, see the supplied text and never the
/// disk (no file of that name exists on disk here).
#[test]
fn embed_string_reads_host_supplied_files() {
    let src = r#"
fn greeting() -> string {
    return embed_string("data/greeting.txt")
}
"#;
    let inputs = memory_inputs(&[("data/greeting.txt", "hello-from-memory")]);
    let lua = try_emit_module_with(src, inputs).expect("supplied embed expands");
    let body = function_body(&lua, "greeting");
    assert!(
        body.contains("hello-from-memory"),
        "supplied content inlined:\n{body}"
    );
}

#[test]
fn embed_string_of_an_unsupplied_file_names_the_host() {
    let src = r#"
fn broken() -> string {
    return embed_string("absent.txt")
}
"#;
    let err = try_emit_module_with(src, memory_inputs(&[]))
        .expect_err("an embed the host did not supply must reject");
    assert!(
        err.message.contains("absent.txt") && err.message.contains("host supplied no file"),
        "error names the file and the host: {}",
        err.message
    );
}

// --- incremental invalidation: a comptime input change is a body edit ---
// bone hashes only source text, so a body whose emitted code depends on a
// build var's value or an embedded file's content never appears in
// `reinferred`; the cache must detect stale inputs itself, and the change must
// reach emit_incremental AND emit_overlay (byte-identity gates compare fresh
// full builds and cannot see this class of staleness).

fn ws_with_inputs(
    src: &str,
    inputs: bone_semantic::inference::ComptimeInputs,
) -> bone_semantic::inference::Workspace {
    let mut ws = crate::common::ws_with_stdlib(src);
    ws.comptime_inputs = inputs;
    ws
}

fn reinferred_of(
    ws: &bone_semantic::inference::Workspace,
    name: &str,
) -> (
    bone_semantic::prelude::ModuleId,
    bone_semantic::hir::BodyOwner,
) {
    for module in ws.modules.iter() {
        for (fid, f) in module.functions.iter() {
            if f.name.as_str() == name {
                return (module.id, bone_semantic::hir::BodyOwner::Function(fid));
            }
        }
    }
    panic!("no fn named {name}");
}

fn global_reinferred_of(
    ws: &bone_semantic::inference::Workspace,
    name: &str,
) -> (
    bone_semantic::prelude::ModuleId,
    bone_semantic::hir::BodyOwner,
) {
    for module in ws.modules.iter() {
        for (gid, global) in module.globals.iter() {
            if global.name.as_str() == name {
                return (module.id, bone_semantic::hir::BodyOwner::Global(gid));
            }
        }
    }
    panic!("no global named {name}");
}

const INCR_SRC: &str = r#"
fn tag() -> string {
    return build_var("SHA")
}
fn other() -> int {
    return 1
}
"#;

#[test]
fn build_var_change_reaches_incremental_and_overlay() {
    use skull::mir_emit::{emit_full, emit_incremental, emit_overlay};

    let ws1 = ws_with_inputs(INCR_SRC, inputs_with_var("SHA", "first-sha-value"));
    let mid = skull::user_module_id(&ws1).unwrap();
    let (full1, mut cache) = emit_full(&ws1, mid).expect("emit_full v1");
    assert!(
        full1.lua.contains("first-sha-value"),
        "control: full build inlines v1"
    );

    // Same source; only the resolved value moved. The edit the session saw is
    // an unrelated body, so only stale-input detection can re-emit `tag`.
    let ws2 = ws_with_inputs(INCR_SRC, inputs_with_var("SHA", "second-sha-value"));
    let edit = vec![reinferred_of(&ws2, "other")];
    let incremental = emit_incremental(&ws2, &mut cache, &edit).expect("emit_incremental");
    assert!(
        incremental.lua.contains("second-sha-value")
            && !incremental.lua.contains("first-sha-value"),
        "incremental assembly must carry the new value"
    );
    let fresh = emit_full(&ws2, skull::user_module_id(&ws2).unwrap())
        .expect("fresh")
        .0;
    assert_eq!(
        incremental.lua, fresh.lua,
        "incremental != fresh after a var change"
    );

    // Overlay from a fresh cache: full at v1, overlay after the value moved.
    let ws1b = ws_with_inputs(INCR_SRC, inputs_with_var("SHA", "first-sha-value"));
    let (_, mut cache2) = emit_full(&ws1b, mid).expect("emit_full v1b");
    let ws3 = ws_with_inputs(INCR_SRC, inputs_with_var("SHA", "third-sha-value"));
    let overlay =
        emit_overlay(&ws3, &mut cache2, &[reinferred_of(&ws3, "other")]).expect("emit_overlay");
    assert!(
        overlay.lua.contains("third-sha-value"),
        "the overlay must redefine the body reading the changed var:\n{}",
        overlay.lua
    );
}

#[test]
fn a_comptime_input_change_reaches_every_recorded_consumer() {
    use skull::mir_emit::{emit_full, emit_incremental};

    let src = r#"
fn first() -> string {
    return build_var("SHA")
}
fn second() -> string {
    return build_var("SHA")
}
const TAG: string = build_var("SHA")
fn other() -> int {
    return 1
}
fn main() {
    print(first())
    print(second())
    print(TAG)
}
"#;
    let ws1 = ws_with_inputs(src, inputs_with_var("SHA", "all-first"));
    let mid = skull::user_module_id(&ws1).unwrap();
    let (full1, mut cache) = emit_full(&ws1, mid).expect("emit_full v1");
    assert_eq!(full1.lua.matches("all-first").count(), 3);

    let ws2 = ws_with_inputs(src, inputs_with_var("SHA", "all-second"));
    let incremental = emit_incremental(&ws2, &mut cache, &[reinferred_of(&ws2, "other")])
        .expect("emit_incremental");
    assert_eq!(incremental.lua.matches("all-second").count(), 3);
    assert!(!incremental.lua.contains("all-first"));
    let fresh = emit_full(&ws2, skull::user_module_id(&ws2).unwrap())
        .expect("fresh")
        .0;
    assert_eq!(
        incremental.lua, fresh.lua,
        "incremental != fresh for all consumers"
    );
}

#[test]
fn embed_file_change_reaches_incremental() {
    use skull::mir_emit::{emit_full, emit_incremental};

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("data.txt"), "embed-v1").unwrap();
    let src = r#"
fn data() -> string {
    return embed_string("data.txt")
}
fn other() -> int {
    return 1
}
"#;
    let inputs = ComptimeInputs {
        embed_root: Some(dir.path().to_path_buf()),
        ..ComptimeInputs::default()
    };

    let ws1 = ws_with_inputs(src, inputs.clone());
    let mid = skull::user_module_id(&ws1).unwrap();
    let (full1, mut cache) = emit_full(&ws1, mid).expect("emit_full v1");
    assert!(
        full1.lua.contains("embed-v1"),
        "control: full build embeds v1"
    );

    std::fs::write(dir.path().join("data.txt"), "embed-v2").unwrap();
    let ws2 = ws_with_inputs(src, inputs);
    let edit = vec![reinferred_of(&ws2, "other")];
    let incremental = emit_incremental(&ws2, &mut cache, &edit).expect("emit_incremental");
    assert!(
        incremental.lua.contains("embed-v2") && !incremental.lua.contains("embed-v1"),
        "incremental assembly must carry the edited file's bytes"
    );
    let fresh = emit_full(&ws2, skull::user_module_id(&ws2).unwrap())
        .expect("fresh")
        .0;
    assert_eq!(
        incremental.lua, fresh.lua,
        "incremental != fresh after a file edit"
    );
}

#[test]
fn embed_file_change_reaches_the_overlay() {
    use skull::mir_emit::{emit_full, emit_overlay};

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("data.txt"), "fn-embed-v1").unwrap();
    let src = r#"
fn data() -> string {
    return embed_string("data.txt")
}
fn other() -> int {
    return 1
}
"#;
    let inputs = ComptimeInputs {
        embed_root: Some(dir.path().to_path_buf()),
        ..ComptimeInputs::default()
    };
    let ws1 = ws_with_inputs(src, inputs.clone());
    let mid = skull::user_module_id(&ws1).unwrap();
    let (_, mut cache) = emit_full(&ws1, mid).expect("emit_full v1");

    std::fs::write(dir.path().join("data.txt"), "fn-embed-v2").unwrap();
    let ws2 = ws_with_inputs(src, inputs);
    let overlay =
        emit_overlay(&ws2, &mut cache, &[reinferred_of(&ws2, "other")]).expect("emit_overlay");
    assert!(
        overlay.lua.contains("fn-embed-v2"),
        "the overlay must redefine the body embedding the edited file:\n{}",
        overlay.lua
    );
}

// --- global initializers ---
// ALL in-tree production comptime reads happen in global initializers (application's
// telemetry/build_info stamps, the profiler SHA, the cimgui prelude), which
// emit through `UnitOut`, not a function Chunk. A read recorded only on
// chunks leaves this whole shape invisible to the watch set and to overlay
// staleness while every function-body fixture stays green.

#[test]
fn global_embed_read_joins_the_watch_file_set() {
    use skull::mir_emit::emit_full;

    let dir = tempfile::tempdir().unwrap();
    let payload = dir.path().join("payload.txt");
    std::fs::write(&payload, "global-embed-payload").unwrap();
    let src = r#"
const BANNER: string = embed_string("payload.txt")
fn main() {
    print(BANNER)
}
"#;
    let inputs = ComptimeInputs {
        embed_root: Some(dir.path().to_path_buf()),
        ..ComptimeInputs::default()
    };
    let ws = ws_with_inputs(src, inputs);
    let mid = skull::user_module_id(&ws).unwrap();
    let (full, cache) = emit_full(&ws, mid).expect("emit_full");
    assert!(
        full.lua.contains("global-embed-payload"),
        "control: the embed landed"
    );

    let canonical = payload.canonicalize().unwrap();
    let files: Vec<_> = cache
        .comptime_file_hashes()
        .into_iter()
        .map(|(path, _)| path)
        .collect();
    assert!(
        files
            .iter()
            .any(|f| std::path::Path::new(f.as_str()) == canonical),
        "a global initializer's embed must join the watch set, got {files:?}"
    );
}

const GLOBAL_VAR_SRC: &str = r#"
const TAG: string = build_var("SHA")
fn other() -> int {
    return 1
}
fn main() {
    print(TAG)
}
"#;

#[test]
fn global_const_build_var_change_reaches_incremental() {
    use skull::mir_emit::{emit_full, emit_incremental};

    let ws1 = ws_with_inputs(GLOBAL_VAR_SRC, inputs_with_var("SHA", "first-sha-value"));
    let mid = skull::user_module_id(&ws1).unwrap();
    let (full1, mut cache) = emit_full(&ws1, mid).expect("emit_full v1");
    assert!(
        full1.lua.contains("first-sha-value"),
        "control: full build inlines v1"
    );

    let ws2 = ws_with_inputs(GLOBAL_VAR_SRC, inputs_with_var("SHA", "second-sha-value"));
    let edit = vec![reinferred_of(&ws2, "other")];
    let incremental = emit_incremental(&ws2, &mut cache, &edit).expect("emit_incremental");
    assert!(
        incremental.lua.contains("second-sha-value")
            && !incremental.lua.contains("first-sha-value"),
        "incremental reassembly re-lowers globals, so the new value must land"
    );
}

#[test]
fn global_const_build_var_change_reaches_the_overlay() {
    use skull::mir_emit::{emit_full, emit_overlay};

    let ws1 = ws_with_inputs(GLOBAL_VAR_SRC, inputs_with_var("SHA", "first-sha-value"));
    let mid = skull::user_module_id(&ws1).unwrap();
    let (_, mut cache) = emit_full(&ws1, mid).expect("emit_full v1");

    // The session saw only an unrelated body edit; the var moved underneath.
    let ws2 = ws_with_inputs(GLOBAL_VAR_SRC, inputs_with_var("SHA", "second-sha-value"));
    let overlay =
        emit_overlay(&ws2, &mut cache, &[reinferred_of(&ws2, "other")]).expect("emit_overlay");
    assert!(
        overlay.lua.contains("second-sha-value"),
        "the overlay must re-emit a const init whose build var moved:\n{}",
        overlay.lua
    );
    assert!(
        overlay.lua.contains("TAG = "),
        "the overlay must re-run the const's init line:\n{}",
        overlay.lua
    );
}

#[test]
fn global_body_edit_replaces_the_recorded_comptime_read_set() {
    use skull::mir_emit::{emit_full, emit_overlay};

    let dir = tempfile::tempdir().unwrap();
    let first_path = dir.path().join("first.txt");
    let second_path = dir.path().join("second.txt");
    std::fs::write(&first_path, "first-v1").unwrap();
    std::fs::write(&second_path, "second-v1").unwrap();
    let v1 = r#"
const TAG: string = embed_string("first.txt")
fn other() -> int {
    return 1
}
fn main() {
    print(TAG)
}
"#;
    let v2 = v1.replace("first.txt", "second.txt");
    let inputs = bone_semantic::inference::ComptimeInputs {
        embed_root: Some(dir.path().to_path_buf()),
        ..bone_semantic::inference::ComptimeInputs::default()
    };
    let ws1 = ws_with_inputs(v1, inputs.clone());
    let mid = skull::user_module_id(&ws1).unwrap();
    let (_, mut cache) = emit_full(&ws1, mid).expect("emit_full v1");

    let ws2 = ws_with_inputs(&v2, inputs.clone());
    let first = emit_overlay(&ws2, &mut cache, &[global_reinferred_of(&ws2, "TAG")])
        .expect("global body edit overlay");
    assert!(
        first.lua.contains("second-v1"),
        "the first overlay must carry the new input: {}",
        first.lua
    );
    let first_canonical = first_path.canonicalize().unwrap();
    let second_canonical = second_path.canonicalize().unwrap();
    let files: Vec<_> = cache
        .comptime_file_hashes()
        .into_iter()
        .map(|(path, _)| path)
        .collect();
    assert!(
        files
            .iter()
            .any(|file| std::path::Path::new(file.as_str()) == second_canonical)
            && !files
                .iter()
                .any(|file| std::path::Path::new(file.as_str()) == first_canonical),
        "the overlay must publish the replacement global read set: {files:?}"
    );

    std::fs::write(&second_path, "second-v2").unwrap();
    let ws3 = ws_with_inputs(&v2, inputs);
    let second =
        emit_overlay(&ws3, &mut cache, &[reinferred_of(&ws3, "other")]).expect("new input overlay");
    assert!(
        second.lua.contains("second-v2"),
        "a later change to the newly recorded input must reach the global initializer: {}",
        second.lua
    );
}

#[test]
fn global_const_embed_change_reaches_the_overlay() {
    use skull::mir_emit::{emit_full, emit_overlay};

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("data.txt"), "overlay-embed-v1").unwrap();
    let src = r#"
const DATA: string = embed_string("data.txt")
fn other() -> int {
    return 1
}
fn main() {
    print(DATA)
}
"#;
    let inputs = ComptimeInputs {
        embed_root: Some(dir.path().to_path_buf()),
        ..ComptimeInputs::default()
    };
    let ws1 = ws_with_inputs(src, inputs.clone());
    let mid = skull::user_module_id(&ws1).unwrap();
    let (_, mut cache) = emit_full(&ws1, mid).expect("emit_full v1");

    std::fs::write(dir.path().join("data.txt"), "overlay-embed-v2").unwrap();
    let ws2 = ws_with_inputs(src, inputs);
    let overlay =
        emit_overlay(&ws2, &mut cache, &[reinferred_of(&ws2, "other")]).expect("emit_overlay");
    assert!(
        overlay.lua.contains("overlay-embed-v2"),
        "the overlay must re-emit a const init whose embedded file changed:\n{}",
        overlay.lua
    );
}

#[test]
fn embed_string_without_project_root_rejects() {
    let src = r#"
fn broken() -> string {
    return embed_string("x.txt")
}
"#;
    let err = try_emit_module_with(src, ComptimeInputs::default())
        .expect_err("embed with no project root must reject");
    assert!(
        err.message.contains("project root"),
        "error names the missing root: {}",
        err.message
    );
}
