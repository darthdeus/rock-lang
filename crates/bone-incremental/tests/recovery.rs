//! Error recovery. Lowering is total: an unresolved type name in any position
//! lowers to `BuiltinTy::Error` with a diagnostic, and `semantic_pass` returns
//! a coherent workspace. Inference keeps what it inferred around a failing
//! statement, so hover and completion keep working in a body with one bad
//! line. A binding whose value failed is poisoned to `<error>`, so one root
//! cause yields one diagnostic instead of a cascade at every use, and a nested
//! body under a failed scrutinee is still analyzed. Session-side cascade and
//! convergence across files: `incremental.rs`.

use crate::common;
use bone_semantic::prelude::*;

fn check(src: &str) -> (Workspace, ModuleId) {
    let ast = skull_parser::parse_module_clean(src, Ustr::from("<recovery>")).expect("parse");
    let ws = semantic_pass(&mut [ast]).expect("an error must recover, not abort the pass");
    let mid = ws
        .modules
        .iter()
        .map(|m| m.id)
        .find(|m| !m.is_builtin())
        .expect("user module survives");
    (ws, mid)
}

fn has_fn(ws: &Workspace, mid: ModuleId, name: &str) -> bool {
    ws.module(mid)
        .functions_by_name
        .keys()
        .any(|key| key.as_str().rsplit("::").next() == Some(name))
}

/// Rendered type at byte `offset`, read through the `type_at` path hover and
/// member completion use.
fn type_at(src: &str, offset: usize) -> Option<String> {
    let (ws, mid) = check(src);
    let inf = ws.inference(mid).expect("inference for user module");
    let at = bone_semantic::query::type_at(&ws, mid, inf, offset)?;
    Some(ws.render(at.body, at.ty))
}

/// Lowering, interface and inference diagnostics of a batch check.
fn workspace_diags(ws: &Workspace) -> Vec<String> {
    ws.inferences_iter()
        .flat_map(|(_, inf)| inf.errors.values().flatten())
        .chain(
            ws.modules
                .iter()
                .flat_map(|m| m.lowering_errors.values().flatten()),
        )
        .chain(ws.modules.iter().flat_map(|m| m.interface_errors.iter()))
        .map(|e| e.message.clone())
        .collect()
}

fn session_diags(src: &str) -> Vec<String> {
    let mut s = common::session(&[]);
    let _ = s.update_file("c.rock", src);
    s.diagnostics().iter().map(|e| e.message.clone()).collect()
}

/// Every binding form poisons its pattern when its value fails: exactly the
/// root error surfaces, the poisoned names render as `<error>`, and a body
/// nested under the failed value is still inferred (`z` is `int`).
#[test]
fn a_failed_value_poisons_its_bindings_without_a_cascade() {
    let cases: &[(&str, &[(&str, &str)])] = &[
        (
            "let x = undefined_thing\n    let a = use_it(x)\n    let b = use_it(x)",
            &[("x = undefined", "<error>")],
        ),
        (
            "let (a, b) = undefined_thing\n    let z = use_it(a)",
            &[("a, b", "<error>"), ("z = use_it", "int")],
        ),
        (
            "for x in undefined_thing {\n        let z = use_it(x)\n    }",
            &[("x in", "<error>"), ("z = use_it", "int")],
        ),
        (
            "while let x = undefined_thing {\n        let z = use_it(x)\n    }",
            &[("z = use_it", "int")],
        ),
        (
            "if let x = undefined_thing {\n        let z = use_it(x)\n    }",
            &[("z = use_it", "int")],
        ),
        // `?.let` rebinds the receiver's name inside the body.
        (
            "let r = undefined_thing?.let {\n        let z = use_it(undefined_thing)\n        z\n    }",
            &[("z = use_it", "int")],
        ),
    ];
    for (body, probes) in cases {
        let src =
            format!("fn use_it(n: int) -> int {{\n    n\n}}\n\nfn main() {{\n    {body}\n}}\n");
        let diags = session_diags(&src);
        assert!(
            diags.len() == 1 && diags[0].contains("undefined_thing"),
            "one root cause must yield one diagnostic: {diags:?}\n{src}"
        );
        for (needle, expected) in *probes {
            let offset = src.find(needle).expect("probe present");
            assert_eq!(
                type_at(&src, offset).as_deref(),
                Some(*expected),
                "type at {needle:?}\n{src}"
            );
        }
    }
}

/// A type error in a body, signature, field or global lowers to `Error`: the
/// item keeps its signature, its sibling survives, and the error surfaces.
#[test]
fn type_errors_in_any_position_recover_and_keep_siblings() {
    let cases: &[(&str, &[&str])] = &[
        (
            "fn broken() -> int {\n    let v: Nonexistent = 0\n    1\n}\nfn f() -> int { 3 }",
            &["broken", "f"],
        ),
        (
            "fn broken(p: Nonexistent) -> int { 1 }\nfn f() -> int { 3 }",
            &["broken", "f"],
        ),
        ("struct Bad { v: Nonexistent }\nfn f() -> int { 1 }", &["f"]),
        ("global g: Nonexistent = 0\nfn f() -> int { 1 }", &["f"]),
    ];
    for (src, kept) in cases {
        let (ws, mid) = check(src);
        for name in *kept {
            assert!(has_fn(&ws, mid, name), "`{name}` survives: {src}");
        }
        let diags = workspace_diags(&ws);
        assert!(
            diags.iter().any(|d| d.contains("Nonexistent")),
            "the unresolved type surfaces: {diags:?}"
        );
    }
}

/// A session commits a recovered error (the function stays present), surfaces
/// it, and leaves no stale diagnostic once the text is fixed.
#[test]
fn recovered_errors_commit_and_clear_on_fix() {
    let cases = [
        (
            "fn f(p: Nonexistent) -> int { 1 }\nfn g() -> int { 2 }",
            "fn f(p: int) -> int { p }\nfn g() -> int { 2 }",
        ),
        (
            "fn f() -> int {\n    let v: Nonexistent = 0\n    1\n}\nfn g() -> int { 2 }",
            "fn f() -> int { 1 }\nfn g() -> int { 2 }",
        ),
    ];
    for (broken, fixed) in cases {
        let mut s = common::session(&[]);
        let _ = s.update_file("c.rock", broken);
        assert!(!s.diagnostics().is_empty(), "the error surfaces: {broken}");
        let mid = s.workspace().module_ids[&Ustr::from("c.rock")];
        assert!(
            has_fn(s.workspace(), mid, "f"),
            "the edit commits: {broken}"
        );

        let _ = s.update_file("c.rock", fixed);
        assert!(
            s.diagnostics().is_empty(),
            "no stale diagnostics after the fix: {:?}",
            s.diagnostics()
        );
    }
}

/// `p` keeps its substituted type whether it is bound before or after a
/// statement that fails to type-check, and the failure is still recorded.
#[test]
fn inference_keeps_bindings_around_a_failed_statement() {
    let pair = "struct Pair<A, B> {\n    first: A,\n    second: B,\n}\n\n";
    let let_p = "let p = Pair { first: 1, second: \"hi\" }";
    let bad = "let n: int = \"not an int\"";
    for (body, fails) in [
        (format!("{let_p}\n    let z = p"), false),
        (format!("{let_p}\n    {bad}"), true),
        (format!("{bad}\n    {let_p}"), true),
    ] {
        let src = format!("{pair}fn main() {{\n    {body}\n}}\n");
        let offset = src.find("p = Pair").expect("binding present");
        assert_eq!(
            type_at(&src, offset).as_deref(),
            Some("Pair<int, string>"),
            "{src}"
        );
        let (ws, mid) = check(&src);
        let inf = ws.inference(mid).expect("inference for user module");
        assert_eq!(!inf.errors.is_empty(), fails, "{src}");
    }
}

/// Two independent type errors in one body both surface.
#[test]
fn two_inference_errors_in_one_body_both_surface() {
    let diags = session_diags(
        "fn main() {\n    let a: int = \"bad one\"\n    let b: int = \"bad two\"\n}\n",
    );
    assert!(diags.len() >= 2, "both type errors surface: {diags:?}");
}
