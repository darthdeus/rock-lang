//! `bone_query::query::definition_at` and `references` over a checked
//! single-module workspace, by byte offset.

use bone_query::query::{FnRefScope, definition_at, references};
use bone_semantic::prelude::*;
use bone_semantic::semantic_passes::semantic_pass;

/// Build a checked workspace from a single source string and return it plus the
/// (sole) user module id.
fn ws_of(src: &str) -> (Workspace, ModuleId) {
    let module =
        skull_parser::parse_module_clean(src, Ustr::from("<query-test>")).expect("parse_rock");
    let ws = semantic_pass(&mut [module]).expect("semantic_pass");
    let mid = ws.source_modules().next().expect("one user module").id;
    (ws, mid)
}

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
        let (ws, mid) = ws_of(src);
        let inf = ws.inference(mid).expect("user module inferred");
        let use_off = nth_offset(src, use_needle, 1) + use_into;
        let decl_off = nth_offset(src, decl_needle, 1) + decl_into;
        let def = definition_at(&ws, mid, inf, use_off)
            .unwrap_or_else(|| panic!("{case}: no definition at {use_off}"));
        let (s, e) = def.offset_range();
        assert!(
            s <= decl_off && decl_off < e,
            "{case}: {s}..{e} misses {decl_off}"
        );
        if case == "function call" {
            assert_eq!(e - s, "helper".len(), "{case}: the bare name, not the item");
        }
    }
}

/// References include the declaration, so find-references and rename share
/// them, and every span is exactly the identifier so rename is safe.
#[test]
fn reference_cases() {
    // (case, source, name, cursor occurrence, referenced occurrences of `name`)
    let cases: &[(&str, &str, &str, usize, &[usize])] = &[
        (
            "local",
            "fn f() -> int {\n    let count = 1\n    let total = count + count\n    total\n}",
            "count",
            2,
            &[1, 2, 3],
        ),
        (
            "parameter",
            "fn f(num: int) -> int {\n    num + num\n}",
            "num",
            2,
            &[1, 2, 3],
        ),
        // Plain calls: an operator lowers to `__op_add`, which does not
        // resolve without the stdlib and would drop `b` from inference.
        (
            "function from a call",
            "fn helper() -> int { 1 }\nfn a() -> int { helper() }\nfn b() -> int {\n    let x = helper()\n    helper()\n}",
            "helper",
            2,
            &[1, 2, 3, 4],
        ),
        (
            "function from its declaration",
            "fn helper() -> int { 1 }\nfn a() -> int { helper() }",
            "helper",
            1,
            &[1, 2],
        ),
        // The UFCS method name is not at the call span's start.
        (
            "UFCS call",
            "fn inc(x: int) -> int { x + 1 }\nfn main() {\n    let a = inc(5)\n    let b = 5.inc()\n}",
            "inc",
            2,
            &[1, 2, 3],
        ),
        // A fn-typed param called as `g()` has no separate Var node.
        (
            "local called as a function",
            "fn apply(g: fn() -> int) -> int {\n    g() + g()\n}",
            "g",
            2,
            &[1, 2, 3],
        ),
    ];
    for (case, src, name, cursor, occurrences) in cases {
        let (ws, mid) = ws_of(src);
        let inf = ws.inference(mid).expect("user module inferred");
        let cursor = nth_offset(src, name, *cursor);
        let mut got: Vec<(usize, usize)> = references(
            &ws,
            mid,
            inf,
            cursor,
            &|_| Some(src),
            FnRefScope::AllOverloads,
        )
        .iter()
        .map(|s| s.span.offset_range())
        .collect();
        got.sort();
        let want: Vec<(usize, usize)> = occurrences
            .iter()
            .map(|&n| nth_offset(src, name, n))
            .map(|off| (off, off + name.len()))
            .collect();
        assert_eq!(got, want, "{case}");
    }
}
