//! A tuple-returning call carries its concrete owner at every call path.
//!
//! A column-stored element cannot be served by an erased body -- that body reads
//! a `_data` leaf the storage does not have -- so dropping the owner on the tuple
//! path is a miscompile, not a missed optimization. These are the three paths
//! that emit a tuple call.

use crate::common::compile_and_run;

/// A plain call to a generic returning `(int, T)?`. Not an arena: the defect is
/// in the tuple call path, and pinning it here keeps it found if the arena
/// surface changes shape.
const PLAIN_CALL: &str = r#"
nobox value NP { x: float, y: float }

struct Holder<T> { items: [T] }

fn Holder<T>.pick(h) -> (int, T)? {
    if vec_len(h.items) == 0 { return null }
    return (0, h.items[0])
}

fn main() {
    let full: Holder<NP> = Holder { items: [NP { x: 3.0, y: 4.0 }] }
    print("{full.pick() != null}")
}
"#;

/// `recv?.method()` returning a tuple -- the safe-nav UFCS path.
const SAFE_NAV: &str = r#"
nobox value NP { x: float, y: float }

struct Holder<T> { items: [T] }

fn Holder<T>.first_pair(h) -> (int, T) {
    return (0, h.items[0])
}

fn main() {
    let h: Holder<NP>? = Holder { items: [NP { x: 1.0, y: 2.0 }] }
    print("{h?.first_pair() != null}")
}
"#;

/// A directly-called `next()` -- the `(bool, T)` iterator protocol outside a
/// `for` head, where the synthesized-next spec path does not apply.
const DIRECT_NEXT: &str = r#"
nobox value NP { x: float, y: float }

fn main() {
    let v: [NP] = [NP { x: 1.0, y: 2.0 }]
    let it = v.iter()
    let (ok, p) = it.next()
    print("{ok}")
}
"#;

#[test]
fn a_plain_tuple_returning_generic_call_reaches_the_specialized_copy() {
    assert_eq!(compile_and_run(PLAIN_CALL), "true");
}

#[test]
fn a_safe_nav_tuple_returning_call_reaches_the_specialized_copy() {
    assert_eq!(compile_and_run(SAFE_NAV), "true");
}

#[test]
fn a_directly_called_next_reaches_the_specialized_copy() {
    assert_eq!(compile_and_run(DIRECT_NEXT), "true");
}
