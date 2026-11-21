//! Lowering is total: an unresolved type name lowers to `BuiltinTy::Error` with a diagnostic.

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

fn type_at(src: &str, offset: usize) -> Option<String> {
    let (ws, mid) = check(src);
    let inf = ws.inference(mid).expect("inference for user module");
    let at = bone_semantic::query::type_at(&ws, mid, inf, offset)?;
    Some(ws.render(at.body, at.ty))
}

fn session_diags(src: &str) -> Vec<String> {
    let mut s = common::session(&[]);
    let _ = s.update_file("c.rock", src);
    s.diagnostics().iter().map(|e| e.message.clone()).collect()
}

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
