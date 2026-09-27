//! The wire-kind codes cross a language boundary, so both sides are pinned here.
//!
//! `WireKind::code()` is what the registration wire carries, and the host reads
//! it to decide how many components a payload element has and whether it goes
//! through the float or the int view. The Rock side spells the same numbers as
//! `WIRE_KIND_*` globals in `stone/engine/rendering/uniform_block.rock`.
//!
//! Nothing in the type system connects the two. A push whose declared kind
//! disagrees with the caller's verb is REJECTED at runtime, so a wrong mapping
//! fails at the first draw rather than writing silent garbage -- but that is a
//! backstop, not a check, and it only fires for a shader someone actually draws.
//! This asserts the correspondence directly, in both directions.
//!
//! Read out of the Rock source rather than restated: a copy of the constants
//! here would agree with itself forever while the engine drifted.

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

/// The Rock globals must equal the compiler's codes for the kinds an array may
/// hold. Parsed out of the engine source, so the assertion is against what the
/// engine actually says rather than against a copy of it.
#[test]
fn the_rock_constants_match_the_compilers_codes() {
    let src = std::fs::read_to_string(engine_source()).expect("uniform_block.rock");

    // The four an array may hold. The other three have no `WIRE_KIND_*` global
    // because `uniform_array_block` refuses them as array elements, so the host
    // can never be handed one -- an absence with a reason, asserted below.
    for (kind, name) in [
        (WireKind::F32, "WIRE_KIND_F32"),
        (WireKind::F32x2, "WIRE_KIND_F32X2"),
        (WireKind::F32x4, "WIRE_KIND_F32X4"),
        (WireKind::I32, "WIRE_KIND_I32"),
        (WireKind::U32, "WIRE_KIND_U32"),
    ] {
        let declared = rock_global(&src, name)
            .unwrap_or_else(|| panic!("`{name}` is not declared in uniform_block.rock"));
        assert_eq!(
            declared,
            kind.code() as i64,
            "`{name}` is {declared} in Rock but {} in the compiler",
            kind.code()
        );
    }

    // A `Vec3`/`Mat4`/`bool` array is rejected by the layout, so a constant for
    // one would describe a payload that cannot exist. Its presence would mean
    // someone widened one side without the other.
    for name in ["WIRE_KIND_F32X3", "WIRE_KIND_MAT4", "WIRE_KIND_BOOL32"] {
        assert!(
            rock_global(&src, name).is_none(),
            "`{name}` exists in Rock, but `uniform_array_block` refuses that element kind -- \
             one side was widened without the other"
        );
    }
}

/// `global <NAME>: int = <n>` or `const <NAME>: int = <n>` in Rock source.
/// Both spellings declare the same wire code; a memory class makes the choice,
/// so matching only one makes the negative half of this test vacuous.
fn rock_global(src: &str, name: &str) -> Option<i64> {
    for keyword in ["global", "const"] {
        let needle = format!("{keyword} {name}: int = ");
        // A modules-v1 declaration may carry its visibility first.
        let decl = |l: &str| {
            let l = l.trim_start();
            let l = l.strip_prefix("pub(package) ").or_else(|| l.strip_prefix("pub ")).unwrap_or(l);
            l.strip_prefix(&needle).map(str::to_string)
        };
        if let Some(rest) = src.lines().find_map(decl) {
            return rest.split_whitespace().next()?.parse().ok();
        }
    }
    None
}

fn engine_source() -> std::path::PathBuf {
    // The crate sits at crates/skull-mir; the engine is a sibling of `crates`.
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("repo root")
        .join("stone/engine/rendering/uniform_block.rock")
}
