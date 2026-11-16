//! `task_null_abandon`: a plain `= null` over a `Task?` place drops the
//! coroutine mid-stack, so its terminal `defer`s never run. `stop` / `replace`
//! are the cancel verbs that fire them. Reads inference (the TARGET's type is
//! what identifies the slot). `Task` is declared locally -- the lint keys on
//! the target type, not on the host library.

use crate::support::lint_only;
use bone_lints::Finding;
use rock_core::errors::CompilerErrorLevel;

const PRELUDE: &str = r#"
struct Arena<T> { _len: int }
value Entity<T> {
    _arena: Arena<T>,
    _index: int,
    _generation: int,
}
fn Entity<T>.get(entity) -> T? { return null }

struct Task<R = ()> { _c: int }

struct Mob {
    co: Task? = null,
    other: Task? = null,
    name: string? = null,
    hp: int = 0,
}

fn make_task() -> Task {
    return Task { _c: 1 }
}

fn stop<T>(o: T, f: FieldOf<T, Task?>) {
    field_set(o, f, null)
}
"#;

fn findings(body: &str) -> Vec<Finding> {
    let src = format!("{PRELUDE}\n{body}\n");
    lint_only(&[("<task_null_abandon>", &src)], "task_null_abandon")
}

fn names_place(finding: &Finding, place: &str) -> bool {
    finding
        .error
        .message
        .starts_with(&format!("`{place} = null` abandons"))
}

fn count(body: &str) -> usize {
    findings(body).len()
}

#[test]
fn prelude_typechecks_and_its_field_defaults_are_clean() {
    // `co: Task? = null` in PRELUDE's struct decl is a default, lowered as a
    // StructDefaults body -- it must not read as an assignment either.
    assert_eq!(count("fn main() -> int { 3 }"), 0);
}

#[test]
fn diagnostic_names_the_abandoned_slot() {
    let cases: &[(&str, &str)] = &[
        ("fn bad(m: Mob) {\n    m.co = null\n}", "m.co"),
        (
            "fn bad(e: Entity<Mob>) {\n    e.get()?.co = null\n}",
            "e.get()?.co",
        ),
        (
            "global SLOT: Task? = null\nfn bad() {\n    SLOT = null\n}",
            "SLOT",
        ),
    ];
    for (body, place) in cases {
        let fs = findings(body);
        assert_eq!(fs.len(), 1, "expected the one finding: {fs:?}");
        assert!(names_place(&fs[0], place), "{fs:?}");
    }
}

#[test]
fn null_assignments_to_task_slots_are_counted() {
    let cases: &[(&str, &str, usize)] = &[
        // The whole point of the lint: the two verbs that DO run the defers.
        (
            "stop_call_is_clean",
            "fn good(m: Mob) {\n    stop(m, .co)\n}",
            0,
        ),
        // An initializer is not an assignment: there is no live coroutine in the slot to
        // abandon, so `co: null` in a literal and the field default are both clean.
        (
            "struct_literal_initializer_is_clean",
            "fn good() -> Mob {\n    return Mob { co: null, name: null }\n}",
            0,
        ),
        // only a `Task?` slot has defers to skip
        (
            "non_task_nullable_field_is_clean",
            "fn fine(m: Mob) {\n    m.name = null\n}",
            0,
        ),
        // v1 covers the `= null` shape only. Overwriting a live slot with a NEW task
        // is the same hazard, but `replace` is the fix and telling a first store from
        // an overwrite needs liveness -- see the doc comment on the lint.
        (
            "non_null_assign_is_clean",
            "fn fine(m: Mob) {\n    m.co = make_task()\n}",
            0,
        ),
        // a local is a place too -- the task is just as abandoned
        (
            "local_task_slot_is_flagged",
            "fn bad() {\n    let t: Task? = make_task()\n    t = null\n}",
            1,
        ),
        // two Task? slots nulled, the int is not one
        (
            "each_slot_is_judged_separately",
            "fn bad(m: Mob) {\n    m.co = null\n    m.other = null\n    m.hp = 0\n}",
            2,
        ),
    ];
    for (name, body, expected) in cases {
        assert_eq!(count(body), *expected, "{name}");
    }
}

// `Warning`, not `Error`: two legal null-assign patterns exist (nulling a task
// just observed terminal, and cleanup inside the task's own terminal defer), and
// liveness-at-assignment is dataflow v1 does not attempt -- so those sites need
// a legal escape hatch, unlike `entity_across_yield`.
#[test]
fn finding_reports_at_warning_level() {
    let fs = findings("fn bad(m: Mob) {\n    m.co = null\n}");
    assert_eq!(fs.len(), 1, "expected the one finding: {fs:?}");
    assert_eq!(fs[0].level(), CompilerErrorLevel::Warning);
}

#[test]
fn allow_comments_suppress() {
    let above = "fn bad(m: Mob) {\n    // rock: allow(task_null_abandon)\n    m.co = null\n}";
    assert_eq!(
        count(&above.replace("// rock: allow(task_null_abandon)", "")),
        1,
        "the finding exists without the comment"
    );
    assert_eq!(count(above), 0, "allow comment above silences it");
    let trailing = "fn bad(m: Mob) {\n    m.co = null // rock: allow(task_null_abandon)\n}";
    assert_eq!(count(trailing), 0, "trailing allow comment silences it");
}
