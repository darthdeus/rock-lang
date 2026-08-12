//! Expansion contract for the tier-1 comptime-input intrinsics
//! (docs/comptime-plan.md): `build_var("NAME")` inlines the value the host
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
        ComptimeVarValue { value: value.to_string(), secret: false },
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
    assert!(msg.contains("NOPE") && msg.contains("build.vars"), "error names var and section: {msg}");
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
    assert!(err.message.contains("literal"), "error names the literal requirement: {}", err.message);
}

#[test]
fn build_var_misuse_in_uninstantiated_generic_rejects() {
    // A generic fn with no call sites never lowers, so the expander's own
    // error cannot fire -- only the fatal gate sees this body. This is the
    // gate's distinct population (its siblings' doctrine: a per-fn lowering
    // error is only a lenient skip).
    let src = r#"
fn dead_generic<T>(x: T) -> string {
    return build_var("NOPE")
}
"#;
    let err = try_emit_module_with(src, ComptimeInputs::default())
        .expect_err("misuse in an uninstantiated generic must still reject");
    assert!(err.message.contains("NOPE"), "error names the var: {}", err.message);
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
    let mut inputs = ComptimeInputs::default();
    inputs.embed_root = Some(dir.path().to_path_buf());
    let lua = try_emit_module_with(src, inputs).expect("embed expands");
    let body = function_body(&lua, "greeting");
    assert!(body.contains("hello-from-embed"), "file content inlined:\n{body}");
}

#[test]
fn embed_string_missing_file_rejects_with_path() {
    let dir = tempfile::tempdir().unwrap();
    let src = r#"
fn broken() -> string {
    return embed_string("absent.txt")
}
"#;
    let mut inputs = ComptimeInputs::default();
    inputs.embed_root = Some(dir.path().to_path_buf());
    let err = try_emit_module_with(src, inputs).expect_err("missing embed file must reject");
    assert!(err.message.contains("absent.txt"), "error names the path: {}", err.message);
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
    let mut inputs = ComptimeInputs::default();
    inputs.embed_root = Some(dir.path().join("proj"));
    let err = try_emit_module_with(src, inputs).expect_err("path escaping the root must reject");
    assert!(
        err.message.contains("project root") || err.message.contains("escapes"),
        "error names the escape: {}",
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
) -> (bone_semantic::prelude::ModuleId, bone_semantic::hir::BodyOwner) {
    for module in ws.modules.iter() {
        for (fid, f) in module.functions.iter() {
            if f.name.as_str() == name {
                return (module.id, bone_semantic::hir::BodyOwner::Function(fid));
            }
        }
    }
    panic!("no fn named {name}");
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
    assert!(full1.lua.contains("first-sha-value"), "control: full build inlines v1");

    // Same source; only the resolved value moved. The edit the session saw is
    // an unrelated body, so only stale-input detection can re-emit `tag`.
    let ws2 = ws_with_inputs(INCR_SRC, inputs_with_var("SHA", "second-sha-value"));
    let edit = vec![reinferred_of(&ws2, "other")];
    let incremental = emit_incremental(&ws2, &mut cache, &edit).expect("emit_incremental");
    assert!(
        incremental.lua.contains("second-sha-value") && !incremental.lua.contains("first-sha-value"),
        "incremental assembly must carry the new value"
    );
    let fresh = emit_full(&ws2, skull::user_module_id(&ws2).unwrap()).expect("fresh").0;
    assert_eq!(incremental.lua, fresh.lua, "incremental != fresh after a var change");

    // Overlay from a fresh cache: full at v1, overlay after the value moved.
    let ws1b = ws_with_inputs(INCR_SRC, inputs_with_var("SHA", "first-sha-value"));
    let (_, mut cache2) = emit_full(&ws1b, mid).expect("emit_full v1b");
    let ws3 = ws_with_inputs(INCR_SRC, inputs_with_var("SHA", "third-sha-value"));
    let overlay = emit_overlay(&ws3, &mut cache2, &[reinferred_of(&ws3, "other")]);
    assert!(
        overlay.lua.contains("third-sha-value"),
        "the overlay must redefine the body reading the changed var:\n{}",
        overlay.lua
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
    let mut inputs = ComptimeInputs::default();
    inputs.embed_root = Some(dir.path().to_path_buf());

    let ws1 = ws_with_inputs(src, inputs.clone());
    let mid = skull::user_module_id(&ws1).unwrap();
    let (full1, mut cache) = emit_full(&ws1, mid).expect("emit_full v1");
    assert!(full1.lua.contains("embed-v1"), "control: full build embeds v1");

    std::fs::write(dir.path().join("data.txt"), "embed-v2").unwrap();
    let ws2 = ws_with_inputs(src, inputs);
    let edit = vec![reinferred_of(&ws2, "other")];
    let incremental = emit_incremental(&ws2, &mut cache, &edit).expect("emit_incremental");
    assert!(
        incremental.lua.contains("embed-v2") && !incremental.lua.contains("embed-v1"),
        "incremental assembly must carry the edited file's bytes"
    );
    let fresh = emit_full(&ws2, skull::user_module_id(&ws2).unwrap()).expect("fresh").0;
    assert_eq!(incremental.lua, fresh.lua, "incremental != fresh after a file edit");
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
