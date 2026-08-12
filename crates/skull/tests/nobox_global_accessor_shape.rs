//! The shape facts the `nobox`-globals reflection accessor
//! (`build_nobox_global_accessors`, `rock-cli/src/skull_backend.rs`) decides on.
//!
//! That accessor answers two questions about a global's leaf-path sequence: is
//! any leaf a nullable child's PRESENCE tag (unaddressable as `v.<path>`, so it
//! rejects), and which of the three ctor suffixes does the type take. Both were
//! answered by punctuation proxies -- "some path is a proper prefix of another"
//! and "some path contains a dot" -- rather than by the oracles that already
//! exist for each question.
//!
//! A `nobox` value with NO data leaves is where the proxies and the oracles
//! disagree: it contributes exactly ONE leaf, keyed on the field's own name with
//! nothing below it (`leaf_names_rec`), so no path is dotted and no path is a
//! prefix of another. The same shape is pinned for the Lua ctor site by
//! `tests/nobox_zero_leaf_child.rock`, where picking by punctuation was a live
//! `attempt to call global 'Tagged_vt' (a nil value)`.
//!
//! Measuring it turned up two further shapes the punctuation rules got wrong,
//! both live crashes through `rock frame --dump-state` on a real project:
//! `Tagged_vt` (a nil value) for the zero-leaf child, and `cannot convert
//! 'number' to 'struct'` for an FFI-clean struct with a NESTED value field --
//! there `_ct` is positional over DECLARED FIELDS while the accessor passes
//! LEAVES, so arity and type both differ.
//!
//! The rule that replaced both proxies: the accessor is positional over the leaf
//! sequence, so it is sound exactly when the CTOR is too. `_lct` is that by
//! construction (presence tags included); `_ct`/`_vt` are only while every field
//! is primitive.
//!
//! These assert the shape layer directly, so they need no game harness -- which
//! is what kept this lead untested (the registry block is appended only by the
//! game emit paths).


use skull_mir::lower::{nobox_global_accessor_shape, NoboxGlobalAccessorShape, ScalarCtx};
use bone_semantic::inference::Workspace;

/// The parent must carry more than four leaves so it exceeds the column budget,
/// exactly as `tests/nobox_zero_leaf_child.rock` explains: at four or fewer,
/// column storage means no ctor is emitted and the choice is never made.
const ZERO_LEAF_CHILD: &str = r#"
nobox value Marker {}
nobox value Tagged { m: Marker?, a: float, b: float, c: float, d: float }

global G: Tagged = Tagged { m: Marker {}, a: 1.0, b: 2.0, c: 3.0, d: 4.0 }

fn main() { print("{G.a}") }
"#;

/// A nullable `nobox` child WITH data leaves: the child contributes `m` plus
/// `m.x`/`m.y`, so a path is dotted and one path is a proper prefix of another.
/// Leaf-table, so it stays buildable.
const DATA_LEAF_CHILD: &str = r#"
nobox value Inner { x: float, y: float }
nobox value Held { m: Inner?, a: float, b: float, c: float, d: float }

global H: Held = Held { m: Inner { x: 1.0, y: 2.0 }, a: 1.0, b: 2.0, c: 3.0, d: 4.0 }

fn main() { print("{H.a}") }
"#;

fn accessor_of(src: &str, global: &str) -> (Workspace, NoboxGlobalAccessorShape) {
    let ws = crate::common::ws_with_stdlib(src);
    let scx = ScalarCtx::new(&ws);
    let shape = ws
        .modules
        .iter()
        .flat_map(|m| m.globals.iter())
        .find(|(_, g)| g.name.as_str() == global)
        .and_then(|(_, g)| nobox_global_accessor_shape(&ws, &scx, g.ty))
        .unwrap_or_else(|| panic!("global `{global}` has no nobox accessor shape"));
    (ws, shape)
}

/// Both punctuation proxies read CLEAN on this shape, while both structural
/// oracles say otherwise. This is the whole defect in one assertion pair: the
/// guard passes where a presence leaf exists, and the suffix rule picks by dot
/// where the type is leaf-table.
#[test]
fn a_zero_data_leaf_child_defeats_both_punctuation_proxies() {
    let (_ws, acc) = accessor_of(ZERO_LEAF_CHILD, "G");
    assert_eq!(
        acc.paths.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
        ["m", "a", "b", "c", "d"],
        "the child contributes one leaf under its own name, with nothing below it"
    );

    // Proxy 1, the old presence guard: no path is a proper prefix of another.
    assert!(
        !acc.paths
            .iter()
            .any(|p| acc.paths.iter().any(|q| q.as_str().starts_with(&format!("{p}.")))),
        "the prefix proxy finds no presence leaf here"
    );
    // The oracle: there IS one, and it is the child's own name.
    assert_eq!(
        acc.presence_path.map(|p| p.to_string()).as_deref(),
        Some("m"),
        "`value_leaf_presence_flags` reports the presence tag the prefix proxy missed"
    );

    // Proxy 2, the old ctor suffix: no path is dotted, so that rule fell through
    // to the GC test and picked `_vt` -- the ctor a leaf-table struct never gets.
    assert!(
        !acc.paths.iter().any(|p| p.contains('.')),
        "the dotted proxy sees a flat path list here"
    );
    assert_eq!(
        acc.ctor_suffix, "_lct",
        "the shared physical storage plan says this is a leaf-table value"
    );
    // And a presence tag does NOT make it unbuildable: `_lct` takes tags like any
    // other leaf. Measured end-to-end -- this global round-trips through
    // `--dump-state` as `{"__ty":"Tagged", ...}`.
    assert!(acc.unbuildable.is_none());
}

/// Bounds it: with a child that HAS data leaves, proxies and oracles agree, so a
/// fix that routed through the oracles cannot have changed this shape's answer.
#[test]
fn a_child_with_data_leaves_agrees_under_both_rules() {
    let (_ws, acc) = accessor_of(DATA_LEAF_CHILD, "H");
    assert!(
        acc.paths
            .iter()
            .any(|p| acc.paths.iter().any(|q| q.as_str().starts_with(&format!("{p}.")))),
        "a data-carrying nullable child makes one path a proper prefix of another"
    );
    assert!(acc.paths.iter().any(|p| p.contains('.')), "and makes a path dotted");
    assert_eq!(acc.presence_path.map(|p| p.to_string()).as_deref(), Some("m"));
    assert_eq!(acc.ctor_suffix, "_lct");
    assert!(acc.unbuildable.is_none(), "a leaf-table ctor IS its leaf sequence");
}

/// An FFI-clean parent with a NON-nullable nested value field. No presence tag,
/// so the old prefix guard passed; dotted paths, so the old suffix rule said
/// `_lct` -- but the type is cdata and its real ctor is `Held_ct`, positional
/// over `(Inner, float, float, float, float)`. Feeding it six floats was the
/// live `cannot convert 'number' to 'struct'`.
const FFI_CLEAN_NESTED: &str = r#"
nobox value Inner { x: float, y: float }
nobox value Held { m: Inner, a: float, b: float, c: float, d: float }

global H: Held = Held { m: Inner { x: 7.0, y: 8.0 }, a: 1.0, b: 2.0, c: 3.0, d: 4.0 }

fn main() { print("{H.a}") }
"#;

/// The shape every game already has, and the reason this could not simply be
/// made stricter: flat, FFI-clean, `_ct`, and its ctor IS its leaf sequence.
const FLAT: &str = r#"
nobox value Flat { x: float, y: float }

global F: Flat = Flat { x: 3.0, y: 4.0 }

fn main() { print("{F.x}") }
"#;

#[test]
fn an_ffi_clean_nested_field_is_unbuildable_and_says_why() {
    let (_ws, acc) = accessor_of(FFI_CLEAN_NESTED, "H");
    assert_eq!(acc.ctor_suffix, "_ct", "an FFI-clean parent is cdata, not leaf-table");
    assert_eq!(acc.presence_path, None, "no nullable child, so no presence tag");
    let why = acc.unbuildable.expect("a declared-field ctor cannot take a leaf sequence");
    assert!(
        why.contains("Held_ct") && why.contains("m.x"),
        "the message must name the ctor and the nested leaf: {why}"
    );
}

/// Bounds the rejection: the shape in every game must stay buildable, or the
/// stricter rule would have broken all of them.
#[test]
fn a_flat_ffi_clean_global_stays_buildable() {
    let (_ws, acc) = accessor_of(FLAT, "F");
    assert_eq!(acc.ctor_suffix, "_ct");
    assert_eq!(acc.presence_path, None);
    assert!(acc.unbuildable.is_none(), "a flat cdata ctor IS its leaf sequence");
}
