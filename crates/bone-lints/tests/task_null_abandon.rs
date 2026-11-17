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

#[test]
fn task_assignment_diagnostics() {
    for (body, places) in [
        ("fn main() {}", vec![]),
        ("fn bad(m: Mob) { m.co = null }", vec!["m.co"]),
        (
            "fn bad(e: Entity<Mob>) { e.get()?.co = null }",
            vec!["e.get()?.co"],
        ),
        (
            "global SLOT: Task? = null\nfn bad() { SLOT = null }",
            vec!["SLOT"],
        ),
        ("fn good(m: Mob) { stop(m, .co) }", vec![]),
        ("fn good() -> Mob { Mob { co: null, name: null } }", vec![]),
        ("fn fine(m: Mob) { m.name = null }", vec![]),
        ("fn fine(m: Mob) { m.co = make_task() }", vec![]),
        (
            "fn bad() { let t: Task? = make_task()\nt = null }",
            vec!["t"],
        ),
        (
            "fn bad(m: Mob) { m.co = null\nm.other = null\nm.hp = 0 }",
            vec!["m.co", "m.other"],
        ),
    ] {
        let fs = findings(body);
        assert_eq!(fs.len(), places.len(), "{body}: {fs:?}");
        for (f, place) in fs.iter().zip(places) {
            assert!(
                f.error
                    .message
                    .starts_with(&format!("`{place} = null` abandons")),
                "{f:?}"
            );
            assert_eq!(f.level(), CompilerErrorLevel::Warning);
        }
    }
}
