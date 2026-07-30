//! SENTINEL -- a NULL tuple-option return from a generic fn reads as non-null
//! when the tuple carries a `nobox` element.
//!
//! A `-> (int, T)?` at a marked `T` splats its slots, and the call site rebuilds
//! a `Tuple` repr from them without consulting the presence slot. The rebuilt
//! repr is structurally never nil, so `f() != null` folds to the constant `true`
//! and the null branch is unreachable -- the callee's `return null` is lost at
//! the boundary. `Arena<NP>.random()` on an EMPTY arena reports a hit for this
//! reason.
//!
//! Distinct from the accessor-selection defect in
//! `nobox_arena_random_sentinel.rs`: that one picked the wrong BODY (shared vs
//! specialized) and crashed; this one reaches the right body and silently drops
//! the option-ness at the ABI. Fixing the former did not touch it -- it
//! reproduces identically at `3c8ccefeb`.
//!
//! It is specific to the MARK. The controls below -- an unmarked value element,
//! and a non-generic fn returning `(int, NP)?` -- both report null correctly,
//! which scopes this to the splatting tuple ABI rather than to nullable tuples
//! or to generics.
//!
//! PINNED BEHAVIOR (BUG): the marked generic is asserted to print `true` for a
//! call that returned null. When the fix lands this test goes RED. Flip it --
//! assert `false`, matching the controls -- and delete this paragraph.

mod common;

use std::process::Command;

fn run_capturing(prog: &str, tag: &str) -> (bool, String) {
    let path = std::env::temp_dir().join(format!("skull_sentinel_{}_{tag}.lua", std::process::id()));
    std::fs::write(&path, prog).expect("write program");
    let out = Command::new("luajit").arg(&path).output().expect("spawn luajit");
    let _ = std::fs::remove_file(&path);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), combined)
}

fn run_module(src: &str, tag: &str) -> String {
    let lua = common::try_emit_module(src).expect("must emit");
    let (ok, out) = run_capturing(&format!("{lua}\nmain()\n"), tag);
    assert!(ok, "program must run:\n{out}");
    out.trim().to_string()
}

const MARKED: &str = r#"
nobox value NP { x: float, y: float }

struct Holder<T> { items: [T] }

fn Holder<T>.pick(h) -> (int, T)? {
    if vec_len(h.items) == 0 { return null }
    return (0, h.items[0])
}

fn main() {
    let empty: Holder<NP> = Holder { items: [] }
    print("{empty.pick() != null}")
}
"#;

/// Control: identical generic, element column-stored but NOT marked.
const UNMARKED: &str = r#"
value CV { x: float, y: float }

struct Holder<T> { items: [T] }

fn Holder<T>.pick(h) -> (int, T)? {
    if vec_len(h.items) == 0 { return null }
    return (0, h.items[0])
}

fn main() {
    let empty: Holder<CV> = Holder { items: [] }
    print("{empty.pick() != null}")
}
"#;

/// Control: a marked element in a tuple option, but no generic to specialize.
const MARKED_NON_GENERIC: &str = r#"
nobox value NP { x: float, y: float }

fn pick(flag: bool) -> (int, NP)? {
    if !flag { return null }
    return (1, NP { x: 3.0, y: 4.0 })
}

fn main() {
    print("{pick(false) != null}")
}
"#;

#[test]
fn a_null_tuple_option_at_a_nobox_element_reads_as_non_null_sentinel() {
    let got = run_module(MARKED, "tuple_option_marked");
    assert_eq!(
        got, "true",
        "PINNED BEHAVIOR (BUG): a `return null` from a tuple-returning generic \
         at a marked element is expected to read as NON-null. It reported \
         `{got}`, so the fix landed -- flip this to assert `false`, matching the \
         controls below."
    );
}

/// Bounds the sentinel: without this, a fix that broke the presence slot for
/// EVERY column element would leave the sentinel green and look like progress.
#[test]
fn a_null_tuple_option_at_an_unmarked_column_element_reads_as_null() {
    assert_eq!(
        run_module(UNMARKED, "tuple_option_unmarked"),
        "false",
        "an unmarked column element must still report null correctly"
    );
}

/// Bounds the sentinel on the other axis: the mark alone is not enough, the
/// splatting tuple ABI needs a generic to specialize through.
#[test]
fn a_null_tuple_option_from_a_non_generic_fn_reads_as_null() {
    assert_eq!(
        run_module(MARKED_NON_GENERIC, "tuple_option_non_generic"),
        "false",
        "a non-generic tuple option must report null correctly even at a mark"
    );
}
