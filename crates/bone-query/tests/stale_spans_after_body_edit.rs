//! A body edit that shifts lines inside one fn must not leave navigation spans
//! of other fns at their pre-edit offsets.
//!
//! A parseable didChange localized to one body takes the incremental path,
//! which keeps other bodies' HIR and shifts spans. Spans stored inline in HIR
//! nodes (callee names, enum variants, literal and pattern field names) live
//! outside the shifted side tables that locals read, so each carrier is pinned.

use crate::fixture::PATH;
use bone_query::BoneWorkspace;
use lsp_types::Position;

const SHORT: &str = "    return 1\n";
const LONG: &str = "    let pad = 0\n    return 1 + pad\n";

/// `src` with its first fn body one line longer, shifting what follows down.
fn lengthened(src: &str) -> String {
    assert!(src.contains(SHORT));
    src.replacen(SHORT, LONG, 1)
}

const LOCALS: &str = "\
fn alpha() -> int {
    return 1
}

fn beta() -> int {
    let count = 2
    return count + count
}
";

const CALLS: &str = "\
fn alpha() -> int {
    return 1
}

fn beta() -> int {
    return alpha() + alpha()
}
";

const ENUM: &str = "\
enum Color {
    Red,
    Green,
}

fn alpha() -> int {
    return 1
}

fn beta(c: Color) -> int {
    match c {
        Color::Red => 1,
        Color::Green => 2,
    }
}
";

const STRUCT: &str = "\
fn helper() -> int {
    return 1
}

struct Point {
    x: int,
    y: int,
}

fn build() -> int {
    let p = Point { x: 1, y: 2 }
    return p.x + p.y
}
";

const GENERIC: &str = "\
fn helper() -> int {
    return 1
}

fn identity<T>(x: T) -> T {
    return x
}
";

/// Sorted (line, char) starts of every returned reference location.
fn ref_starts(bone: &BoneWorkspace, pos: Position) -> Vec<(u32, u32)> {
    let mut v: Vec<(u32, u32)> = bone
        .references(PATH, pos)
        .into_iter()
        .map(|loc| (loc.range.start.line, loc.range.start.character))
        .collect();
    v.sort();
    v
}

#[test]
fn references_track_line_shifts_from_another_fn_edit() {
    let local_short = ((5, 8), vec![(5, 8), (6, 11), (6, 19)]);
    let local_long = ((6, 8), vec![(6, 8), (7, 11), (7, 19)]);
    // (case, first text, second text, (cursor, references) before and after)
    let cases = [
        (
            "local",
            LOCALS.to_string(),
            lengthened(LOCALS),
            local_short.clone(),
            local_long.clone(),
        ),
        (
            "line removal",
            lengthened(LOCALS),
            LOCALS.to_string(),
            local_long,
            local_short,
        ),
        (
            "call sites",
            CALLS.to_string(),
            lengthened(CALLS),
            ((5, 12), vec![(0, 3), (5, 11), (5, 21)]),
            ((6, 12), vec![(0, 3), (6, 11), (6, 21)]),
        ),
        (
            "enum variant pattern",
            ENUM.to_string(),
            lengthened(ENUM),
            ((1, 4), vec![(1, 4), (11, 15)]),
            ((1, 4), vec![(1, 4), (12, 15)]),
        ),
    ];
    for (case, first, second, (p0, refs0), (p1, refs1)) in cases {
        let mut bone = BoneWorkspace::new();
        bone.update(PATH, first);
        let at = |(l, c)| Position::new(l, c);
        assert_eq!(ref_starts(&bone, at(p0)), refs0, "{case}: baseline");
        bone.update(PATH, second);
        assert_eq!(ref_starts(&bone, at(p1)), refs1, "{case}: after the edit");
    }
}

#[test]
fn document_highlight_tracks_line_shift_from_other_fn_edit() {
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, LOCALS.to_string());
    bone.update(PATH, lengthened(LOCALS));
    let mut starts: Vec<(u32, u32)> = bone
        .document_highlight(PATH, Position::new(6, 8))
        .into_iter()
        .map(|h| (h.range.start.line, h.range.start.character))
        .collect();
    starts.sort();
    assert_eq!(starts, [(6, 8), (7, 11), (7, 19)]);
}

/// Typed a keystroke at a time, the intermediate states fail to parse or lower
/// and revert to last-good before the final valid text arrives.
#[test]
fn references_track_line_shift_typed_char_by_char() {
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, LOCALS.to_string());
    let (head, tail) = LOCALS.split_at(LOCALS.find("    return 1").unwrap() + 12);
    let typed = "\n    let pad = 0";
    for i in 1..=typed.len() {
        bone.update(PATH, format!("{head}{}{tail}", &typed[..i]));
    }
    assert_eq!(
        ref_starts(&bone, Position::new(6, 8)),
        [(6, 8), (7, 11), (7, 19)]
    );
}

// The pinned tests cover only the span carriers someone named. The oracle
// below requires navigation at every offset after an incremental edit to
// match a from-scratch check of the same text, so any unshifted span shows.
// (bone-incremental's own oracle compares only `type_at`, which reads the
// shifted `node_index` and is blind to inline node spans.)

/// For every cursor position in `text`, the sorted `query` results there.
/// Empty results are dropped so a divergence report shows only the positions
/// that actually resolve to something.
fn nav_profile(
    bone: &BoneWorkspace,
    text: &str,
    query: impl Fn(&BoneWorkspace, Position) -> Vec<String>,
) -> Vec<((u32, u32), Vec<String>)> {
    let mut out = Vec::new();
    for (line, line_text) in text.lines().enumerate() {
        for ch in 0..=line_text.chars().count() {
            let pos = Position::new(line as u32, ch as u32);
            let mut r = query(bone, pos);
            r.sort();
            if !r.is_empty() {
                out.push(((line as u32, ch as u32), r));
            }
        }
    }
    out
}

/// Apply `before` then `after` incrementally, then compare references, hover,
/// and definition at every offset against a fresh workspace built from `after`
/// alone. All three read inline node spans, so each is an independent net.
fn assert_incremental_nav_matches_scratch(before: &str, after: &str) {
    let mut inc = BoneWorkspace::new();
    inc.update(PATH, before.to_string());
    inc.update(PATH, after.to_string());

    let mut scratch = BoneWorkspace::new();
    scratch.update(PATH, after.to_string());

    let refs = |b: &BoneWorkspace, p| {
        b.references(PATH, p)
            .into_iter()
            .map(|l| format!("{:?}", l.range))
            .collect::<Vec<_>>()
    };
    let hover = |b: &BoneWorkspace, p| {
        b.hover(PATH, p)
            .into_iter()
            .map(|h| format!("{h:?}"))
            .collect::<Vec<_>>()
    };
    let def = |b: &BoneWorkspace, p| {
        b.definition(PATH, p)
            .into_iter()
            .map(|l| format!("{l:?}"))
            .collect::<Vec<_>>()
    };

    assert_eq!(
        nav_profile(&inc, after, refs),
        nav_profile(&scratch, after, refs),
        "incremental references must equal from-scratch after the edit"
    );
    assert_eq!(
        nav_profile(&inc, after, hover),
        nav_profile(&scratch, after, hover),
        "incremental hover must equal from-scratch after the edit"
    );
    assert_eq!(
        nav_profile(&inc, after, def),
        nav_profile(&scratch, after, def),
        "incremental definition must equal from-scratch after the edit"
    );
}

#[test]
fn incremental_nav_matches_from_scratch_across_offsets() {
    // Between them these exercise locals, call sites, enum variants,
    // struct-literal and field-access spans, and fn generic params.
    for before in [LOCALS, CALLS, ENUM, STRUCT, GENERIC] {
        assert_incremental_nav_matches_scratch(before, &lengthened(before));
    }
}
