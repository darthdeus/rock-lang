//! Completion CONTEXT-COVERAGE sweep. Probes completion at a cursor (`$0`) in
//! every syntactic context we care about and asserts a plausible candidate is
//! offered -- the guard against a context silently returning nothing (the class
//! of bug where field-name completion "ate" a value slot, or literal recovery
//! failed inside a lambda). Each case shares a small preamble of named symbols;
//! expression contexts expect the `MARKER` global, type contexts the `MarkerS`
//! struct. One test reports ALL broken contexts at once. Ranking is guarded
//! separately (`completion_ranking.rs`).

use bone_query::BoneWorkspace;
use lsp_types::{HoverContents, Position};

#[cfg(not(windows))]
const PATH: &str = "/tmp/rock_lsp_sweep.rock";
#[cfg(windows)]
const PATH: &str = r"C:\tmp\rock_lsp_sweep.rock";

const PREAMBLE: &str = "global MARKER: int = 0\n\
     fn marker_fn() -> int { 0 }\n\
     struct MarkerS { f: int }\n\
     enum MarkerE { AAA, BBB }\n";

fn labels_for(full: &str) -> Vec<String> {
    let off = full.find("$0").expect("`$0` marker");
    let before = &full[..off];
    let line = before.matches('\n').count() as u32;
    let col = (before.len() - before.rfind('\n').map(|i| i + 1).unwrap_or(0)) as u32;
    let src = full.replacen("$0", "", 1);
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src);
    bone.completion(PATH, Position::new(line, col))
        .into_iter()
        .map(|c| c.label)
        .collect()
}

#[test]
fn completion_context_sweep() {
    // (name, body appended to PREAMBLE, expected-substring-in-some-label)
    let cases: &[(&str, &str, &str)] = &[
        // -- expression contexts (expect the MARKER global) --
        ("statement", "fn f() {\n  mar$0\n}\n", "MARKER"),
        (
            "return value",
            "fn f() -> int {\n  return mar$0\n}\n",
            "MARKER",
        ),
        ("let value", "fn f() {\n  let a = mar$0\n}\n", "MARKER"),
        ("if condition", "fn f() {\n  if mar$0 {}\n}\n", "MARKER"),
        (
            "while condition",
            "fn f() {\n  while mar$0 {}\n}\n",
            "MARKER",
        ),
        (
            "binary operand",
            "fn f() {\n  let a = 1 + mar$0\n}\n",
            "MARKER",
        ),
        (
            "call arg",
            "fn g(x: int) {}\nfn f() {\n  g(mar$0)\n}\n",
            "MARKER",
        ),
        (
            "second call arg",
            "fn g(x: int, y: int) {}\nfn f() {\n  g(1, mar$0)\n}\n",
            "MARKER",
        ),
        (
            "nested call arg",
            "fn g(x: int) -> int { x }\nfn f() {\n  let a = g(g(mar$0))\n}\n",
            "MARKER",
        ),
        (
            "array element",
            "fn f() {\n  let xs = [mar$0]\n}\n",
            "MARKER",
        ),
        (
            "array element after comma",
            "fn f() {\n  let xs = [1, mar$0]\n}\n",
            "MARKER",
        ),
        (
            "array in global init",
            "global XS: [int] = [mar$0]\n",
            "MARKER",
        ),
        (
            "struct field value",
            "fn f() {\n  let s = MarkerS { f: mar$0 }\n}\n",
            "MARKER",
        ),
        (
            "nested struct field value",
            "struct Outer { inner: MarkerS }\nfn f() {\n  let o = Outer { inner: MarkerS { f: mar$0 } }\n}\n",
            "MARKER",
        ),
        (
            "struct value in array global init",
            "global XS: [MarkerS] = [\n  MarkerS { f: mar$0 }\n]\n",
            "MARKER",
        ),
        (
            "match arm body",
            "fn f(e: MarkerE) {\n  match e {\n    MarkerE::AAA => mar$0\n  }\n}\n",
            "MARKER",
        ),
        (
            "match scrutinee",
            "fn f() {\n  match mar$0 {}\n}\n",
            "MARKER",
        ),
        (
            "for iterable",
            "fn f() {\n  for x in mar$0 {}\n}\n",
            "MARKER",
        ),
        (
            "index expr",
            "fn f(xs: [int]) {\n  let a = xs[mar$0]\n}\n",
            "MARKER",
        ),
        (
            "lambda body",
            "fn f() {\n  let g = () => mar$0\n}\n",
            "MARKER",
        ),
        (
            "if-expr branch value",
            "fn f() {\n  let a = if true { mar$0 } else { 0 }\n}\n",
            "MARKER",
        ),
        (
            "nullable coalesce rhs",
            "fn f(a: int?) {\n  let b = a ?? mar$0\n}\n",
            "MARKER",
        ),
        // -- type contexts (expect the MarkerS struct) --
        (
            "let type annotation",
            "fn f() {\n  let a: Mar$0 = todo()\n}\n",
            "MarkerS",
        ),
        ("param type", "fn f(a: Mar$0) {}\n", "MarkerS"),
        ("return type", "fn f() -> Mar$0 {}\n", "MarkerS"),
        ("struct field type", "struct S { x: Mar$0 }\n", "MarkerS"),
        ("global type", "global G: Mar$0 = todo()\n", "MarkerS"),
        ("array element type", "fn f(a: [Mar$0]) {}\n", "MarkerS"),
        (
            "generic type arg",
            "fn f(a: Map<int, Mar$0>) {}\n",
            "MarkerS",
        ),
        // -- qualified / member --
        (
            "enum qualifier",
            "fn f() {\n  let e = MarkerE::$0\n}\n",
            "AAA",
        ),
        (
            "member access",
            "fn f(s: MarkerS) {\n  let a = s.$0\n}\n",
            "f: int",
        ),
        // -- deep nesting (the class that actually broke) --
        (
            "lambda in struct-lit in array global-init",
            "struct P { is_open: fn() -> bool }\nglobal PANELS: [P] = [\n  P { is_open: () => mar$0 }\n]\n",
            "MARKER",
        ),
        (
            "lambda in struct-lit, later sibling entry present",
            "struct P { is_open: fn() -> bool }\nglobal PANELS: [P] = [\n  P { is_open: () => mar$0 },\n  P { is_open: () => true }\n]\n",
            "MARKER",
        ),
        (
            "method-chain arg inside lambda in field",
            "struct P { r: fn() -> int }\nfn scale(x: int) -> int { x }\nfn f(s: MarkerS) {\n  let p = P { r: () => scale(s.$0) }\n}\n",
            "f: int",
        ),
        (
            "call arg with method-chain receiver",
            "fn scale(x: int) -> int { x }\nfn f(s: MarkerS) {\n  let a = scale(s.f).$0\n}\n",
            "MARKER",
        ),
        // -- mid-edit / UNBALANCED buffers (completion fires while typing) --
        (
            "unbalanced call arg",
            "fn g(x: int) {}\nfn f() {\n  g(mar$0\n}\n",
            "MARKER",
        ),
        (
            "unbalanced struct value",
            "fn f() {\n  let s = MarkerS { f: mar$0\n",
            "MARKER",
        ),
        (
            "unbalanced array element",
            "fn f() {\n  let xs = [1, mar$0\n",
            "MARKER",
        ),
        (
            "unbalanced lambda in struct in array",
            "struct P { is_open: fn() -> bool }\nglobal PANELS: [P] = [\n  P { is_open: () => mar$0\n",
            "MARKER",
        ),
        (
            "unbalanced nested call",
            "fn g(x: int) -> int { x }\nfn f() {\n  let a = g(g(mar$0\n}\n",
            "MARKER",
        ),
        (
            "incomplete member then more code",
            "fn f(s: MarkerS) {\n  let a = s.$0\n  let b = 1\n}\n",
            "f: int",
        ),
    ];

    let mut fails: Vec<String> = Vec::new();
    for (name, body, expected) in cases {
        let full = format!("{PREAMBLE}{body}");
        let labels = labels_for(&full);
        let hit = labels.iter().any(|l| l == expected || l.contains(expected));
        if !hit {
            let sample: Vec<&String> = labels.iter().take(8).collect();
            fails.push(format!(
                "  [{name}] expected `{expected}`, got (n={}): {sample:?}",
                labels.len()
            ));
        }
    }
    assert!(
        fails.is_empty(),
        "broken completion contexts:\n{}",
        fails.join("\n")
    );
}

fn hover_at(src: &str, line: u32, col: u32) -> String {
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    match bone
        .hover(PATH, Position::new(line, col))
        .map(|h| h.contents)
    {
        Some(HoverContents::Markup(m)) => m.value,
        _ => String::new(),
    }
}

/// Hover must be stable across EVERY offset of an identifier -- the first char
/// must not resolve differently from the rest. This is the class the completion
/// sweep couldn't see: a spurious 1-wide `never` node at an identifier start made
/// hover flicker `never` at the first char and the real type one char in
/// (canonical: a lambda body `() => IDENT` in a struct-literal field). Every
/// context must hover the `MARKER` (int) global INFORMATIVELY (not `never` /
/// `<error>` / empty) and IDENTICALLY at its first char and one char in.
#[test]
fn hover_context_sweep() {
    // A distinct `MARKER` (int) occurrence in each context.
    let cases: &[(&str, &str)] = &[
        ("statement", "fn f() {\n  MARKER\n}\n"),
        ("return value", "fn f() -> int {\n  return MARKER\n}\n"),
        ("let value", "fn f() {\n  let a = MARKER\n}\n"),
        ("binary operand", "fn f() {\n  let a = 1 + MARKER\n}\n"),
        ("call arg", "fn g(x: int) {}\nfn f() {\n  g(MARKER)\n}\n"),
        ("array element", "fn f() {\n  let xs = [MARKER]\n}\n"),
        (
            "struct field value",
            "struct B { n: int }\nfn f() {\n  let s = B { n: MARKER }\n}\n",
        ),
        ("lambda body", "fn f() {\n  let g = () => MARKER\n}\n"),
        (
            "lambda in struct-lit in array global-init",
            "struct P { get: fn() -> int }\nglobal PANELS: [P] = [\n  P { get: () => MARKER }\n]\n",
        ),
        (
            "match arm body",
            "fn f(e: MarkerE) {\n  match e {\n    MarkerE::AAA => MARKER\n  }\n}\n",
        ),
        (
            "if branch value",
            "fn f() {\n  let a = if true { MARKER } else { 0 }\n}\n",
        ),
    ];

    let mut fails: Vec<String> = Vec::new();
    for (name, body) in cases {
        let full = format!("{PREAMBLE}{body}");
        // locate the MARKER occurrence inside `body` (skip the PREAMBLE decl).
        let bidx = body.find("MARKER").expect("MARKER in body");
        let before = &body[..bidx];
        let line = (PREAMBLE.matches('\n').count() + before.matches('\n').count()) as u32;
        let col = (before.len() - before.rfind('\n').map(|i| i + 1).unwrap_or(0)) as u32;
        let start = hover_at(&full, line, col);
        let mid = hover_at(&full, line, col + 1);
        let uninformative =
            start.is_empty() || start.contains("never") || start.contains("<error>");
        if uninformative {
            fails.push(format!(
                "  [{name}] hover at START uninformative = {start:?}"
            ));
        }
        if start != mid {
            fails.push(format!(
                "  [{name}] hover START {start:?} != MID {mid:?} (offset-unstable)"
            ));
        }
    }
    assert!(
        fails.is_empty(),
        "broken/unstable hover contexts:\n{}",
        fails.join("\n")
    );
}

fn def_count(src: &str, line: u32, col: u32) -> usize {
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    bone.definition(PATH, Position::new(line, col))
        .into_iter()
        .count()
}

/// Goto-definition must resolve at EVERY offset of a reference, including its
/// first char -- the same phantom-`never`-node boundary that broke hover also
/// made definition return nothing at the start of a lambda-body identifier.
#[test]
fn definition_context_sweep() {
    // Each references the `marker_fn` fn (has a definition to jump to).
    let cases: &[(&str, &str)] = &[
        ("statement call", "fn f() {\n  marker_fn()\n}\n"),
        (
            "call arg",
            "fn g(x: int) {}\nfn f() {\n  g(marker_fn())\n}\n",
        ),
        ("array element", "fn f() {\n  let xs = [marker_fn()]\n}\n"),
        ("lambda body", "fn f() {\n  let g = () => marker_fn()\n}\n"),
        (
            "lambda in struct-lit in array global-init",
            "struct P { get: fn() -> int }\nglobal PANELS: [P] = [\n  P { get: () => marker_fn() }\n]\n",
        ),
        (
            "struct field value",
            "struct B { n: int }\nfn f() {\n  let s = B { n: marker_fn() }\n}\n",
        ),
    ];
    let mut fails: Vec<String> = Vec::new();
    for (name, body) in cases {
        let full = format!("{PREAMBLE}{body}");
        let bidx = body.find("marker_fn").expect("marker_fn in body");
        let before = &body[..bidx];
        let line = (PREAMBLE.matches('\n').count() + before.matches('\n').count()) as u32;
        let col = (before.len() - before.rfind('\n').map(|i| i + 1).unwrap_or(0)) as u32;
        if def_count(&full, line, col) == 0 {
            fails.push(format!(
                "  [{name}] goto-definition at START resolved nothing"
            ));
        }
    }
    assert!(
        fails.is_empty(),
        "broken definition contexts:\n{}",
        fails.join("\n")
    );
}
