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

use bone_query::BoneWorkspace;
use lsp_types::Position;

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
fn def_shared_macro_and_fn_name_resolves_by_arity() {
    // A 2-arg macro and a 3-arg fn share the name (mirrors `run_into`). The
    // 3-arg call is a FN call -- goto-def must fall through to the fn decl, not
    // claim the macro (the registry arity check returns None for argc=3).
    let path = "/probe/arity.rock";
    let src = "macro pick(a: expr, b: expr) {\n\
               \x20   let _ = a\n\
               \x20   let _ = b\n\
               }\n\
               fn pick(a: int, b: int, c: int) -> int { return a + b + c }\n\
               fn main() {\n\
               \x20   let _ = pick(1, 2, 3)\n\
               }\n";

    let mut bone = BoneWorkspace::new();
    bone.update(path, src.to_string());

    // The 3-arg call -> the fn decl (`fn pick` name at char 3 on line 4).
    let fn_call = bone
        .definition(path, pos(src, "pick(1, 2, 3)", 2))
        .expect("goto-def on the 3-arg fn call");
    assert_eq!(
        (fn_call.range.start.line, fn_call.range.start.character),
        (4, 3),
        "the 3-arg call is a fn call -- must resolve to `fn pick`, not the macro"
    );
}
