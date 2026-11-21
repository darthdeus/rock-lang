//! LSP positions over multiline tokens, multibyte text, UFCS calls and generic parameters.

use crate::fixture::{PATH, pos as at, workspace};
use bone_query::BoneWorkspace;
use lsp_types::{Location, Position};

/// What the range covers in `src`, with its line and start column.
fn text(src: &str, location: &Location) -> (u32, u32, String) {
    let range = location.range;
    assert_eq!(range.start.line, range.end.line);
    let spelled = src
        .lines()
        .nth(range.start.line as usize)
        .unwrap_or_default()
        .chars()
        .skip(range.start.character as usize)
        .take((range.end.character - range.start.character) as usize)
        .collect();
    (range.start.line, range.start.character, spelled)
}

fn renamed_sites(bone: &BoneWorkspace, pos: Position) -> Vec<(u32, u32)> {
    let edit = bone
        .rename(PATH, pos, "renamed")
        .expect("rename is allowed")
        .expect("rename finds sites");
    let mut sites: Vec<_> = edit
        .changes
        .expect("rename returns changes")
        .into_values()
        .flatten()
        .map(|e| (e.range.start.line, e.range.start.character))
        .collect();
    sites.sort();
    sites
}

#[test]
fn goto_definition_on_a_generic_parameter_reaches_its_declaration() {
    let src =
        "fn f<T>(a: T, b: T?) -> [T?] { [b] }\nstruct S<U> { v: U, w: U? }\nenum E<V> { A(V) }\n";
    let bone = workspace(src);
    let expect = [
        ("a: T", 3, (0, 5, "T")),
        ("b: T", 3, (0, 5, "T")),
        ("[T?", 1, (0, 5, "T")),
        ("v: U", 3, (1, 9, "U")),
        ("w: U", 3, (1, 9, "U")),
        ("A(V", 2, (2, 7, "V")),
    ];
    for (needle, extra, (line, col, name)) in expect {
        let found = bone
            .definition(PATH, at(src, needle, extra))
            .unwrap_or_else(|| panic!("no definition for {needle}"));
        assert_eq!(text(src, &found), (line, col, name.to_string()), "{needle}");
    }
}

#[test]
fn renaming_a_generic_parameter_reaches_its_nullable_uses() {
    let src = "fn f<T>(a: T, b: T?) -> [T?] { [b] }\n";
    let bone = workspace(src);
    let all = [(0, 5), (0, 11), (0, 17), (0, 25)];
    for (needle, extra) in [("<T>", 1), ("a: T", 3), ("b: T", 3), ("[T?", 1)] {
        assert_eq!(
            renamed_sites(&bone, at(src, needle, extra)),
            all,
            "{needle}"
        );
    }
}
