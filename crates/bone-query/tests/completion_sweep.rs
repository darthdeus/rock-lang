//! Context-coverage sweep: completion, hover and goto-definition in every syntactic context.

use crate::completion::{complete_in, cursor, label_matches, labels_of};
use crate::fixture::{hover_markup, workspace_at};
use lsp_types::Position;

#[cfg(not(windows))]
const PATH: &str = "/tmp/rock_lsp_sweep.rock";
#[cfg(windows)]
const PATH: &str = r"C:\tmp\rock_lsp_sweep.rock";

const PREAMBLE: &str = "global MARKER: int = 0\n\
     fn marker_fn() -> int { 0 }\n\
     struct MarkerS { f: int }\n\
     enum MarkerE { AAA, BBB }\n";

const CALL_CONTEXTS: &[(&str, &str)] = &[
    ("statement", "fn f() {\n  <expr>\n}\n"),
    ("call arg", "fn g(x: int) {}\nfn f() {\n  g(<expr>)\n}\n"),
    ("array element", "fn f() {\n  let xs = [<expr>]\n}\n"),
    ("lambda body", "fn f() {\n  let g = () => <expr>\n}\n"),
];

const VALUE_CONTEXTS: &[(&str, &str)] = &[
    ("return value", "fn f() -> int {\n  return <expr>\n}\n"),
    ("let value", "fn f() {\n  let a = <expr>\n}\n"),
    ("binary operand", "fn f() {\n  let a = 1 + <expr>\n}\n"),
    (
        "match arm body",
        "fn f(e: MarkerE) {\n  match e {\n    MarkerE::AAA => <expr>\n  }\n}\n",
    ),
    (
        "if-expr branch value",
        "fn f() {\n  let a = if true { <expr> } else { 0 }\n}\n",
    ),
];

const NESTED_CONTEXTS: &[(&str, &str)] = &[
    (
        "struct field value",
        "struct B { n: int }\nfn f() {\n  let s = B { n: <expr> }\n}\n",
    ),
    (
        "lambda in struct-lit in array global-init",
        "struct P { get: fn() -> int }\nglobal PANELS: [P] = [\n  P { get: () => <expr> }\n]\n",
    ),
];

fn fixture(body: &str, expression: &str) -> String {
    format!("{PREAMBLE}{}", body.replace("<expr>", expression))
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
        ("if condition", "fn f() {\n  if mar<|> {}\n}\n"),
        ("while condition", "fn f() {\n  while mar<|> {}\n}\n"),
        (
            "second call arg",
            "fn g(x: int, y: int) {}\nfn f() {\n  g(1, mar<|>)\n}\n",
        ),
        (
            "nested call arg",
            "fn g(x: int) -> int { x }\nfn f() {\n  let a = g(g(mar<|>))\n}\n",
        ),
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
        ("match scrutinee", "fn f() {\n  match mar<|> {}\n}\n"),
        ("for iterable", "fn f() {\n  for x in mar<|> {}\n}\n"),
        ("index expr", "fn f(xs: [int]) {\n  let a = xs[mar<|>]\n}\n"),
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
    for (name, body, expected) in CALL_CONTEXTS
        .iter()
        .chain(VALUE_CONTEXTS)
        .copied()
        .chain(expressions)
        .map(|(name, body)| (name, body, "MARKER"))
        .chain(other_contexts)
    {
        let labels = labels_of(&complete_in(PATH, &fixture(body, "mar<|>")));
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

/// Each `MARKER` (int) must hover informatively, identically at its first char and one char in.
#[test]
fn hover_context_sweep() {
    let mut fails = Vec::new();
    for (name, body) in CALL_CONTEXTS
        .iter()
        .chain(VALUE_CONTEXTS)
        .chain(NESTED_CONTEXTS)
    {
        let (src, pos) = cursor(&fixture(body, "<|>MARKER"));
        let bone = workspace_at(PATH, &src);
        let start = hover_markup(&bone, PATH, pos).unwrap_or_default();
        let mid = hover_markup(&bone, PATH, Position::new(pos.line, pos.character + 1))
            .unwrap_or_default();
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

/// Goto-definition must resolve at the first char of a lambda-body identifier.
#[test]
fn definition_context_sweep() {
    let mut fails = Vec::new();
    for (name, body) in CALL_CONTEXTS.iter().chain(NESTED_CONTEXTS) {
        let (src, pos) = cursor(&fixture(body, "<|>marker_fn()"));
        let bone = workspace_at(PATH, &src);
        if bone.definition(PATH, pos).is_none() {
            fails.push(format!(
                "  [{name}] goto-definition at START resolved nothing"
            ));
        }
    }
    report("definition", fails);
}
