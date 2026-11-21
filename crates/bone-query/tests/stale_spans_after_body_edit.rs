//! A body edit that shifts lines in one fn must not leave other fns' navigation spans stale.

use crate::fixture::{PATH, workspace};
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

// Navigation at every offset after an incremental edit must match a fresh check.

/// For every cursor position in `text`, the sorted non-empty `query` results there.
fn nav_profile<I>(
    bone: &BoneWorkspace,
    text: &str,
    query: impl Fn(&BoneWorkspace, Position) -> I,
) -> Vec<((u32, u32), Vec<String>)>
where
    I: IntoIterator,
    I::Item: std::fmt::Debug,
{
    let mut out = Vec::new();
    for (line, line_text) in text.lines().enumerate() {
        for ch in 0..=line_text.chars().count() {
            let pos = Position::new(line as u32, ch as u32);
            let mut r: Vec<_> = query(bone, pos)
                .into_iter()
                .map(|r| format!("{r:?}"))
                .collect();
            r.sort();
            if !r.is_empty() {
                out.push(((line as u32, ch as u32), r));
            }
        }
    }
    out
}

/// Compare references, hover and definition at every offset against a fresh workspace.
fn assert_incremental_nav_matches_scratch(before: &str, after: &str) {
    let mut inc = workspace(before);
    inc.update(PATH, after.to_string());
    let scratch = workspace(after);
    let refs = |b: &BoneWorkspace, p| b.references(PATH, p).into_iter().map(|l| l.range);
    let hover = |b: &BoneWorkspace, p| b.hover(PATH, p);
    let def = |b: &BoneWorkspace, p| b.definition(PATH, p);

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
