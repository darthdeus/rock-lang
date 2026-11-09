//! `Arena<P>.random()` at a `nobox` P, and the same op at an unmarked
//! column-stored element as its control.
//!
//! `random` returns a TUPLE (`(Entity<T>, T)?`), and a tuple-returning call used
//! to drop its instantiation key, so it fell through to the shared generic body
//! and read `arena.data[i]` through the boxed accessor (`__vec_checked_get`) on
//! column storage that has no `_data` field. The key is now attached on the tuple
//! path too, so the call targets the specialized copy that reads the columns.
//!
//! `a.random() != null` is the only shape a marked element can be consumed in
//! today -- binding or projecting the `T` half materializes and is rejected -- so
//! the load-bearing assertion here is that the program RUNS. Its `true` does not
//! prove the presence slot: that comparison folds to a constant, which is pinned
//! separately in `nobox_tuple_option_presence_sentinel.rs`.

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

/// The control: identical program, element column-stored but NOT marked.
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
    assert!(
        ok,
        "`Arena<NP>.random()` must work for a marked element:\n{out}"
    );
    assert_eq!(
        out.trim(),
        "true",
        "expected a live entry to be selected:\n{out}"
    );
}

/// The call must reach the SPECIALIZED copy: reading the columns is the whole
/// fix, and asserting only on the run would stay green if a future change served
/// this from the shared generic body again.
#[test]
fn arena_random_at_a_nobox_element_targets_the_specialized_copy() {
    let lua = crate::common::try_emit_module(MARKED).expect("marked column element must emit");
    assert!(
        lua.contains("random_Arena_T_spec_"),
        "the tuple-returning `random` call must carry its instantiation key and \
         target the specialized copy:\n{lua}"
    );
}

/// Bounds the sentinel above: without this, a fix that broke `random` for EVERY
/// column element would leave the sentinel green and look like progress.
#[test]
fn arena_random_at_an_unmarked_column_element_works() {
    let lua = crate::common::try_emit_module(UNMARKED).expect("unmarked column element must emit");
    let (ok, out) = run_luajit_capturing(&format!("{lua}\nmain()\n"));
    assert!(
        ok,
        "`Arena<CV>.random()` must work for a column-stored element:\n{out}"
    );
    assert_eq!(
        out.trim(),
        "true",
        "expected a live entry to be selected:\n{out}"
    );
}
