//! A body edit that keeps the line count (here: a longer literal inside
//! `alpha`) moves later byte offsets but not later columns. Cached lint
//! findings in other bodies must keep their columns.

use bone_query::BoneWorkspace;

#[cfg(not(windows))]
const PATH: &str = "/tmp/rock_lsp_bone_lint_columns.rock";
#[cfg(windows)]
const PATH: &str = r"C:\tmp\rock_lsp_bone_lint_columns.rock";

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

const AFTER: &str = "\
fn alpha() -> int {
    return 1000000
}

fn beta() -> int {
    let unused_here = 2
    return 3
}

fn main() {
    print(alpha() + beta())
}
";

fn unused_var_ranges(bone: &BoneWorkspace) -> Vec<(u32, u32, u32, u32)> {
    let mut v: Vec<_> = bone
        .lint_diagnostics(PATH)
        .into_iter()
        .filter(|d| d.message.contains("unused_here"))
        .map(|d| {
            (
                d.range.start.line,
                d.range.start.character,
                d.range.end.line,
                d.range.end.character,
            )
        })
        .collect();
    v.sort();
    v
}

#[test]
fn lint_columns_survive_same_line_count_body_edit() {
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, BEFORE.to_string());
    let before = unused_var_ranges(&bone);
    assert!(!before.is_empty(), "baseline must report unused_here");

    bone.update(PATH, AFTER.to_string());
    let after = unused_var_ranges(&bone);

    let mut fresh = BoneWorkspace::new();
    fresh.update(PATH, AFTER.to_string());
    let expected = unused_var_ranges(&fresh);

    assert_eq!(before, expected, "the edit does not move beta's text");
    assert_eq!(
        after, expected,
        "incremental lint range must match a fresh load"
    );
}
