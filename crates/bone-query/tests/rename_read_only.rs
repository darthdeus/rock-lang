//! Rename only ever edits sources the user owns: a name shared with stdlib
//! overloads stays out of the stdlib, and a reference rename cannot pin to one
//! side refuses the rename instead of guessing.

use bone_query::BoneWorkspace;
use lsp_types::Position;

#[cfg(not(windows))]
const PATH: &str = "/tmp/rock_rename_read_only.rock";
#[cfg(windows)]
const PATH: &str = r"C:\scratch\rock_rename_read_only.rock";

fn workspace(src: &str) -> BoneWorkspace {
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    bone
}

/// Position `extra` bytes into the first occurrence of `needle` (ASCII sources).
fn at(src: &str, needle: &str, extra: usize) -> Position {
    let idx = src.find(needle).expect(needle) + extra;
    let before = &src[..idx];
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    Position::new(
        before.matches('\n').count() as u32,
        (idx - line_start) as u32,
    )
}

/// The refusal for a rename at `pos`, checked to agree with prepare-rename.
fn refusal(bone: &BoneWorkspace, pos: Position) -> String {
    let renamed = bone.rename(PATH, pos, "fetch").expect_err("refused");
    let prepared = bone.prepare_rename(PATH, pos).expect_err("refused");
    assert_eq!(renamed, prepared, "prepare and rename refuse alike");
    renamed.0
}

/// Every edited `(line, col)`, after checking no edit left the user's file.
fn edited_sites(bone: &BoneWorkspace, pos: Position) -> Vec<(u32, u32)> {
    let edit = bone
        .rename(PATH, pos, "fetch")
        .expect("rename is allowed")
        .expect("rename finds sites");
    let by_file: Vec<_> = edit
        .changes
        .expect("rename returns changes")
        .into_iter()
        .collect();
    assert_eq!(
        by_file.len(),
        1,
        "only the user's file changes: {by_file:?}"
    );
    let mut sites: Vec<_> = by_file
        .into_iter()
        .flat_map(|(_, edits)| edits)
        .map(|e| (e.range.start.line, e.range.start.character))
        .collect();
    sites.sort();
    sites
}

const USER_GET: &str = "\
struct Box { v: int }
fn Box.get(b) -> int { b.v }
fn main() {
  let b = Box { v: 1 }
  print(b.get())
}
";

#[test]
fn renaming_a_user_function_named_like_stdlib_overloads_stays_in_the_user_file() {
    let bone = workspace(USER_GET);
    let expected = vec![(1, 7), (4, 10)];
    assert_eq!(edited_sites(&bone, at(USER_GET, "Box.get", 4)), expected);
    assert_eq!(edited_sites(&bone, at(USER_GET, "b.get", 2)), expected);
}

#[test]
fn a_call_resolved_to_a_stdlib_overload_keeps_its_name() {
    let src = "\
struct Box { v: int }
fn Box.get(b) -> int { b.v }
fn main() {
  let b = Box { v: 1 }
  let a = arena_new<int>()
  let e = a.insert(2)
  print(b.get())
  print(a.get(e))
}
";
    let bone = workspace(src);
    assert_eq!(
        edited_sites(&bone, at(src, "Box.get", 4)),
        vec![(1, 7), (6, 10)]
    );
}

#[test]
fn renaming_a_stdlib_function_is_refused() {
    let src = "\
fn main() {
  let a = arena_new<int>()
  let e = a.insert(2)
  print(a.get(e))
}
";
    let bone = workspace(src);
    let why = refusal(&bone, at(src, "a.get", 2));
    assert!(why.contains("standard library"), "{why}");
}

#[test]
fn a_function_value_that_could_name_either_side_refuses_the_rename() {
    let src = "\
struct Box { v: int }
fn get(b: Box) -> int { b.v }
fn main() {
  let f = get
  print(f(Box { v: 1 }))
}
";
    let bone = workspace(src);
    for pos in [at(src, "fn get", 3), at(src, "= get", 2)] {
        let why = refusal(&bone, pos);
        assert!(why.contains("cannot rename `get`"), "{why}");
        assert!(why.contains("standard library"), "{why}");
    }
}

#[test]
fn a_name_only_the_user_declares_still_renames_function_values() {
    let src = "\
fn twice_it(x: int) -> int { x * 2 }
fn main() {
  let f = twice_it
  print(f(1) + twice_it(2))
}
";
    let bone = workspace(src);
    assert_eq!(
        edited_sites(&bone, at(src, "fn twice_it", 3)),
        vec![(0, 3), (2, 10), (3, 15)]
    );
}

#[test]
fn renaming_a_stdlib_type_is_refused() {
    let src = "\
fn f(a: Arena<int>) -> int {
  a.len()
}
";
    let bone = workspace(src);
    let why = refusal(&bone, at(src, "Arena", 1));
    assert!(why.contains("standard library"), "{why}");
}

#[test]
fn an_unresolved_call_that_could_name_either_side_refuses_the_rename() {
    let src = "\
struct Box { v: int }
fn get(b: Box) -> int { b.v }
fn main() {
  get(1, 2, 3, 4)
}
";
    let bone = workspace(src);
    let why = refusal(&bone, at(src, "fn get", 3));
    assert!(why.contains("rename cannot tell which"), "{why}");
}
