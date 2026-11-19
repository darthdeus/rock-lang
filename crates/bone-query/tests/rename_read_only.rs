//! Rename edits only user-owned sources, and refuses when a reference rename cannot pin one side.

use crate::fixture::{PATH, pos as at, workspace};
use bone_query::BoneWorkspace;
use lsp_types::Position;

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

const BOX_GET: &str =
    "struct Box { v: int }\nfn Box.get(b) -> int { b.v }\nfn main() {\n  let b = Box { v: 1 }\n";
const BOX_FN: &str = "struct Box { v: int }\nfn get(b: Box) -> int { b.v }\nfn main() {\n";
const ARENA: &str = "  let a = arena_new<int>()\n  let e = a.insert(2)\n";

#[test]
fn renames_stay_in_the_user_file() {
    // (case, source, cursors as (needle, offset), edited sites)
    let cases: &[(&str, String, &[(&str, usize)], &[(u32, u32)])] = &[
        (
            "user method named like stdlib overloads",
            format!("{BOX_GET}  print(b.get())\n}}\n"),
            &[("Box.get", 4), ("b.get", 2)],
            &[(1, 7), (4, 10)],
        ),
        (
            "a call resolved to a stdlib overload keeps its name",
            format!("{BOX_GET}{ARENA}  print(b.get())\n  print(a.get(e))\n}}\n"),
            &[("Box.get", 4)],
            &[(1, 7), (6, 10)],
        ),
        (
            "function values of a name only the user declares",
            "fn twice_it(x: int) -> int { x * 2 }\nfn main() {\n  let f = twice_it\n  print(f(1) + twice_it(2))\n}\n".to_string(),
            &[("fn twice_it", 3)],
            &[(0, 3), (2, 10), (3, 15)],
        ),
        (
            "a function value binds the user definition by canonical identity",
            format!("{BOX_FN}  let f = get\n  print(f(Box {{ v: 1 }}))\n}}\n"),
            &[("fn get", 3), ("= get", 2)],
            &[(1, 3), (3, 10)],
        ),
    ];
    for (case, src, cursors, sites) in cases {
        let bone = workspace(src);
        for (needle, extra) in *cursors {
            assert_eq!(
                edited_sites(&bone, at(src, needle, *extra)),
                *sites,
                "{case}: {needle}"
            );
        }
    }
}

#[test]
fn renames_that_would_touch_or_guess_at_the_stdlib_are_refused() {
    // (case, source, cursors as (needle, offset), refusal fragments)
    let cases: &[(&str, String, &[(&str, usize)], &[&str])] = &[
        (
            "stdlib function",
            format!("fn main() {{\n{ARENA}  print(a.get(e))\n}}\n"),
            &[("a.get", 2)],
            &["standard library"],
        ),
        (
            "stdlib type",
            "fn f(a: Arena<int>) -> int {\n  a.len()\n}\n".to_string(),
            &[("Arena", 1)],
            &["standard library"],
        ),
        (
            "an unresolved call that could name either side",
            format!("{BOX_FN}  get(1, 2, 3, 4)\n}}\n"),
            &[("fn get", 3)],
            &["rename cannot tell which"],
        ),
    ];
    for (case, src, cursors, fragments) in cases {
        let bone = workspace(src);
        for (needle, extra) in *cursors {
            let why = refusal(&bone, at(src, needle, *extra));
            for f in *fragments {
                assert!(why.contains(f), "{case}: {needle}: {why}");
            }
        }
    }
}
