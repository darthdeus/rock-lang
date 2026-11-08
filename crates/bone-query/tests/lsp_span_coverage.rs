//! LSP positions over the shapes that have gone wrong before: tokens spanning
//! lines, multibyte text ahead of a name, UFCS calls and generic parameters.

use bone_query::BoneWorkspace;
use lsp_types::{Location, Position, SemanticTokens};

const PATH: &str = "/tmp/rock_lsp_span_coverage.rock";

fn workspace(src: &str) -> BoneWorkspace {
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    bone
}

/// Position `extra` bytes into the first occurrence of `needle`, with the
/// column counted in scalar values independently of the code under test.
fn at(src: &str, needle: &str, extra: usize) -> Position {
    let idx = src.find(needle).expect(needle) + extra;
    let before = &src[..idx];
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    Position::new(
        before.matches('\n').count() as u32,
        src[line_start..idx].chars().count() as u32,
    )
}

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

fn references(bone: &BoneWorkspace, src: &str, pos: Position) -> Vec<(u32, u32, String)> {
    let mut sites: Vec<_> = bone
        .references(PATH, pos)
        .iter()
        .map(|l| text(src, l))
        .collect();
    sites.sort();
    sites
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

/// Tokens as absolute `(line, column, length)`.
fn absolute(tokens: &SemanticTokens) -> Vec<(u32, u32, u32)> {
    let (mut line, mut col) = (0, 0);
    tokens
        .data
        .iter()
        .map(|t| {
            if t.delta_line != 0 {
                line += t.delta_line;
                col = t.delta_start;
            } else {
                col += t.delta_start;
            }
            (line, col, t.length)
        })
        .collect()
}

#[test]
fn a_multi_line_comment_gets_one_token_per_line() {
    let src = "/* \u{e9}a\n  bc */ fn f() {\n}\n";
    let tokens = absolute(&workspace(src).semantic_tokens(PATH));
    assert_eq!(
        tokens[..3],
        [(0, 0, 5), (1, 0, 7), (1, 8, 2)],
        "comment lines, then the `fn` after it: {tokens:?}"
    );
}

#[test]
fn multi_line_strings_get_one_token_per_line() {
    let src = "fn main() {\n    let m = r\"\"\"\u{e9}\n  z\"\"\" // t\n    print(m)\n}\n";
    let tokens = absolute(&workspace(src).semantic_tokens(PATH));
    assert!(tokens.contains(&(1, 12, 5)), "first line: {tokens:?}");
    assert!(tokens.contains(&(2, 0, 6)), "last line: {tokens:?}");
    assert!(tokens.contains(&(2, 7, 4)), "comment after it: {tokens:?}");
}

#[test]
fn a_range_request_covers_the_visible_lines_of_a_multi_line_comment() {
    let src = "/* one\n   two\n   three */\nfn f() {\n}\n";
    let bone = workspace(src);
    let range = lsp_types::Range::new(Position::new(1, 0), Position::new(2, 0));
    let tokens = absolute(&bone.semantic_tokens_range(PATH, range));
    assert_eq!(tokens, [(1, 0, 6)]);
}

#[test]
fn holes_in_a_multi_line_string_keep_their_own_tokens() {
    let src = "fn main() {\n    let a = 1\n    let s = \"\"\"x {a}\n{a} y\"\"\"\n    print(s)\n}\n";
    let tokens = workspace(src).semantic_tokens(PATH);
    let abs = absolute(&tokens);
    // `a` on each line of the string, each a token of its own.
    assert!(abs.contains(&(2, 18, 1)), "first hole: {abs:?}");
    assert!(abs.contains(&(3, 1, 1)), "second hole: {abs:?}");
}

#[test]
fn multibyte_text_before_a_decorator_keeps_navigation_columns() {
    let src = "deco trace(x: int) {\n}\n/* \u{e9}\u{e9}\u{e9} */ @trace(1) fn f() {\n}\n/* \u{4e2d}\u{6587} */ @trace(2)\nfn g() {\n}\n";
    let bone = workspace(src);
    let expected = vec![
        (0, 5, "trace".to_string()),
        (2, 11, "trace".to_string()),
        (4, 10, "trace".to_string()),
    ];
    assert_eq!(references(&bone, src, at(src, "@trace(1)", 1)), expected);
    assert_eq!(references(&bone, src, at(src, "@trace(2)", 1)), expected);
    assert_eq!(references(&bone, src, at(src, "deco trace", 5)), expected);
}

#[test]
fn ufcs_calls_navigate_and_rename_with_the_function() {
    let src = "struct Mob { hp: int }\nfn hit(m: Mob, n: int) -> int { m.hp + n }\nfn Mob.heal(m, n: int) -> int { m.hp + n }\nfn main() {\n    let m = Mob { hp: 1 }\n    print(m.hit(1), hit(m, 2), m.heal(3), heal(m, 4))\n}\n";
    let bone = workspace(src);
    let decl = |needle: &str, extra| bone.definition(PATH, at(src, needle, extra)).unwrap();
    assert_eq!(text(src, &decl("m.hit", 2)), (1, 3, "hit".to_string()));
    assert_eq!(text(src, &decl("hit(m, 2)", 0)), (1, 3, "hit".to_string()));
    assert_eq!(text(src, &decl("m.heal", 2)), (2, 7, "heal".to_string()));
    assert_eq!(
        renamed_sites(&bone, at(src, "m.hit", 2)),
        [(1, 3), (5, 12), (5, 20)]
    );
    assert_eq!(
        renamed_sites(&bone, at(src, "fn Mob.heal", 7)),
        [(2, 7), (5, 33), (5, 42)]
    );
}

#[test]
fn generic_calls_navigate_and_rename_with_the_function() {
    let src = "fn id<T>(x: T) -> T { x }\nfn main() {\n    print(id<int>(1), id(2), 3.id())\n}\n";
    let bone = workspace(src);
    for needle in ["id<int>", "id(2)"] {
        let found = bone.definition(PATH, at(src, needle, 0)).unwrap();
        assert_eq!(text(src, &found), (0, 3, "id".to_string()));
    }
    assert_eq!(
        renamed_sites(&bone, at(src, "id(2)", 0)),
        [(0, 3), (2, 10), (2, 22), (2, 31)]
    );
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

#[test]
fn renaming_a_struct_or_enum_generic_from_a_use_reaches_every_use() {
    let src =
        "struct S<T> { v: T, w: [T], x: T?, y: T? = null }\nenum E<Z> { A(Z), B { q: [Z] } }\n";
    let bone = workspace(src);
    let fields = [(0, 9), (0, 17), (0, 24), (0, 31), (0, 38)];
    for (needle, extra) in [("v: T", 3), ("x: T", 3), ("y: T", 3)] {
        assert_eq!(
            renamed_sites(&bone, at(src, needle, extra)),
            fields,
            "{needle}"
        );
    }
    let variants = [(1, 7), (1, 14), (1, 26)];
    for (needle, extra) in [("A(Z", 2), ("[Z", 1)] {
        assert_eq!(
            renamed_sites(&bone, at(src, needle, extra)),
            variants,
            "{needle}"
        );
    }
}
