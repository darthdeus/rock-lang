//! Test-drive for `bone_query::query::definition_at` / `references` -- the
//! goto-definition + find-references primitives the LSP needs (see
//! `docs/lsp-bone-migration-plan.md`). Run: `cargo test -p bone-query --test query`.
//!
//! Each test resolves a *use site* offset and asserts the returned declaration
//! span contains the *declaration name* -- robust to whether the span is the
//! bare name or the whole item. `query` lives in this crate over bone's public
//! HIR; queries that need new bone internals get proposed as a bone hint.

use bone_semantic::prelude::*;
use bone_query::query::{definition_at, references, FnRefScope};
use bone_semantic::semantic_passes::semantic_pass;

/// Build a checked workspace from a single source string and return it plus the
/// (sole) user module id.
fn ws_of(src: &str) -> (Workspace, ModuleId) {
    let module = skull_parser::parse_module_clean(src, Ustr::from("<query-test>")).expect("parse_rock");
    let ws = semantic_pass(&mut [module]).expect("semantic_pass");
    let mid = ws
        .modules
        .iter()
        .find(|m| m.id != ModuleId::builtin())
        .expect("one user module")
        .id;
    (ws, mid)
}

/// Byte offset of the `occurrence`-th (1-based) appearance of `needle` in `src`.
fn nth_offset(src: &str, needle: &str, occurrence: usize) -> usize {
    let mut idx = 0usize;
    let mut found = 0usize;
    loop {
        let rel = src[idx..]
            .find(needle)
            .unwrap_or_else(|| panic!("occurrence {occurrence} of {needle:?} not found"));
        let at = idx + rel;
        found += 1;
        if found == occurrence {
            return at;
        }
        idx = at + 1;
    }
}

/// Resolve `use_off` and assert the returned definition span contains `decl_off`.
fn assert_defines(src: &str, use_off: usize, decl_off: usize) {
    let (ws, mid) = ws_of(src);
    let inf = ws.inference(mid).expect("user module inferred");
    let def = definition_at(&ws, mid, inf, use_off)
        .unwrap_or_else(|| panic!("definition_at returned None for offset {use_off}"));
    let (s, e) = def.offset_range;
    assert!(
        s <= decl_off && decl_off < e,
        "definition span {:?} should contain the declaration at byte {} \
         (use site at {})",
        def.offset_range,
        decl_off,
        use_off
    );
}

#[test]
fn definition_of_local_variable() {
    let src = "fn f() -> int {\n    let x = 1\n    return x\n}";
    let use_off = nth_offset(src, "return x", 1) + "return ".len();
    let decl_off = nth_offset(src, "let x", 1) + "let ".len();
    assert_defines(src, use_off, decl_off);
}

#[test]
fn definition_of_parameter() {
    let src = "fn f(n: int) -> int {\n    return n\n}";
    let use_off = nth_offset(src, "return n", 1) + "return ".len();
    let decl_off = nth_offset(src, "f(n", 1) + "f(".len();
    assert_defines(src, use_off, decl_off);
}

#[test]
fn definition_of_function_call() {
    let src = "fn helper() -> int {\n    1\n}\nfn main() {\n    let r = helper()\n}";
    let use_off = nth_offset(src, "= helper()", 1) + "= ".len();
    let decl_off = nth_offset(src, "fn helper", 1) + "fn ".len();
    assert_defines(src, use_off, decl_off);
}

#[test]
fn definition_of_struct_literal() {
    let src = "struct Point { x: int }\nfn f() -> Point {\n    Point { x: 1 }\n}";
    let use_off = nth_offset(src, "Point { x: 1", 1);
    let decl_off = nth_offset(src, "struct Point", 1) + "struct ".len();
    assert_defines(src, use_off, decl_off);
}

#[test]
fn definition_of_enum_variant() {
    let src = "enum Color { Red, Green }\nfn f() -> Color {\n    Color::Red\n}";
    let use_off = nth_offset(src, "Color::Red", 1);
    let decl_off = nth_offset(src, "enum Color", 1) + "enum ".len();
    assert_defines(src, use_off, decl_off);
}

#[test]
fn definition_of_global() {
    let src = "global G: int = 5\nfn f() -> int {\n    return G\n}";
    let use_off = nth_offset(src, "return G", 1) + "return ".len();
    let decl_off = nth_offset(src, "global G", 1) + "global ".len();
    assert_defines(src, use_off, decl_off);
}

#[test]
fn definition_of_field_ref_resolves_to_the_struct_field() {
    // `.attack_co` in a `FieldRef<Mob, int?>` slot -> the `attack_co` field decl
    // on `Mob` (mirrors `mob.attack_co` field access, which already resolves).
    let src = "struct Mob { health: int, attack_co: int? }\n\
               fn kill_co<T>(o: T, f: FieldRef<T, int?>) {}\n\
               fn r(mob: Mob) {\n    kill_co(mob, .attack_co)\n}\n";
    let use_off = nth_offset(src, ".attack_co", 1) + ".".len();
    let decl_off = nth_offset(src, "attack_co: int?", 1);
    assert_defines(src, use_off, decl_off);
}

// ---------------------------------------------------------------------------
// references: every use site of the symbol under the cursor, INCLUDING the
// declaration (so find-references and rename share it). Covers locals/params,
// functions, structs + fields, enums + variants, and fn generic params --
// all name-precise (the span is the identifier, not the whole item), so
// rename is safe. Globals aren't covered yet.
// ---------------------------------------------------------------------------

/// Resolve `cursor_off`; assert the returned reference set covers exactly the
/// expected declaration+use offsets (order-independent).
fn assert_refs(src: &str, cursor_off: usize, expected: &[usize]) {
    let (ws, mid) = ws_of(src);
    let inf = ws.inference(mid).expect("user module inferred");
    let spans = references(&ws, mid, inf, cursor_off, &|_| Some(src), FnRefScope::AllOverloads);
    assert_eq!(
        spans.len(),
        expected.len(),
        "reference count: got {:?}, expected offsets {:?}",
        spans.iter().map(|s| s.span.offset_range).collect::<Vec<_>>(),
        expected
    );
    for &off in expected {
        assert!(
            spans
                .iter()
                .any(|s| s.span.offset_range.0 <= off && off < s.span.offset_range.1),
            "no reference span covers offset {off}; got {:?}",
            spans.iter().map(|s| s.span.offset_range).collect::<Vec<_>>()
        );
    }
}

#[test]
fn references_of_local_variable() {
    let src = "fn f() -> int {\n    let count = 1\n    let total = count + count\n    total\n}";
    let decl = nth_offset(src, "count", 1);
    let use1 = nth_offset(src, "count", 2);
    let use2 = nth_offset(src, "count", 3);
    assert_refs(src, use1, &[decl, use1, use2]);
}

#[test]
fn references_of_parameter() {
    let src = "fn f(num: int) -> int {\n    num + num\n}";
    let decl = nth_offset(src, "num", 1);
    let use1 = nth_offset(src, "num", 2);
    let use2 = nth_offset(src, "num", 3);
    assert_refs(src, use1, &[decl, use1, use2]);
}

/// All reference offset-ranges at `cursor_off`.
fn refs_at(src: &str, cursor_off: usize) -> Vec<(usize, usize)> {
    let (ws, mid) = ws_of(src);
    let inf = ws.inference(mid).expect("user module inferred");
    references(&ws, mid, inf, cursor_off, &|_| Some(src), FnRefScope::AllOverloads)
        .iter()
        .map(|s| s.span.offset_range)
        .collect()
}

/// Assert the reference set covers exactly `expected` offsets AND every span is
/// `name_len` wide (name-precise -> rename-safe).
fn assert_named_refs(src: &str, cursor_off: usize, expected: &[usize], name_len: usize) {
    let ranges = refs_at(src, cursor_off);
    assert_eq!(ranges.len(), expected.len(), "ref count; got {ranges:?}");
    for &off in expected {
        assert!(
            ranges.iter().any(|(s, e)| *s <= off && off < *e),
            "no ref covers {off}; got {ranges:?}"
        );
    }
    for (s, e) in &ranges {
        assert_eq!(
            e - s,
            name_len,
            "ref span {:?} should be exactly the identifier ({name_len} bytes)",
            (s, e)
        );
    }
}

#[test]
fn definition_of_function_is_name_precise() {
    // The decl span returned for a call must be the function NAME, not the whole
    // `fn ... { ... }` item (so rename/highlight target just the identifier).
    let src = "fn helper() -> int {\n    1\n}\nfn main() {\n    let r = helper()\n}";
    let (ws, mid) = ws_of(src);
    let inf = ws.inference(mid).expect("user module inferred");
    let use_off = nth_offset(src, "= helper()", 1) + "= ".len();
    let def = definition_at(&ws, mid, inf, use_off).expect("resolves");
    assert_eq!(
        def.offset_range.1 - def.offset_range.0,
        "helper".len(),
        "definition span should be the bare name, got {:?}",
        def.offset_range
    );
}

#[test]
fn references_of_function() {
    // `b` calls helper twice via two statements (not `helper() + helper()`): an
    // operator lowers to `__op_add`, which doesn't resolve without stdlib, so
    // `b`'s body fails inference and drops out of `per_fn` -- its call sites
    // would never reach `resolved_overload_calls`. Plain calls keep it honest.
    let src = "fn helper() -> int { 1 }\nfn a() -> int { helper() }\nfn b() -> int {\n    let x = helper()\n    helper()\n}";
    let decl = nth_offset(src, "helper", 1); // fn helper
    let c1 = nth_offset(src, "helper", 2); // a()
    let c2 = nth_offset(src, "helper", 3); // b(), first (let x = helper())
    let c3 = nth_offset(src, "helper", 4); // b(), second (tail)
    // cursor on the first call site.
    assert_named_refs(src, c1, &[decl, c1, c2, c3], "helper".len());
}

#[test]
fn references_of_function_from_declaration() {
    // Cursor on the `fn helper` name resolves the same reference set.
    let src = "fn helper() -> int { 1 }\nfn a() -> int { helper() }";
    let decl = nth_offset(src, "helper", 1);
    let call = nth_offset(src, "helper", 2);
    assert_named_refs(src, decl, &[decl, call], "helper".len());
}

#[test]
fn references_of_function_includes_ufcs_call() {
    // The UFCS call `5.inc()` lowers to Callee::Unresolved with the method name
    // NOT at the call-span start -- only a recorded name_span gets this right.
    let src = "fn inc(x: int) -> int { x + 1 }\nfn main() {\n    let a = inc(5)\n    let b = 5.inc()\n}";
    let decl = nth_offset(src, "inc", 1); // fn inc
    let plain = nth_offset(src, "inc", 2); // inc(5)
    let ufcs = nth_offset(src, "inc", 3); // 5.inc()
    assert_named_refs(src, plain, &[decl, plain, ufcs], "inc".len());
}

#[test]
fn references_of_local_called_as_function() {
    // A fn-typed param called as `g()` lowers to Callee::Local with NO separate
    // Var node -- references must still find it (name span reconstructed from the
    // call span), or a rename would miss the call sites and corrupt the program.
    let src = "fn apply(g: fn() -> int) -> int {\n    g() + g()\n}";
    let decl = nth_offset(src, "g", 1); // apply(g
    let call1 = nth_offset(src, "g(", 1); // first g()
    let call2 = nth_offset(src, "g(", 2); // second g()
    assert_refs(src, call1, &[decl, call1, call2]);
}
