use crate::common::run_luajit_capturing;

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
fn arena_random_at_a_nobox_element_works() {
    let lua = crate::common::try_emit_module(MARKED).expect("marked column element must emit");
    let (ok, out) = run_luajit_capturing(&format!("{lua}\nmain()\n"));
    assert!(ok);
    assert_eq!(out.trim(), "true");
}

#[test]
fn arena_random_at_a_nobox_element_targets_the_specialized_copy() {
    let lua = crate::common::try_emit_module(MARKED).expect("marked column element must emit");
    assert!(lua.contains("random_Arena_T_spec_"));
}

#[test]
fn arena_random_at_an_unmarked_column_element_works() {
    let lua = crate::common::try_emit_module(UNMARKED).expect("unmarked column element must emit");
    let (ok, out) = run_luajit_capturing(&format!("{lua}\nmain()\n"));
    assert!(ok);
    assert_eq!(out.trim(), "true");
}
