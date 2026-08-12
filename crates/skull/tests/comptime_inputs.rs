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
