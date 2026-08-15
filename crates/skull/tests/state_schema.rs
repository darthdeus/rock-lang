//! RED checkpoint for the compiler-owned logical state inventory.

use crate::common::emit_mir;

const FIXTURE: &str = r#"
value Boxed { x: int, y: int }
nobox value Flat { x: int, y: int }

global BOXED: Boxed = Boxed { x: 1, y: 2 }
global FLAT: Flat = Flat { x: 3, y: 4 }
global RESULT: int = 0

fn with_static() -> int {
    static initialized = 7
    static lazy = 2
    return initialized + lazy
}

@inline fn generic_static<T>(value: T) -> T {
    static calls = 0
    calls = calls + 1
    return value
}

co fn frame_source() -> Coroutine<int> {
    let live = 11
    yield live
    yield live + 1
}

fn main() {
    let _ = generic_static(5)
    let _frame = frame_source()
    RESULT = with_static() + BOXED.x + FLAT.x
}
"#;

#[test]
fn compiler_owned_state_schema_is_total_for_a_nondegenerate_fixture() {
    let emit = emit_mir(FIXTURE);

    let census = emit.state_schema.census();
    assert!(
        census.globals > 0,
        "global population must be nonzero: {census:?}"
    );
    assert!(
        census.statics > 0,
        "static population must be nonzero: {census:?}"
    );
    assert!(
        census.frames > 0,
        "coroutine-frame population must be nonzero: {census:?}"
    );
    assert!(
        census.storage > 0,
        "storage population must be nonzero: {census:?}"
    );
    assert_eq!(emit.state_schema, emit.state_schema);

    let identities: Vec<_> = emit
        .state_schema
        .entries
        .iter()
        .map(|entry| &entry.identity)
        .collect();
    assert!(
        identities.iter().any(|id| id.contains("generic_static")),
        "generic static identity is absent"
    );
    assert!(
        emit.lua.contains("_G.__ROCK_STATICS"),
        "the shadow schema must compare against the old static registry"
    );
    for entry in emit
        .state_schema
        .entries
        .iter()
        .filter(|entry| entry.kind == skull_mir::state_schema::StateKind::Static)
    {
        assert!(
            emit.lua.contains(&format!(
                "[{:?}] = {:?}",
                entry.physical[0], entry.physical[1]
            )),
            "old static registry omitted {}",
            entry.identity
        );
    }
}

#[test]
fn state_schema_identity_ignores_body_and_source_order_but_tracks_membership_and_layout() {
    let baseline = emit_mir(FIXTURE).state_schema;
    let body_edit = FIXTURE.replace("live + 1", "live + 2");
    assert_eq!(
        baseline.schema_id,
        emit_mir(&body_edit).state_schema.schema_id
    );

    let reordered = FIXTURE.replace(
        "global BOXED: Boxed = Boxed { x: 1, y: 2 }\nglobal FLAT: Flat = Flat { x: 3, y: 4 }",
        "global FLAT: Flat = Flat { x: 3, y: 4 }\nglobal BOXED: Boxed = Boxed { x: 1, y: 2 }",
    );
    assert_eq!(
        baseline.schema_id,
        emit_mir(&reordered).state_schema.schema_id
    );

    let added = FIXTURE.replace(
        "global RESULT: int = 0",
        "global EXTRA: int = 9\nglobal RESULT: int = 0",
    );
    assert_ne!(baseline.schema_id, emit_mir(&added).state_schema.schema_id);

    let reshaped = FIXTURE
        .replace(
            "value Boxed { x: int, y: int }",
            "value Boxed { x: int, z: float }",
        )
        .replace("Boxed { x: 1, y: 2 }", "Boxed { x: 1, z: 2.0 }");
    assert_ne!(
        baseline.schema_id,
        emit_mir(&reshaped).state_schema.schema_id
    );
}

#[test]
fn state_schema_rejects_omissions_duplicates_and_incomplete_entries() {
    use skull_mir::state_schema::{StateEntry, StateKind, StateSchema, StateSchemaError};

    let entry = |identity: &str, fingerprint: &str, physical: &[&str]| StateEntry {
        kind: StateKind::Global,
        identity: identity.to_string(),
        fingerprint: fingerprint.to_string(),
        physical: physical.iter().map(|name| (*name).to_string()).collect(),
        owner: None,
        initializer_identity: None,
    };
    assert!(matches!(
        StateSchema::from_entries(vec![entry("", "shape", &["g"])]),
        Err(StateSchemaError::Incomplete { .. })
    ));
    assert!(matches!(
        StateSchema::from_entries(vec![entry("g", "shape", &[])]),
        Err(StateSchemaError::MissingPhysical { .. })
    ));
    assert!(matches!(
        StateSchema::from_entries(vec![
            entry("g", "shape", &["x"]),
            entry("g", "shape", &["y"])
        ]),
        Err(StateSchemaError::DuplicateLayout { .. })
    ));
    assert!(matches!(
        StateSchema::from_entries(vec![
            entry("g", "shape", &["x"]),
            entry("h", "shape", &["x"])
        ]),
        Err(StateSchemaError::UnownedPhysical { .. })
    ));
}
