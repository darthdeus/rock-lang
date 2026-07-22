//! Completion inside a `// rock: ...` magic comment is a separate early-exit
//! context: typing `// rock: ` offers the `allow` verb, and inside
//! `// rock: allow(` the known lint names complete. Generic symbol/type
//! completion is short-circuited -- a comment is not code.

use bone_query::BoneWorkspace;
use lsp_types::Position;

const PATH: &str = "/tmp/rock_lsp_magic_comment.rock";

fn labels(src: &str, line: u32, ch: u32) -> Vec<String> {
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    bone.completion(PATH, Position::new(line, ch)).into_iter().map(|c| c.label).collect()
}

#[test]
fn rock_directive_offers_allow_verb() {
    // Cursor right after `// rock: ` -- offer the directive verb.
    let src = "fn main() {\n    // rock: \n    let unused_thing = 5\n}\n";
    let out = labels(src, 1, 13);
    assert!(out.iter().any(|l| l == "allow"), "expected `allow` verb, got {out:?}");
}

#[test]
fn inside_allow_offers_lint_names() {
    // Cursor right after `// rock: allow(` -- offer the known lint names.
    let src = "fn main() {\n    // rock: allow(\n    let unused_thing = 5\n}\n";
    let out = labels(src, 1, 19);
    assert!(out.iter().any(|l| l == "unused_var"), "expected `unused_var`, got {out:?}");
    assert!(out.iter().any(|l| l == "unused_fn"), "expected `unused_fn`, got {out:?}");
}

#[test]
fn inside_allow_filters_by_typed_prefix() {
    // `// rock: allow(unused_v` -- only names matching the typed prefix.
    let src = "fn main() {\n    // rock: allow(unused_v\n    let unused_thing = 5\n}\n";
    let out = labels(src, 1, 27);
    assert!(out.iter().any(|l| l == "unused_var"), "expected `unused_var`, got {out:?}");
    assert!(!out.iter().any(|l| l == "unused_fn"), "`unused_fn` should be filtered out, got {out:?}");
}

#[test]
fn magic_comment_suppresses_generic_symbol_completion() {
    // A helper fn `unused_helper` is in scope; inside the magic comment the
    // menu must NOT offer it (a comment is not code).
    let src = "fn unused_helper() {}\nfn main() {\n    // rock: allow(un\n}\n";
    let out = labels(src, 2, 21);
    assert!(!out.iter().any(|l| l == "unused_helper"), "code symbol leaked into magic comment: {out:?}");
    assert!(out.iter().any(|l| l == "unused_var"), "expected lint name, got {out:?}");
}
