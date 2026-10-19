//! `task_null_abandon`: a plain `= null` over a `Task?` place drops the
//! coroutine mid-stack, so its terminal `defer`s never run. `stop` / `replace`
//! are the cancel verbs that fire them. Reads inference (the TARGET's type is
//! what identifies the slot), so the harness runs a full `semantic_pass`.
//! `Task` is declared locally -- the lint keys on the target type, not on the host library.

use bone_lints::{filter_suppressed, task_null_abandon, LintFinding, TaskNullAbandon};
use bone_semantic::prelude::*;
use rock_core::errors::CompilerErrorLevel;

const FILE: &str = "<task_null_abandon>";

fn parse(src: &str) -> skull_parser::Ast {
    skull_parser::parse_module(src, Ustr::from(FILE)).expect("parse")
}

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

fn findings(body: &str) -> Vec<TaskNullAbandon> {
    let src = format!("{PRELUDE}\n{body}\n");
    let ws = semantic_pass(&mut [parse(&src)]).expect("clean program typechecks");
    task_null_abandon(&ws)
}

fn count(body: &str) -> usize {
    findings(body).len()
}

#[test]
fn prelude_typechecks() {
    assert_eq!(count("fn main() -> int { 3 }"), 0);
}

#[test]
fn field_null_assign_is_flagged() {
    let body = "fn bad(m: Mob) {\n    m.co = null\n}";
    assert_eq!(count(body), 1, "`m.co = null` abandons the coroutine mid-stack");
}

#[test]
fn diagnostic_names_the_slot() {
    let fs = findings("fn bad(m: Mob) {\n    m.co = null\n}");
    assert_eq!(fs.len(), 1, "expected the one finding: {fs:?}");
    assert_eq!(fs[0].place.as_deref(), Some("m.co"), "the message points at the slot");
}

#[test]
fn diagnostic_names_a_safe_nav_slot() {
    let fs = findings("fn bad(e: Entity<Mob>) {\n    e.get()?.co = null\n}");
    assert_eq!(fs.len(), 1, "expected the one finding: {fs:?}");
    assert_eq!(fs[0].place.as_deref(), Some("e.get()?.co"), "the whole path is re-spelled");
}

#[test]
fn diagnostic_names_a_global_slot() {
    let body = "global SLOT: Task? = null\nfn bad() {\n    SLOT = null\n}";
    let fs = findings(body);
    assert_eq!(fs.len(), 1, "expected the one finding: {fs:?}");
    assert_eq!(fs[0].place.as_deref(), Some("SLOT"));
}

// The whole point of the lint: the two verbs that DO run the defers.
#[test]
fn stop_call_is_clean() {
    let body = "fn good(m: Mob) {\n    stop(m, .co)\n}";
    assert_eq!(count(body), 0, "`stop` fires the terminal defers -- that is the fix");
}

// An initializer is not an assignment: there is no live coroutine in the slot to
// abandon, so `co: null` in a literal and the field default are both clean.
#[test]
fn struct_literal_initializer_is_clean() {
    let body = "fn good() -> Mob {\n    return Mob { co: null, name: null }\n}";
    assert_eq!(count(body), 0, "a literal initializes the slot, it does not overwrite one");
}

#[test]
fn field_default_is_clean() {
    // `co: Task? = null` in PRELUDE's struct decl is a default, lowered as a
    // StructDefaults body -- it must not read as an assignment either.
    assert_eq!(count("fn main() -> int { 3 }"), 0);
}

#[test]
fn non_task_nullable_field_is_clean() {
    let body = "fn fine(m: Mob) {\n    m.name = null\n}";
    assert_eq!(count(body), 0, "only a `Task?` slot has defers to skip");
}

#[test]
fn non_null_assign_is_clean() {
    // v1 covers the `= null` shape only. Overwriting a live slot with a NEW task
    // is the same hazard, but `replace` is the fix and telling a first store from
    // an overwrite needs liveness -- see the doc comment on the lint.
    let body = "fn fine(m: Mob) {\n    m.co = make_task()\n}";
    assert_eq!(count(body), 0, "not a null assignment");
}

#[test]
fn deep_safe_nav_target_is_flagged() {
    let body = "fn bad(e: Entity<Mob>) {\n    e.get()?.co = null\n}";
    assert_eq!(count(body), 1, "the safe-nav target is still a `Task?` place");
}

#[test]
fn local_task_slot_is_flagged() {
    let body = "fn bad() {\n    let t: Task? = make_task()\n    t = null\n}";
    assert_eq!(count(body), 1, "a local is a place too -- the task is just as abandoned");
}

#[test]
fn each_slot_is_judged_separately() {
    let body = "fn bad(m: Mob) {\n    m.co = null\n    m.other = null\n    m.hp = 0\n}";
    assert_eq!(count(body), 2, "two Task? slots nulled, the int is not one");
}

// `Warning`, not `Error`: two legal null-assign patterns exist (nulling a task
// just observed terminal, and cleanup inside the task's own terminal defer), and
// liveness-at-assignment is dataflow v1 does not attempt -- so those sites need
// a legal escape hatch, unlike `entity_across_yield`.
#[test]
fn finding_reports_at_warning_level() {
    let fs = findings("fn bad(m: Mob) {\n    m.co = null\n}");
    assert_eq!(fs.len(), 1, "expected the one finding: {fs:?}");
    assert_eq!(fs[0].to_diagnostic().level, CompilerErrorLevel::Warning);
}

#[test]
fn allow_comment_above_suppresses() {
    let body = "fn bad(m: Mob) {\n    // rock: allow(task_null_abandon)\n    m.co = null\n}";
    assert_eq!(count(body), 1, "raw finding still exists");
    let src = format!("{PRELUDE}\n{body}\n");
    let kept = filter_suppressed(findings(body), &src);
    assert!(kept.is_empty(), "allow comment above silences it: {kept:?}");
}

#[test]
fn trailing_allow_comment_suppresses() {
    let body = "fn bad(m: Mob) {\n    m.co = null // rock: allow(task_null_abandon)\n}";
    let src = format!("{PRELUDE}\n{body}\n");
    let kept = filter_suppressed(findings(body), &src);
    assert!(kept.is_empty(), "trailing allow comment silences it: {kept:?}");
}
