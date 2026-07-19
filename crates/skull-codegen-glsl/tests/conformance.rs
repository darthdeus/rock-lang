//! Conformance harness skeleton (docs/intrinsics-plan.md, slice 5): the
//! DIFFERENTIAL precondition for the intrinsics that have BOTH a portable Rock
//! fallback AND a backend native mapping.
//!
//! A real differential run executes each backend twice -- native mappings vs
//! fallbacks-forced -- and diffs the outputs against the portable reference. On
//! luajit (the only backend that executes today) an intrinsic has exactly ONE
//! body, so numeric differencing is degenerate; the executable reference values
//! live in `tests/intrinsics/fallback_reference.rock`. What this harness pins is
//! the STRUCTURAL precondition a second executing backend (JS, C) will diff
//! against: for each fallback+mapping intrinsic, a portable Rock fallback body
//! exists (a new backend inherits it) AND GLSL maps it natively. It goes red if
//! a fallback is deleted or a GLSL mapping drops.

use std::sync::LazyLock;
use ustr::Ustr;

use skull_codegen_glsl::GlslIntrinsics;
use skull_mir::intrinsics::{
    declared_intrinsics, is_intrinsic, is_native_required, unbacked_entries, IntrinsicTable,
};

static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_host::stdlib_loader::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(format!("<stdlib:{}>", f.name).as_str());
            skull_parser::parse_module(f.contents, id).expect("stdlib parse")
        })
        .collect()
});

fn stdlib_ws() -> bone_semantic::inference::Workspace {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module("fn main() {}", Ustr::from("<test>")).expect("parse"));
    bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass")
}

// Each has a portable Rock fallback (the reference impl) AND a GLSL native
// mapping -- the differential precondition. The Rock fallback runs on luajit;
// GLSL supplies a native `min`/`clamp`/... a future backend must match.
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
            f.lua_body.is_none(),
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
