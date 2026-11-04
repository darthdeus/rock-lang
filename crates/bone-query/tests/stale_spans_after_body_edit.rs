//! A body edit that shifts lines (add/remove a line inside one fn) must not
//! leave reference/document-highlight spans of OTHER fns pointing at their
//! pre-edit offsets. Symptom: edit fn A, cursor on a symbol in fn B ->
//! occurrences render one line off.

use bone_query::BoneWorkspace;
use lsp_types::Position;

const PATH: &str = "/tmp/rock_lsp_bone_stale_spans.rock";

const BEFORE: &str = "\
fn alpha() -> int {
    return 1
}

fn beta() -> int {
    let count = 2
    return count + count
}
";

// One line added inside `alpha`'s body: everything in `beta` shifts down 1.
const AFTER: &str = "\
fn alpha() -> int {
    let pad = 0
    return 1 + pad
}

fn beta() -> int {
    let count = 2
    return count + count
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
fn references_track_line_shift_from_other_fn_edit() {
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, BEFORE.to_string());

    // `count` in BEFORE: decl (5,8), uses (6,11) and (6,19).
    assert_eq!(
        ref_starts(&bone, Position::new(5, 8)),
        vec![(5, 8), (6, 11), (6, 19)],
        "pre-edit baseline"
    );

    bone.update(PATH, AFTER.to_string());

    // Same symbol, now one line lower: decl (6,8), uses (7,11) and (7,19).
    assert_eq!(
        ref_starts(&bone, Position::new(6, 8)),
        vec![(6, 8), (7, 11), (7, 19)],
        "post-edit references must use post-edit offsets"
    );
}

#[test]
fn document_highlight_tracks_line_shift_from_other_fn_edit() {
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, BEFORE.to_string());
    bone.update(PATH, AFTER.to_string());

    let mut starts: Vec<(u32, u32)> = bone
        .document_highlight(PATH, Position::new(6, 8))
        .into_iter()
        .map(|h| (h.range.start.line, h.range.start.character))
        .collect();
    starts.sort();
    assert_eq!(
        starts,
        vec![(6, 8), (7, 11), (7, 19)],
        "post-edit highlights must use post-edit offsets"
    );
}

// A single parseable didChange localized to one body takes the incremental
// fast path (`try_incremental_body_edit`), which keeps other bodies' HIR and
// patches spans via `span_shift`. Spans stored INLINE in HIR nodes (callee
// name spans, enum-variant spans, literal/pattern field names) are separate
// from the shifted side tables -- these tests pin them. Locals (above) read
// the side tables and can't catch this class.

const CALL_BEFORE: &str = "\
fn alpha() -> int {
    return 1
}

fn beta() -> int {
    return alpha() + alpha()
}
";

const CALL_AFTER: &str = "\
fn alpha() -> int {
    let pad = 0
    return 1 + pad
}

fn beta() -> int {
    return alpha() + alpha()
}
";

#[test]
fn call_site_references_track_line_shift() {
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, CALL_BEFORE.to_string());
    assert_eq!(
        ref_starts(&bone, Position::new(5, 12)),
        vec![(0, 3), (5, 11), (5, 21)],
        "pre-edit baseline (decl + both call sites)"
    );

    bone.update(PATH, CALL_AFTER.to_string());
    assert_eq!(
        ref_starts(&bone, Position::new(6, 12)),
        vec![(0, 3), (6, 11), (6, 21)],
        "post-edit call-site references must use post-edit offsets"
    );
}

const ENUM_BEFORE: &str = "\
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

const ENUM_AFTER: &str = "\
enum Color {
    Red,
    Green,
}

fn alpha() -> int {
    let pad = 0
    return 1 + pad
}

fn beta(c: Color) -> int {
    match c {
        Color::Red => 1,
        Color::Green => 2,
    }
}
";

#[test]
fn enum_variant_references_track_line_shift() {
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, ENUM_BEFORE.to_string());
    assert_eq!(
        ref_starts(&bone, Position::new(1, 4)),
        vec![(1, 4), (11, 15)],
        "pre-edit baseline (variant decl + match pattern)"
    );

    bone.update(PATH, ENUM_AFTER.to_string());
    assert_eq!(
        ref_starts(&bone, Position::new(1, 4)),
        vec![(1, 4), (12, 15)],
        "post-edit variant references must use post-edit offsets"
    );
}

/// The live-editor path: the new line arrives one keystroke at a time, so the
/// session sees broken intermediate states (`l`, `le`, `let`, `let p`, ...)
/// that fail parse/lower and revert to last-good, before the final valid text.
#[test]
fn references_track_line_shift_typed_char_by_char() {
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, BEFORE.to_string());
    assert_eq!(
        ref_starts(&bone, Position::new(5, 8)),
        vec![(5, 8), (6, 11), (6, 19)],
        "pre-edit baseline"
    );

    // Type "\n    let pad = 0" at the end of `return 1` (line 1), one char at
    // a time -- every intermediate state is a full-text didChange.
    let head = "fn alpha() -> int {\n    return 1";
    let tail = "\n}\n\nfn beta() -> int {\n    let count = 2\n    return count + count\n}\n";
    let typed = "\n    let pad = 0";
    for i in 1..=typed.len() {
        bone.update(PATH, format!("{head}{}{tail}", &typed[..i]));
    }

    // `beta` shifted down one line: decl (6,8), uses (7,11) and (7,19).
    assert_eq!(
        ref_starts(&bone, Position::new(6, 8)),
        vec![(6, 8), (7, 11), (7, 19)],
        "references after char-by-char typing must use post-edit offsets"
    );
}

#[test]
fn references_track_line_removal_from_other_fn_edit() {
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, AFTER.to_string());
    assert_eq!(
        ref_starts(&bone, Position::new(6, 8)),
        vec![(6, 8), (7, 11), (7, 19)],
        "pre-edit baseline (longer file)"
    );

    bone.update(PATH, BEFORE.to_string());
    assert_eq!(
        ref_starts(&bone, Position::new(5, 8)),
        vec![(5, 8), (6, 11), (6, 19)],
        "post-removal references must shift back up"
    );
}

// ---------------------------------------------------------------------------
// Guard: incremental-vs-from-scratch navigation oracle.
//
// The pinned tests above assert hand-computed offsets for specific cursors, so
// they only cover span carriers someone thought to name. This asserts the
// general invariant: after an incremental edit, references / hover / definition
// at EVERY offset must match a from-scratch check of the same final text. Any
// unshifted span these read makes the two diverge -- it catches the whole class
// without enumerating which field. (bone-incremental's equivalence oracle
// compares only `type_at`, which reads the shifted `node_index`, so it was
// blind to inline node spans; this closes that gap for a future span add.)
//
// A consistent bug present in BOTH sides (e.g. the typed-for-binding empty-hover
// sentinel) keeps the two equal, so the oracle doesn't false-positive on it --
// it fires only when incremental and from-scratch actually disagree.
// ---------------------------------------------------------------------------

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

const ORACLE_STRUCT_BEFORE: &str = "\
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

const ORACLE_STRUCT_AFTER: &str = "\
fn helper() -> int {
    let pad = 0
    return 1 + pad
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

const ORACLE_GENERIC_BEFORE: &str = "\
fn helper() -> int {
    return 1
}

fn identity<T>(x: T) -> T {
    return x
}
";

const ORACLE_GENERIC_AFTER: &str = "\
fn helper() -> int {
    let pad = 0
    return 1 + pad
}

fn identity<T>(x: T) -> T {
    return x
}
";

#[test]
fn incremental_nav_matches_from_scratch_across_offsets() {
    // Each pair edits a body that precedes the symbols of interest, so those
    // symbols shift and their inline spans must be patched. Between them the
    // corpus exercises locals, call sites, enum variants, struct-literal and
    // field-access spans, and fn generic params.
    for (before, after) in [
        (BEFORE, AFTER),
        (CALL_BEFORE, CALL_AFTER),
        (ENUM_BEFORE, ENUM_AFTER),
        (ORACLE_STRUCT_BEFORE, ORACLE_STRUCT_AFTER),
        (ORACLE_GENERIC_BEFORE, ORACLE_GENERIC_AFTER),
    ] {
        assert_incremental_nav_matches_scratch(before, after);
    }
}
