//! GUARD: goto-definition on a macro INVOCATION name jumps to the `macro` decl,
//! not to a token from the spliced expansion.
//!
//! Macro expansion erases the invocation and `retag_block` stamps the whole
//! invocation span onto every expanded template node, so a body-first
//! definition lookup at the call site resolves whatever expanded token now sits
//! there (canonically `enum Wait` inside the `run_into` body). The fix reads the
//! macro target off the registry on the raw pre-expansion AST
//! (`macro_refs::macro_invocation_def`), arity-selected exactly like expansion.
//! Semantic-tokens twin of the same erasure root: `macro_call_site_sentinel.rs`.

use bone_query::{canonicalize_lenient, BoneWorkspace};
use lsp_types::{Position, Uri};

/// (line, char) of `src[find(marker) + into]`, 0-based.
fn pos(src: &str, marker: &str, into: usize) -> Position {
    let idx = src.find(marker).unwrap_or_else(|| panic!("marker not found: {marker:?}")) + into;
    let before = &src[..idx];
    let line = before.matches('\n').count() as u32;
    let col = (idx - before.rfind('\n').map(|i| i + 1).unwrap_or(0)) as u32;
    Position::new(line, col)
}

#[test]
fn def_macro_invocation_jumps_to_the_macro_decl_cross_file() {
    // The reported shape: the macro is defined in one file, invoked in another.
    let def_path = "/probe/def.rock";
    let use_path = "/probe/use.rock";
    let def_src = "macro twice(x: expr) {\n    let _ = x + x\n}\n";
    let use_src = "fn main() {\n    twice(41)\n}\n";

    let mut bone = BoneWorkspace::new();
    bone.update(def_path, def_src.to_string());
    bone.update(use_path, use_src.to_string());

    let loc = bone
        .definition(use_path, pos(use_src, "twice(41)", 2))
        .expect("goto-def on the macro invocation name");
    // `macro twice` -- name starts at char 6 ("macro ") on line 0 of def.rock.
    assert!(
        loc.uri.as_str().ends_with("def.rock"),
        "must jump to the macro's defining file, got {}",
        loc.uri.as_str()
    );
    assert_eq!(
        (loc.range.start.line, loc.range.start.character),
        (0, 6),
        "must land on the `macro twice` name, not an expanded body token"
    );
}

#[test]
fn public_macro_navigation_survives_namespace_construction() {
    for language in ["", "\n[language]\nmodules = \"v1\"\n"] {
        let dir = tempfile::tempdir().expect("macro project");
        std::fs::write(
            dir.path().join("stick.toml"),
            format!("[package]\nname = \"macro_navigation\"\nversion = \"0.1.0\"\ntype = \"bin\"\n{language}"),
        ).expect("project manifest");
        let declaration = "macro twice(x: expr) { x + x }\n";
        let invocation = "fn main() { let value = twice(21) }\n";
        let def_path = dir.path().join("macros.rock");
        let use_path = dir.path().join("main.rock");
        std::fs::write(&def_path, declaration).expect("macro source");
        std::fs::write(&use_path, invocation).expect("consumer source");
        let mut bone = BoneWorkspace::new();
        bone.update(use_path.to_str().unwrap(), invocation.to_string());
        let location = bone.definition(
            use_path.to_str().unwrap(), pos(invocation, "twice(21)", 2),
        ).expect("public macro invocation definition");
        assert_eq!(
            location.uri,
            Uri::from_file_path(&canonicalize_lenient(def_path.to_str().unwrap())).unwrap(),
        );
        assert_eq!(location.range.start, pos(declaration, "twice(x:", 0));
    }
}

#[test]
fn def_macro_overload_resolves_to_the_arity_matching_decl() {
    // A macro name can't collide with a fn (rule: macro names are unique), but
    // it CAN have overloads by arity. Goto-def picks the decl of the overload
    // the call's arity selects -- the same `select_overload` expansion uses.
    let path = "/probe/overload.rock";
    let src = "macro pick(a: expr) {\n\
               \x20   let _ = a\n\
               }\n\
               macro pick(a: expr, b: expr) {\n\
               \x20   let _ = a\n\
               \x20   let _ = b\n\
               }\n\
               fn main() {\n\
               \x20   pick(1)\n\
               \x20   pick(1, 2)\n\
               }\n";

    let mut bone = BoneWorkspace::new();
    bone.update(path, src.to_string());

    // 1-arg call -> the 1-arg macro decl (`macro pick` name at char 6, line 0).
    let one = bone
        .definition(path, pos(src, "pick(1)", 2))
        .expect("goto-def on the 1-arg macro call");
    assert_eq!(
        (one.range.start.line, one.range.start.character),
        (0, 6),
        "1-arg call must resolve to the 1-arg overload decl"
    );

    // 2-arg call -> the 2-arg macro decl (`macro pick` name at char 6, line 3).
    let two = bone
        .definition(path, pos(src, "pick(1, 2)", 2))
        .expect("goto-def on the 2-arg macro call");
    assert_eq!(
        (two.range.start.line, two.range.start.character),
        (3, 6),
        "2-arg call must resolve to the 2-arg overload decl"
    );
}
