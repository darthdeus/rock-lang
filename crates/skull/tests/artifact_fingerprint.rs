//! RED contract fixtures for the compiler-owned exact Artifact fingerprint.

mod common;

use bone_semantic::inference::{ComptimeInputs, ComptimeVarValue};
use common::{emit_mir_with_inputs, try_emit_mir_with_inputs};
use ustr::Ustr;

fn inputs(values: &[(&str, &str)], volatile: &[&str]) -> ComptimeInputs {
    let mut out = ComptimeInputs::default();
    for (name, value) in values {
        out.vars.insert(
            Ustr::from(*name),
            ComptimeVarValue { value: (*value).to_string(), secret: false },
        );
    }
    out.volatile_vars.extend(volatile.iter().copied().map(Ustr::from));
    out
}

fn fingerprint(src: &str, inputs: ComptimeInputs) -> String {
    emit_mir_with_inputs(src, inputs).artifact_fingerprint
}

const VOLATILE_SOURCE: &str = r#"
global BUILD_DATE: string = build_var("BUILD_DATE")
global BUILD_UNIX: string = build_var("BUILD_UNIX")
global BUILD_LABEL: string = "2026-08-23 10:00"
fn main() {}
"#;

const BODY_SOURCE: &str = r#"
fn helper() -> int { return 1 }
fn main() { let _ = helper() }
"#;

#[test]
fn declared_volatile_metadata_does_not_change_the_artifact_fingerprint() {
    let old = fingerprint(
        VOLATILE_SOURCE,
        inputs(&[("BUILD_DATE", "2026-08-23 10:00"), ("BUILD_UNIX", "1")], &["BUILD_DATE", "BUILD_UNIX"]),
    );
    let new = fingerprint(
        VOLATILE_SOURCE,
        inputs(&[("BUILD_DATE", "2026-08-23 10:01"), ("BUILD_UNIX", "2")], &["BUILD_DATE", "BUILD_UNIX"]),
    );
    assert_eq!(old, new, "only declared volatile presentation metadata changed");
}

#[test]
fn a_one_sided_body_change_changes_the_artifact_fingerprint() {
    let changed = BODY_SOURCE.replace("return 1", "return 2");
    assert_ne!(
        fingerprint(BODY_SOURCE, ComptimeInputs::default()),
        fingerprint(&changed, ComptimeInputs::default()),
        "the changed body must be represented by the exact Artifact identity"
    );
}

#[test]
fn lifecycle_definition_and_state_structure_change_the_fingerprint() {
    let lifecycle_a = r#"
@lifecycle(phase="update", owner="session") deco init tick() {}
@tick fn first() {}
fn main() {}
"#;
    let lifecycle_b = lifecycle_a.replace("@tick fn first() {}", "@tick fn first() {}\n@tick fn second() {}");
    assert_ne!(
        fingerprint(lifecycle_a, ComptimeInputs::default()),
        fingerprint(&lifecycle_b, ComptimeInputs::default()),
        "lifecycle membership is an Artifact input"
    );

    let lifecycle_phase_a = r#"
@lifecycle(phase="update", owner="session") deco init tick() {}
@tick fn first() {}
fn main() {}
"#;
    let lifecycle_phase_b = lifecycle_phase_a.replace("phase=\"update\"", "phase=\"late_update\"");
    assert_ne!(
        fingerprint(lifecycle_phase_a, ComptimeInputs::default()),
        fingerprint(&lifecycle_phase_b, ComptimeInputs::default()),
        "lifecycle structure is an Artifact input even when code bodies are unchanged"
    );

    let definition_a = r#"
@artifact_definition(catalog="widgets", key_param="key")
deco init widget(key: string) {}
@widget(key="alpha") fn make_alpha() -> int { return 1 }
fn main() {}
"#;
    let definition_b = definition_a.replace(
        "fn main() {}",
        "@widget(key=\"beta\") fn make_beta() -> int { return 2 }\nfn main() {}",
    );
    assert_ne!(
        fingerprint(definition_a, ComptimeInputs::default()),
        fingerprint(&definition_b, ComptimeInputs::default()),
        "definition membership is an Artifact input"
    );

    let definition_catalog_a = definition_a.replace("catalog=\"widgets\"", "catalog=\"widgets_a\"");
    let definition_catalog_b = definition_a.replace("catalog=\"widgets\"", "catalog=\"widgets_b\"");
    assert_ne!(
        fingerprint(&definition_catalog_a, ComptimeInputs::default()),
        fingerprint(&definition_catalog_b, ComptimeInputs::default()),
        "definition structure is an Artifact input even when code bodies are unchanged"
    );

    let state_a = "global STATE: int = 0\nfn main() {}";
    let state_b = "global STATE: int = 0\nglobal EXTRA: int = 1\nfn main() {}";
    assert_ne!(
        fingerprint(state_a, ComptimeInputs::default()),
        fingerprint(state_b, ComptimeInputs::default()),
        "the global layout is an Artifact input"
    );
}

#[test]
fn embedded_comptime_input_change_changes_the_artifact_fingerprint() {
    let source = "global EMBEDDED: string = build_var(\"TOKEN\")\nfn main() {}";
    assert_ne!(
        fingerprint(source, inputs(&[("TOKEN", "A")], &[])),
        fingerprint(source, inputs(&[("TOKEN", "B")], &[])),
        "a changed embedded comptime value is an Artifact input"
    );
}

#[test]
fn unknown_or_malformed_volatile_metadata_fails_fingerprint_generation() {
    let unknown = match try_emit_mir_with_inputs(
        "fn main() {}",
        inputs(&[], &["NOT_A_DECLARED_INPUT"]),
    ) {
        Ok(_) => panic!("unknown volatile metadata must not disappear"),
        Err(error) => error,
    };
    assert!(unknown.message.contains("volatile"), "diagnostic must identify volatile metadata: {unknown}");

    let empty = match try_emit_mir_with_inputs(
        "fn main() {}",
        inputs(&[], &[""]),
    ) {
        Ok(_) => panic!("malformed volatile metadata must not disappear"),
        Err(error) => error,
    };
    assert!(empty.message.contains("volatile"), "diagnostic must identify volatile metadata: {empty}");
}
