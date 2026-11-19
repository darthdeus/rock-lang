//! Code actions through `compute_code_actions_workspace`: the action is
//! offered at the cursor, and applying its edit yields the expected source.

use crate::common::*;
use lsp_types::*;

fn find<'a>(actions: &'a [CodeAction], title: &str) -> Option<&'a CodeAction> {
    actions.iter().find(|a| a.title.contains(title))
}

fn struct_literal(decl: &str, literal: &str) -> String {
    format!("{decl}\n\nfn main() {{\n    let p = {literal}\n}}\n")
}

fn color_match(arms: &str) -> String {
    format!(
        "enum Color {{ Red, Green, Blue }}\n\nfn main() {{\n    let c = Color::Red\n    match c {{{arms}}}\n}}\n"
    )
}

const POINT: &str = "struct Point { x: int, y: int }";
const BAR: &str = "struct Bar { a: int, b: int = 5 }";

#[test]
fn applied_actions_produce_the_expected_source() {
    let shapes = "enum Shape { Circle(float), Rect(float, float), Square { side: float }, Dot }\n\n\
                  fn main() {\n    let c = Shape::Dot\n    match c {}\n}\n";
    let mistyped = "enum TargetError { TooFar, NoLos }\n\n\
                    fn message(e: TargetError) -> string {\n    match e {\n    }\n}\n";
    // (source, cursor, title fragment, text the result contains, text it holds at most once)
    let cases: &[(String, (u32, u32), &str, &[&str], &str)] = &[
        (
            "fn f(v: int) -> int {\n    let y = g(v)\n    return y + 1\n}\nfn g(a: int) -> int { a }\n"
                .into(),
            (2, 11),
            "Inline local",
            &["    return g(v) + 1\n"],
            "let y",
        ),
        (
            struct_literal(POINT, "Point {}"),
            (3, 16),
            "struct field",
            &["Point { x: ?, y: ? }"],
            "",
        ),
        (
            struct_literal(POINT, "Point { x: 1 }"),
            (3, 20),
            "struct field",
            &["Point { x: 1, y: ? }"],
            "x: ?",
        ),
        (
            struct_literal(POINT, "Point { x: 1, }"),
            (3, 20),
            "struct field",
            &["Point { x: 1, y: ? }"],
            "",
        ),
        // A field with a default may be omitted.
        (
            struct_literal(BAR, "Bar {}"),
            (3, 17),
            "struct field",
            &["Bar { a: ? }"],
            "b: ?",
        ),
        (
            struct_literal(POINT, "Point {\n        x: 1\n    }"),
            (4, 8),
            "struct field",
            &["        x: 1,\n        y: ?,\n"],
            "",
        ),
        (
            color_match(""),
            (4, 4),
            "3 missing match arms",
            &["Color::Red => {}", "Color::Green => {}", "Color::Blue => {}"],
            "",
        ),
        (
            color_match("\n        Color::Red => {}\n    "),
            (4, 4),
            "2 missing match arms",
            &["Color::Green => {}", "Color::Blue => {}"],
            "Color::Red =>",
        ),
        (
            shapes.into(),
            (4, 4),
            "missing match arms",
            &[
                "Shape::Circle(_) => {}",
                "Shape::Rect(_, _) => {}",
                "Shape::Square { .. } => {}",
                "Shape::Dot => {}",
            ],
            "",
        ),
        // The wrong return annotation is an inference error; the parameter
        // still fixes the scrutinee type.
        (
            mistyped.into(),
            (3, 4),
            "missing match arms",
            &["TargetError::TooFar => {}", "TargetError::NoLos => {}"],
            "",
        ),
    ];
    for (src, (line, ch), title, contains, absent) in cases {
        let actions = run(src, *line, *ch);
        let action = find(&actions, title)
            .unwrap_or_else(|| panic!("{title:?} at {line}:{ch} in {src}: {actions:?}"));
        let out = apply_all(src, action);
        for want in *contains {
            assert!(out.contains(want), "{title}: missing {want:?} in {out}");
        }
        assert!(
            absent.is_empty() || out.matches(absent).count() <= 1,
            "{title}: {out}"
        );
    }
}

#[test]
fn inline_local_is_a_refactor_inline_action() {
    let actions = run("fn f() -> int {\n    let y = 1\n    return y\n}\n", 2, 11);
    let action = find(&actions, "Inline local").expect("inline local offered");
    assert_eq!(action.kind, Some(CodeActionKind::REFACTOR_INLINE));
}

#[test]
fn actions_not_offered() {
    let cases = [
        (
            "fn f(p: int) -> int {\n    return p + 1\n}\n",
            (1, 11),
            "Inline local",
        ),
        (
            &struct_literal(BAR, "Bar { a: 1 }"),
            (3, 18),
            "struct field",
        ),
        (
            &struct_literal(POINT, "Point { x: 1, y: 2 }"),
            (3, 20),
            "struct field",
        ),
        (
            &struct_literal(POINT, "Point { x: 1, y: 2 }"),
            (0, 8),
            "struct field",
        ),
        (
            &color_match(
                "\n        Color::Red => {}\n        Color::Green => {}\n        Color::Blue => {}\n    ",
            ),
            (4, 4),
            "match arm",
        ),
        (
            &color_match("\n        Color::Red => {}\n        _ => {}\n    "),
            (4, 4),
            "match arm",
        ),
        (&color_match(""), (3, 8), "match arm"),
        // A call with a type error against an existing function.
        (
            "fn foo(x: int) {}\n\nfn main() {\n    foo(\"bad\")\n}\n",
            (3, 4),
            "Generate function",
        ),
        (
            "fn main() {\n    let x = 1\n}\n",
            (1, 8),
            "Generate function",
        ),
    ];
    for (src, (line, ch), title) in cases {
        let actions = run(src, line, ch);
        assert!(
            find(&actions, title).is_none(),
            "{title} at {line}:{ch} in {src}: {actions:?}"
        );
    }
}

/// Type-check `src` on a fresh bone workspace.
fn errors_and_warnings(src: &str) -> (Vec<String>, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("checked.rock");
    let diagnostics = bone_ws(&path, src)
        .bone
        .diagnostics_for(path.to_str().unwrap());
    let (errors, rest): (Vec<_>, Vec<_>) = diagnostics
        .into_iter()
        .partition(|d| d.severity == Some(DiagnosticSeverity::ERROR));
    let messages = |ds: Vec<Diagnostic>| ds.into_iter().map(|d| d.message).collect();
    (messages(errors), messages(rest))
}

/// A generated stub resolves the call: no new errors, and its `?` body is a
/// visible expression-hole warning.
#[test]
fn generated_function_stubs_resolve_the_call() {
    let ambiguous = "mod a { pub fn hit() {} }\nmod b { pub global hit: int = 0 }\nuse a::*;\nuse b::*;\n\
                     fn main() {\n    hit()\n    want(3)\n}\n";
    // (source, cursor, signature, error the stub leaves in place)
    for (src, (line, ch), signature, remaining_error) in [
        (
            "fn main() {\n    do_thing(5)\n}\n",
            (1, 6),
            "fn do_thing(arg0: int)",
            None,
        ),
        (
            "fn main() {\n    let count = 5\n    tally(count)\n}\n",
            (2, 4),
            "fn tally(count: int)",
            None,
        ),
        (
            "global count: int = 1\nfn main() {\n    want(count)\n}\n",
            (2, 6),
            "fn want(count: any)",
            None,
        ),
        (
            "fn main() {\n    let count = 5\n    place(count, 9)\n}\n",
            (2, 4),
            "fn place(count: int, arg1: int)",
            None,
        ),
        (
            "fn main() {\n    let s: string = label(7)\n}\n",
            (1, 20),
            "fn label(arg0: int) -> string",
            None,
        ),
        (ambiguous, (6, 6), "fn want(arg0: int)", Some("hit")),
    ] {
        let actions = run(src, line, ch);
        let name = signature[3..].split('(').next().unwrap();
        let action = find(&actions, "Generate function").expect("generate-function action");
        assert!(action.title.contains(name), "title: {}", action.title);
        let out = apply_all(src, action);
        assert!(out.contains(signature), "missing {signature:?}: {out}");
        let (errors, warnings) = errors_and_warnings(&out);
        match remaining_error {
            None => assert!(errors.is_empty(), "{errors:?}\n{out}"),
            Some(e) => assert!(
                errors.iter().all(|m| m.contains(e)) && !errors.is_empty(),
                "{errors:?}"
            ),
        }
        assert!(
            warnings.iter().any(|m| m.contains("expression hole")),
            "{warnings:?}"
        );
    }
}

/// Quick-fixes carry the diagnostic they resolve; suppression is keyed on a
/// known lint name in `Diagnostic.code`, never on the message.
#[test]
fn quick_fixes_carry_the_diagnostic_they_resolve() {
    let unused = "fn main() {\n    let unused_thing = 5\n}\n";
    let partial = "enum Color { Red, Green, Blue }\n\nfn f(c: Color) {\n    match c {\n        Color::Red => {}\n    }\n}\n";
    for (src, lint, message, title, expected) in [
        (
            unused,
            true,
            "unused variable",
            "Suppress",
            Some("fn main() {\n    // rock: allow(unused_var)\n    let unused_thing = 5\n}\n"),
        ),
        (
            partial,
            false,
            "Non-exhaustive match",
            "missing match arm",
            None,
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.rock");
        let mut ws = bone_ws(&path, src);
        let file = path.to_str().unwrap();
        let diagnostics = if lint {
            ws.bone.lint_diagnostics(file)
        } else {
            ws.bone.diagnostics_for(file)
        };
        let diag = diagnostics
            .into_iter()
            .find(|d| d.message.contains(message))
            .unwrap_or_else(|| panic!("{message} diagnostic"));
        let start = diag.range.start;
        let actions = actions_at(
            &mut ws,
            &path,
            start.line,
            start.character,
            vec![diag.clone()],
        );
        let action = find(&actions, title).unwrap_or_else(|| panic!("{title}: {actions:?}"));
        assert_eq!(action.diagnostics.as_deref(), Some(&[diag][..]));
        if let Some(expected) = expected {
            assert!(action.title.contains("unused_var"), "{}", action.title);
            assert_eq!(apply_all(src, action), expected);
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.rock");
    let mut ws = bone_ws(&path, unused);
    let not_a_lint = Diagnostic {
        range: Range::new(Position::new(1, 8), Position::new(1, 20)),
        source: Some("bone".to_string()),
        code: Some(NumberOrString::String("not_a_lint".to_string())),
        message: "unused variable".to_string(),
        ..Default::default()
    };
    for diagnostics in [vec![], vec![not_a_lint]] {
        let actions = actions_at(&mut ws, &path, 1, 8, diagnostics);
        assert!(find(&actions, "Suppress").is_none(), "{actions:?}");
    }
}
