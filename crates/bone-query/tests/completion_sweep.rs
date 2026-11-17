//! Context-coverage sweep: completion, hover and goto-definition at a cursor
//! in every syntactic context, each against a shared preamble of named
//! symbols. Guards against a context silently returning nothing (field-name
//! completion "eating" a value slot, literal recovery failing inside a
//! lambda). Each test reports all broken contexts at once.

use crate::completion::{complete_in, cursor, label_matches, labels_of};
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

/// The preamble plus `body`, and the position of `body`'s cursor marker.
fn fixture(body: &str) -> (String, Position) {
    cursor(&format!("{PREAMBLE}{body}"))
}

fn report(what: &str, fails: Vec<String>) {
    assert!(
        fails.is_empty(),
        "broken {what} contexts:\n{}",
        fails.join("\n")
    );
}

#[test]
fn completion_context_sweep() {
    // Expression contexts share the same global; type/member contexts below
    // select the symbol appropriate to that slot.
    let expressions = [
        ("statement", "fn f() {\n  mar<|>\n}\n"),
        ("return value", "fn f() -> int {\n  return mar<|>\n}\n"),
        ("let value", "fn f() {\n  let a = mar<|>\n}\n"),
        ("if condition", "fn f() {\n  if mar<|> {}\n}\n"),
        ("while condition", "fn f() {\n  while mar<|> {}\n}\n"),
        ("binary operand", "fn f() {\n  let a = 1 + mar<|>\n}\n"),
        ("call arg", "fn g(x: int) {}\nfn f() {\n  g(mar<|>)\n}\n"),
        (
            "second call arg",
            "fn g(x: int, y: int) {}\nfn f() {\n  g(1, mar<|>)\n}\n",
        ),
        (
            "nested call arg",
            "fn g(x: int) -> int { x }\nfn f() {\n  let a = g(g(mar<|>))\n}\n",
        ),
        ("array element", "fn f() {\n  let xs = [mar<|>]\n}\n"),
        (
            "array element after comma",
            "fn f() {\n  let xs = [1, mar<|>]\n}\n",
        ),
        ("array in global init", "global XS: [int] = [mar<|>]\n"),
        (
            "struct field value",
            "fn f() {\n  let s = MarkerS { f: mar<|> }\n}\n",
        ),
        (
            "nested struct field value",
            "struct Outer { inner: MarkerS }\nfn f() {\n  let o = Outer { inner: MarkerS { f: mar<|> } }\n}\n",
        ),
        (
            "struct value in array global init",
            "global XS: [MarkerS] = [\n  MarkerS { f: mar<|> }\n]\n",
        ),
        (
            "match arm body",
            "fn f(e: MarkerE) {\n  match e {\n    MarkerE::AAA => mar<|>\n  }\n}\n",
        ),
        ("match scrutinee", "fn f() {\n  match mar<|> {}\n}\n"),
        ("for iterable", "fn f() {\n  for x in mar<|> {}\n}\n"),
        ("index expr", "fn f(xs: [int]) {\n  let a = xs[mar<|>]\n}\n"),
        ("lambda body", "fn f() {\n  let g = () => mar<|>\n}\n"),
        (
            "if-expr branch value",
            "fn f() {\n  let a = if true { mar<|> } else { 0 }\n}\n",
        ),
        (
            "nullable coalesce rhs",
            "fn f(a: int?) {\n  let b = a ?? mar<|>\n}\n",
        ),
        (
            "lambda in struct-lit in array global-init",
            "struct P { is_open: fn() -> bool }\nglobal PANELS: [P] = [\n  P { is_open: () => mar<|> }\n]\n",
        ),
        (
            "lambda in struct-lit, later sibling entry present",
            "struct P { is_open: fn() -> bool }\nglobal PANELS: [P] = [\n  P { is_open: () => mar<|> },\n  P { is_open: () => true }\n]\n",
        ),
        (
            "call arg with method-chain receiver",
            "fn scale(x: int) -> int { x }\nfn f(s: MarkerS) {\n  let a = scale(s.f).<|>\n}\n",
        ),
        (
            "unbalanced call arg",
            "fn g(x: int) {}\nfn f() {\n  g(mar<|>\n}\n",
        ),
        (
            "unbalanced struct value",
            "fn f() {\n  let s = MarkerS { f: mar<|>\n",
        ),
        (
            "unbalanced array element",
            "fn f() {\n  let xs = [1, mar<|>\n",
        ),
        (
            "unbalanced lambda in struct in array",
            "struct P { is_open: fn() -> bool }\nglobal PANELS: [P] = [\n  P { is_open: () => mar<|>\n",
        ),
        (
            "unbalanced nested call",
            "fn g(x: int) -> int { x }\nfn f() {\n  let a = g(g(mar<|>\n}\n",
        ),
    ];
    let other_contexts = [
        (
            "let type annotation",
            "fn f() {\n  let a: Mar<|> = todo()\n}\n",
            "MarkerS",
        ),
        ("param type", "fn f(a: Mar<|>) {}\n", "MarkerS"),
        ("return type", "fn f() -> Mar<|> {}\n", "MarkerS"),
        ("struct field type", "struct S { x: Mar<|> }\n", "MarkerS"),
        ("global type", "global G: Mar<|> = todo()\n", "MarkerS"),
        ("array element type", "fn f(a: [Mar<|>]) {}\n", "MarkerS"),
        (
            "generic type arg",
            "fn f(a: Map<int, Mar<|>>) {}\n",
            "MarkerS",
        ),
        (
            "enum qualifier",
            "fn f() {\n  let e = MarkerE::<|>\n}\n",
            "AAA",
        ),
        (
            "member access",
            "fn f(s: MarkerS) {\n  let a = s.<|>\n}\n",
            "f: int",
        ),
        (
            "method-chain arg inside lambda in field",
            "struct P { r: fn() -> int }\nfn scale(x: int) -> int { x }\nfn f(s: MarkerS) {\n  let p = P { r: () => scale(s.<|>) }\n}\n",
            "f: int",
        ),
        (
            "incomplete member then more code",
            "fn f(s: MarkerS) {\n  let a = s.<|>\n  let b = 1\n}\n",
            "f: int",
        ),
    ];
    let mut fails = Vec::new();
    for (name, body, expected) in expressions
        .into_iter()
        .map(|(name, body)| (name, body, "MARKER"))
        .chain(other_contexts)
    {
        let labels = labels_of(&complete_in(PATH, &format!("{PREAMBLE}{body}")));
        if !labels
            .iter()
            .any(|l| label_matches(l, &format!("*{expected}*")))
        {
            let sample = &labels[..labels.len().min(8)];
            let n = labels.len();
            fails.push(format!(
                "  [{name}] expected `{expected}`, got (n={n}): {sample:?}"
            ));
        }
    }
    report("completion", fails);
}

/// Hover must agree at every offset of an identifier. A spurious 1-wide
/// `never` node at an identifier start once made hover show `never` at the
/// first char and the real type one char in (canonical: a lambda body
/// `() => IDENT` in a struct-literal field). Each `MARKER` (int) must hover
/// informatively, and identically at its first char and one char in.
#[test]
fn hover_context_sweep() {
    let cases: &[(&str, &str)] = &[
        ("statement", "fn f() {\n  <|>MARKER\n}\n"),
        ("return value", "fn f() -> int {\n  return <|>MARKER\n}\n"),
        ("let value", "fn f() {\n  let a = <|>MARKER\n}\n"),
        ("binary operand", "fn f() {\n  let a = 1 + <|>MARKER\n}\n"),
        ("call arg", "fn g(x: int) {}\nfn f() {\n  g(<|>MARKER)\n}\n"),
        ("array element", "fn f() {\n  let xs = [<|>MARKER]\n}\n"),
        (
            "struct field value",
            "struct B { n: int }\nfn f() {\n  let s = B { n: <|>MARKER }\n}\n",
        ),
        ("lambda body", "fn f() {\n  let g = () => <|>MARKER\n}\n"),
        (
            "lambda in struct-lit in array global-init",
            "struct P { get: fn() -> int }\nglobal PANELS: [P] = [\n  P { get: () => <|>MARKER }\n]\n",
        ),
        (
            "match arm body",
            "fn f(e: MarkerE) {\n  match e {\n    MarkerE::AAA => <|>MARKER\n  }\n}\n",
        ),
        (
            "if branch value",
            "fn f() {\n  let a = if true { <|>MARKER } else { 0 }\n}\n",
        ),
    ];
    let hover = |src: &str, pos: Position| {
        let mut bone = BoneWorkspace::new();
        bone.update(PATH, src.to_string());
        match bone.hover(PATH, pos).map(|h| h.contents) {
            Some(HoverContents::Markup(m)) => m.value,
            _ => String::new(),
        }
    };
    let mut fails = Vec::new();
    for (name, body) in cases {
        let (src, pos) = fixture(body);
        let start = hover(&src, pos);
        let mid = hover(&src, Position::new(pos.line, pos.character + 1));
        if start.is_empty() || start.contains("never") || start.contains("<error>") {
            fails.push(format!(
                "  [{name}] hover at START uninformative = {start:?}"
            ));
        }
        if start != mid {
            fails.push(format!("  [{name}] hover START {start:?} != MID {mid:?}"));
        }
    }
    report("hover", fails);
}

/// The same phantom-`never` boundary made goto-definition return nothing at
/// the first char of a lambda-body identifier.
#[test]
fn definition_context_sweep() {
    let cases: &[(&str, &str)] = &[
        ("statement call", "fn f() {\n  <|>marker_fn()\n}\n"),
        (
            "call arg",
            "fn g(x: int) {}\nfn f() {\n  g(<|>marker_fn())\n}\n",
        ),
        (
            "array element",
            "fn f() {\n  let xs = [<|>marker_fn()]\n}\n",
        ),
        (
            "lambda body",
            "fn f() {\n  let g = () => <|>marker_fn()\n}\n",
        ),
        (
            "lambda in struct-lit in array global-init",
            "struct P { get: fn() -> int }\nglobal PANELS: [P] = [\n  P { get: () => <|>marker_fn() }\n]\n",
        ),
        (
            "struct field value",
            "struct B { n: int }\nfn f() {\n  let s = B { n: <|>marker_fn() }\n}\n",
        ),
    ];
    let mut fails = Vec::new();
    for (name, body) in cases {
        let (src, pos) = fixture(body);
        let mut bone = BoneWorkspace::new();
        bone.update(PATH, src);
        if bone.definition(PATH, pos).is_none() {
            fails.push(format!(
                "  [{name}] goto-definition at START resolved nothing"
            ));
        }
    }
    report("definition", fails);
}
