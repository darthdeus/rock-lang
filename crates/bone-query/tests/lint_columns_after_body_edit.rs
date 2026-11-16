//! A body edit that keeps the line count (here: a longer literal inside
//! `alpha`) moves later byte offsets but not later columns. Cached lint
//! findings in other bodies must keep their columns.

use crate::fixture::{PATH, workspace};
use bone_query::BoneWorkspace;

const BEFORE: &str = "\
fn alpha() -> int {
    return 1
}

fn beta() -> int {
    let unused_here = 2
    return 3
}

fn main() {
    print(alpha() + beta())
}
";

fn unused_var_ranges(bone: &BoneWorkspace) -> Vec<lsp_types::Range> {
    bone.lint_diagnostics(PATH)
        .into_iter()
        .filter(|d| d.message.contains("unused_here"))
        .map(|d| d.range)
        .collect()
}

#[test]
fn lint_columns_survive_same_line_count_body_edit() {
    let after = BEFORE.replacen("return 1\n", "return 1000000\n", 1);
    let mut bone = workspace(BEFORE);
    let before = unused_var_ranges(&bone);
    assert!(!before.is_empty(), "baseline must report unused_here");
    bone.update(PATH, after.clone());
    let expected = unused_var_ranges(&workspace(&after));
    assert_eq!(before, expected, "the edit does not move beta's text");
    assert_eq!(
        unused_var_ranges(&bone),
        expected,
        "incremental matches fresh"
    );
}
