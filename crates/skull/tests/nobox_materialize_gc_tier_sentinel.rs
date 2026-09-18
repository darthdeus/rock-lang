//! SENTINEL: `materialize(v)` of a `nobox` value struct at the GC-carrying tier
//! (any string, closure, array or flat-enum field) does not compile.
//!
//! The explicit box is the sanctioned escape from the guarantee, so it should
//! build the value's table (`<Name>_lct` / `_vt`). Today
//! `flatten_nobox_nested_ctors` (`crates/skull-transform/src/lib.rs`) accepts
//! only CData-tier nobox ctors and rejects the rest with a location-less
//! "without CData storage" error. Behind it sits a second gap: a flat-enum
//! field's part of the value Group reaches `materialize_group`
//! (`crates/skull-mir/src/lower/structs.rs`) as if it were a struct and fails
//! with "a planned value group has too many fields"; `pack` already has the
//! flat-enum arm it needs. Recorded in TODO.md.
//!
//! When this assertion fails, the GC-tier explicit box landed: flip it to
//! assert the program runs, and cover a flat-enum field too.

use crate::common::try_emit_module;

const SRC: &str = r#"
nobox value SRow { kind: int, label: string }
inline(lua) fn show(v: any) -> string = "return tostring(v.kind) .. v.label"
fn main() {
    let v = SRow { kind: 1, label: "a" }
    print(show(materialize(v)))
}
"#;

#[test]
fn materializing_a_gc_tier_nobox_value_is_still_rejected() {
    let err = try_emit_module(SRC).expect_err(
        "PINNED BUG: a GC-tier nobox materialize compiled -- flip this sentinel",
    );
    assert!(
        err.to_string().contains("without CData storage"),
        "PINNED BUG changed shape: {err}"
    );
}
