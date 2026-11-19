use crate::common::{compile_and_run, emit_module, run_main};

const MARKED: &str = r#"
nobox value NP { x: float, y: float }

fn main() {
    let a: Arena<NP> = arena_new()
    a.insert(NP { x: 1.0, y: 1.0 })
    a.insert(NP { x: 2.0, y: 2.0 })
    print("{a.random() != null}")
}
"#;

const UNMARKED: &str = r#"
value CV { x: float, y: float }

fn main() {
    let a: Arena<CV> = arena_new()
    a.insert(CV { x: 1.0, y: 1.0 })
    a.insert(CV { x: 2.0, y: 2.0 })
    print("{a.random() != null}")
}
"#;

#[test]
fn arena_random_works_at_nobox_and_unmarked_column_elements() {
    let marked = emit_module(MARKED);
    assert!(marked.contains("random_Arena_T_spec_"));
    assert_eq!(run_main(&marked), "true");
    assert_eq!(compile_and_run(UNMARKED), "true");
}
