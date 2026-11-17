//! Definition and reference ranges through the LSP workspace facade.

use crate::fixture::{PATH, offset_of, workspace};

/// Byte offset of the `occurrence`-th (1-based) appearance of `needle` in `src`.
fn nth_offset(src: &str, needle: &str, occurrence: usize) -> usize {
    src.match_indices(needle)
        .nth(occurrence - 1)
        .unwrap_or_else(|| panic!("occurrence {occurrence} of {needle:?} not found"))
        .0
}

#[test]
fn definition_cases() {
    // The definition span must contain the declaration name; it may be the
    // bare name or the whole item.
    // (case, source, use needle, offset into it, declaration needle, offset into it)
    let cases = [
        (
            "local",
            "fn f() -> int {\n    let x = 1\n    return x\n}",
            "return x",
            7,
            "let x",
            4,
        ),
        (
            "parameter",
            "fn f(n: int) -> int {\n    return n\n}",
            "return n",
            7,
            "f(n",
            2,
        ),
        (
            "function call",
            "fn helper() -> int {\n    1\n}\nfn main() {\n    let r = helper()\n}",
            "= helper()",
            2,
            "fn helper",
            3,
        ),
        (
            "struct literal",
            "struct Point { x: int }\nfn f() -> Point {\n    Point { x: 1 }\n}",
            "Point { x: 1",
            0,
            "struct Point",
            7,
        ),
        (
            "enum qualifier",
            "enum Color { Red, Green }\nfn f() -> Color {\n    Color::Red\n}",
            "Color::Red",
            0,
            "enum Color",
            5,
        ),
        (
            "global",
            "global G: int = 5\nfn f() -> int {\n    return G\n}",
            "return G",
            7,
            "global G",
            7,
        ),
        // Like a `mob.attack_co` access, a FieldRef key path names the field.
        (
            "field ref",
            "struct Mob { health: int, attack_co: int? }\nfn kill_co<T>(o: T, f: FieldRef<T, int?>) {}\nfn r(mob: Mob) {\n    kill_co(mob, .attack_co)\n}\n",
            ".attack_co",
            1,
            "attack_co: int?",
            0,
        ),
    ];
    for (case, src, use_needle, use_into, decl_needle, decl_into) in cases {
        let at = crate::fixture::pos(src, use_needle, use_into);
        let decl_off = nth_offset(src, decl_needle, 1) + decl_into;
        let def = workspace(src)
            .definition(PATH, at)
            .unwrap_or_else(|| panic!("{case}: no definition at {at:?}"));
        assert_eq!(def.uri.to_file_path().unwrap(), PATH);
        let (s, e) = (
            offset_of(src, def.range.start),
            offset_of(src, def.range.end),
        );
        assert!(
            s <= decl_off && decl_off < e,
            "{case}: {s}..{e} misses {decl_off}"
        );
        if case == "function call" {
            assert_eq!(e - s, "helper".len(), "{case}: the bare name, not the item");
        }
    }
}

#[test]
fn direct_enum_path_definition() {
    use bone_semantic::prelude::*;

    let src = "enum Color { Red, Green }\nfn f() -> Color { Color::Red }";
    let ast = skull_parser::parse_module_clean(src, Ustr::from(PATH)).unwrap();
    let ws = semantic_pass(&mut [ast]).unwrap();
    let mid = ws.source_modules().next().unwrap().id;
    let span = bone_query::query::definition_at(
        &ws,
        mid,
        ws.inference(mid).unwrap(),
        src.find("Color::Red").unwrap(),
    )
    .expect("enum qualifier resolves through its recorded path segment");
    let (start, end) = span.offset_range();
    assert!(start <= 5 && 5 < end, "enum declaration: {start}..{end}");
}

/// Corpus fixtures pin reference starts; keep the query-level span-width
/// invariant here because the corpus currently checks starts only.
#[test]
fn reference_ranges_cover_only_names() {
    // (case, source, name, cursor occurrence)
    let cases: &[(&str, &str, &str, usize)] = &[
        (
            "local",
            "fn f() -> int {\n    let count = 1\n    let total = count + count\n    total\n}",
            "count",
            2,
        ),
        (
            "parameter",
            "fn f(num: int) -> int {\n    num + num\n}",
            "num",
            2,
        ),
        (
            "function from a call",
            "fn helper() -> int { 1 }\nfn a() -> int { helper() }\nfn b() -> int {\n    let x = helper()\n    helper()\n}",
            "helper",
            2,
        ),
        (
            "function from its declaration",
            "fn helper() -> int { 1 }\nfn a() -> int { helper() }",
            "helper",
            1,
        ),
        // The UFCS method name is not at the call span's start.
        (
            "UFCS call",
            "fn inc(x: int) -> int { x + 1 }\nfn main() {\n    let a = inc(5)\n    let b = 5.inc()\n}",
            "inc",
            3,
        ),
        // A fn-typed param called as `g()` has no separate Var node.
        (
            "local called as a function",
            "fn apply(g: fn() -> int) -> int {\n    g() + g()\n}",
            "g",
            2,
        ),
    ];
    for (case, src, name, cursor) in cases {
        let cursor = crate::fixture::position_of_offset(src, nth_offset(src, name, *cursor));
        let refs = workspace(src).references(PATH, cursor);
        assert!(!refs.is_empty(), "{case}: no references");
        for loc in refs {
            assert_eq!(loc.uri.to_file_path().unwrap(), PATH);
            let (start, end) = (
                offset_of(src, loc.range.start),
                offset_of(src, loc.range.end),
            );
            assert_eq!(end - start, name.len(), "{case}: {start}..{end}");
        }
    }
}
