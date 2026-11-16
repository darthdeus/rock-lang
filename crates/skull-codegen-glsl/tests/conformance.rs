//! Structural preconditions for intrinsics with both a portable Rock fallback
//! and a backend native mapping. Runtime reference tests live in
//! `tests/intrinsics/scalar_reference.rock`; its scalar-returning tests also run
//! on GLSL. This harness checks that each fallback body exists and GLSL maps it
//! natively, so a backend cannot silently drop either side of that contract.

use skull_codegen_glsl::GlslIntrinsics;
use skull_mir::intrinsics::{
    IntrinsicTable, declared_intrinsics, is_intrinsic, is_native_required, unbacked_entries,
};

fn stdlib_ws() -> bone_core::workspace::Workspace {
    crate::support::workspace("fn main() {}").0
}

// Each has a portable Rock fallback and a GLSL native mapping.
const FALLBACK_AND_MAPPING: &[(&str, usize)] = &[
    ("min", 2),
    ("max", 2),
    ("clamp", 3),
    ("lerp", 3),
    ("smoothstep", 3),
    ("round", 1),
    ("sign", 1),
    ("fract", 1),
];

#[test]
fn fallback_plus_mapping_intrinsics_satisfy_the_differential_precondition() {
    let ws = stdlib_ws();
    for &(name, arity) in FALLBACK_AND_MAPPING {
        let f = ws
            .modules
            .iter()
            .flat_map(|m| m.functions.iter().map(|(_, f)| f))
            .find(|f| f.name.as_str() == name && f.param_decls.len() == arity)
            .unwrap_or_else(|| panic!("no stdlib fn `{name}`/{arity}"));

        assert!(is_intrinsic(f), "`{name}` must be @intrinsic");
        assert!(
            !is_native_required(f),
            "`{name}` must NOT be @native_required -- it has a portable fallback to diff against"
        );
        assert!(
            f.inline_body.is_none(),
            "`{name}` must have a Rock fallback body (the portable reference), not an inline(lua) mapping"
        );
        assert!(
            GlslIntrinsics.lookup(f).is_some(),
            "`{name}`/{arity} must have a GLSL native mapping (the differential's other side)"
        );
    }
}

// Fail-closed, END TO END against the REAL GLSL table: every GLSL entry is
// backed by a stdlib `@intrinsic` declaration, so `unbacked_entries` is empty.
// The JS sibling (skull-codegen-js) pins the same invariant on the toy table;
// this pins it on the shipping one -- the Vec-method/ctor/cast/texture entries
// too, not just the scalar proving set. Goes RED the moment a GLSL key drifts
// from its stdlib signature or an entry loses its `@intrinsic` annotation.
#[test]
fn every_glsl_entry_is_backed_by_a_stdlib_intrinsic() {
    let ws = stdlib_ws();
    let declared = declared_intrinsics(&ws);
    let unbacked = unbacked_entries(&GlslIntrinsics, &declared);
    assert!(
        unbacked.is_empty(),
        "GLSL table has entries no stdlib @intrinsic declaration backs (drift): {unbacked:?}"
    );
}
