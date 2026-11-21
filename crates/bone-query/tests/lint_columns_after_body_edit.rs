//! A same-line-count body edit shifts bytes, not columns; cached lints elsewhere keep columns.

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
