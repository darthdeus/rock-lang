//! Goto-definition on a macro invocation name jumps to the `macro` decl, not the expansion.

use crate::fixture::{Project, pos};
use bone_query::BoneWorkspace;
use bone_query::FileUriExt;
use lsp_types::Uri;

fn probe_path(name: &str) -> String {
    if cfg!(windows) {
        format!("C:/probe/{name}")
    } else {
        format!("/probe/{name}")
    }
}

#[test]
fn def_macro_invocation_jumps_to_the_macro_decl_cross_file() {
    let (def_path, use_path) = (probe_path("def.rock"), probe_path("use.rock"));
    let use_src = "fn main() {\n    twice(41)\n}\n";
    let mut bone = BoneWorkspace::new();
    bone.update(
        &def_path,
        "macro twice(x: expr) {\n    let _ = x + x\n}\n".into(),
    );
    bone.update(&use_path, use_src.into());
    let loc = bone
        .definition(&use_path, pos(use_src, "twice(41)", 2))
        .expect("goto-def on the macro invocation name");
    assert!(
        loc.uri.as_str().ends_with("def.rock"),
        "{}",
        loc.uri.as_str()
    );
    assert_eq!((loc.range.start.line, loc.range.start.character), (0, 6));
}

#[test]
fn public_macro_navigation_survives_namespace_construction() {
    let declaration = "macro twice(x: expr) { x + x }\n";
    let invocation = "fn main() { let value = twice(21) }\n";
    for language in ["", "\n[language]\nmodules = \"v1\"\n"] {
        let manifest = format!(
            "[package]\nname = \"macro_navigation\"\nversion = \"0.1.0\"\ntype = \"bin\"\n{language}"
        );
        let project = Project::new(&[
            ("stick.toml", &manifest),
            ("macros.rock", declaration),
            ("main.rock", invocation),
        ]);
        // The opened path is as spelled; the definition URI is canonical.
        let main = project.dir.path().join("main.rock");
        let main = main.to_str().unwrap();
        let mut bone = BoneWorkspace::new();
        bone.update(main, invocation.to_string());
        let location = bone
            .definition(main, pos(invocation, "twice(21)", 2))
            .expect("public macro invocation definition");
        assert_eq!(
            location.uri,
            Uri::from_file_path(&project.path("macros.rock")).unwrap()
        );
        assert_eq!(location.range.start, pos(declaration, "twice(x:", 0));
    }
}

/// Macros can overload by arity; goto-def picks the overload the call's arity selects.
#[test]
fn def_macro_overload_resolves_to_the_arity_matching_decl() {
    let path = probe_path("overload.rock");
    let src = "macro pick(a: expr) {\n    let _ = a\n}\nmacro pick(a: expr, b: expr) {\n    let _ = a\n    let _ = b\n}\nfn main() {\n    pick(1)\n    pick(1, 2)\n}\n";
    let mut bone = BoneWorkspace::new();
    bone.update(&path, src.into());
    for (call, decl_line) in [("pick(1)", 0), ("pick(1, 2)", 3)] {
        let loc = bone.definition(&path, pos(src, call, 2)).expect(call);
        assert_eq!(
            (loc.range.start.line, loc.range.start.character),
            (decl_line, 6),
            "{call}"
        );
    }
}
