//! The wire-kind codes are a compiler-to-host ABI, so every compiler code is
//! pinned here. The monorepo's integration tests compare the engine constants.
//!
//! `WireKind::code()` is what the registration wire carries, and the host reads
//! it to decide how many components a payload element has and whether it goes
//! through the float or the int view. The Rock side spells the same numbers as
//! `WIRE_KIND_*` globals in the engine source. A push whose declared kind
//! disagrees with the caller's verb is rejected at runtime, so the explicit
//! values below keep compiler-side renumbering from silently changing the ABI.

use skull_mir::gpu_layout::WireKind;
use std::collections::BTreeMap;

/// Every kind, so a new variant cannot be added without deciding its code.
/// Written out rather than iterated, because there is no iterator over the enum
/// -- a new variant makes this fail to compile, which is the intent.
const ALL: [(WireKind, &str); 8] = [
    (WireKind::F32, "WIRE_KIND_F32"),
    (WireKind::F32x2, "WIRE_KIND_F32X2"),
    (WireKind::F32x3, "WIRE_KIND_F32X3"),
    (WireKind::F32x4, "WIRE_KIND_F32X4"),
    (WireKind::I32, "WIRE_KIND_I32"),
    (WireKind::U32, "WIRE_KIND_U32"),
    (WireKind::Bool32, "WIRE_KIND_BOOL32"),
    (WireKind::Mat4, "WIRE_KIND_MAT4"),
];

#[test]
fn every_kind_has_a_distinct_code() {
    let mut seen = BTreeMap::new();
    for (k, name) in ALL {
        if let Some(prev) = seen.insert(k.code(), name) {
            panic!("`{name}` and `{prev}` share wire code {}", k.code());
        }
    }
    assert_eq!(seen.len(), ALL.len());
}

/// The codes are WRITTEN OUT in `code()`, not derived from the discriminant,
/// precisely so reordering the enum cannot renumber them. This is the assertion
/// that makes that promise real -- without it, "written out" is a comment.
#[test]
fn the_codes_are_the_documented_numbers() {
    assert_eq!(WireKind::F32.code(), 0);
    assert_eq!(WireKind::F32x2.code(), 1);
    assert_eq!(WireKind::F32x3.code(), 2);
    assert_eq!(WireKind::F32x4.code(), 3);
    assert_eq!(WireKind::I32.code(), 4);
    assert_eq!(WireKind::U32.code(), 7);
    assert_eq!(WireKind::Bool32.code(), 5);
    assert_eq!(WireKind::Mat4.code(), 6);
}
