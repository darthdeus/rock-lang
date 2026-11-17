//! Control 4 of the 2.4 contract: a column
//! receiver must not REACH the boxed `vec_push` body, and if its column route
//! were missing the call would reject rather than fall through.
//!
//! `vec_push`'s receiver is declared `[T]`, which names the vec layout, so the
//! boxed body reads `_data`. `column_push` (`lower/call.rs`) replaces the call
//! at a column-eligible element and emits stores directly. The two assertions
//! below are the same fact from both sides, and both have a population -- a
//! test that only checked the column case would pass just as well if `vec_push`
//! had stopped being emitted at all.
//!
//! The fall-through half is verified by MUTATION rather than pinned here:
//! deleting the `column_push` arm makes the column case reject with the
//! 2.4 diagnostic, because the receiver's declared `[T]` names a layout the
//! column element does not have. It cannot silently reach `_data`.

use crate::common::{emit_module, function_body, run_main};
const COLUMN_RECEIVER: &str = r#"
value PushCell { a: int, b: int }

fn main() {
    let xs: [PushCell] = []
    xs.push(PushCell { a: 7, b: 8 })
    print("{xs[0].a}")
}
"#;

/// The DENOMINATOR. A boxed element gets the `_data` splice, so its ABSENCE
/// above is evidence about routing rather than about `vec_push` having stopped
/// being reachable at all.
const BOXED_RECEIVER: &str = r#"
fn main() {
    let ns: [int] = []
    ns.push(7)
    print("{ns[0]}")
}
"#;

/// The payload position: a column vec pushed into a boxed outer. The outer's
/// element is a Vec, not a scalar value struct, so the outer stays boxed and
/// takes the `_data` splice -- with the column vec stored as an opaque value.
/// This is the case that made the payload's contract `any` rather than `T`.
const COLUMN_PAYLOAD: &str = r#"
value PushCell { a: int, b: int }

fn main() {
    let outer: [[PushCell]] = []
    let inner: [PushCell] = [PushCell { a: 1, b: 2 }]
    outer.push(inner)
    print("{outer[0][0].a}")
}
"#;

/// Whether the boxed body reached this call site. `vec_push` is `@inline`, so
/// it is SPLICED rather than called -- there is no call to look for, and the
/// observable is the boxed layout the splice writes.
fn reaches_boxed_body(lua: &str) -> bool {
    // `main`'s body: the always-emitted `vec_push` definition contains `._data`
    // too, so a module-wide match would answer yes for any program.
    function_body(lua, "main").contains("._data")
}

#[test]
fn a_column_receiver_never_reaches_the_boxed_body() {
    let lua = emit_module(COLUMN_RECEIVER);
    assert!(
        !reaches_boxed_body(&lua),
        "a column push must emit per-leaf stores and never the `_data` splice"
    );
    assert_eq!(run_main(&lua), "7");
}

/// Without this the assertion above is unfalsifiable.
#[test]
fn a_boxed_receiver_does_reach_the_boxed_body() {
    let lua = emit_module(BOXED_RECEIVER);
    assert!(
        reaches_boxed_body(&lua),
        "a boxed push is exactly the case the boxed body serves"
    );
    let defs: Vec<&str> = lua
        .match_indices("function vec_push")
        .filter_map(|(i, _)| lua[i..].lines().next())
        .collect();
    assert_eq!(
        defs.len(),
        1,
        "an opaque boundary has one definition, so exactly one `vec_push` definition \
         may exist: {defs:?}"
    );
    assert_eq!(run_main(&lua), "7");
}

#[test]
fn a_column_payload_reaches_the_boxed_body_and_is_correct() {
    let lua = emit_module(COLUMN_PAYLOAD);
    assert!(
        reaches_boxed_body(&lua),
        "the OUTER vec is boxed, so its push is the ordinary `_data` splice"
    );
    assert_eq!(run_main(&lua), "1");
}
