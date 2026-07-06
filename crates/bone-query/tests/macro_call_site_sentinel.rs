// PINNED BUG: a macro call site whose EXPANSION begins with a variable /
// global / enum-variant reference from the macro TEMPLATE colors as that
// reference (modifiers included), not as MACRO.
//
// Mechanics: semantic tokens classify raw-source ident tokens HIR-first (by
// span-start offset), falling back to the workspace name map where macro
// names live. Macro expansion replaces the call expr with the template body
// and `retag_block` stamps the WHOLE invocation's span onto every expanded
// template node -- so a template-side `Global(SPEED)` ends up with a span
// whose START is the call-site ident's first byte. Its map entry
// (variable+static) then beats the name map's MACRO at exactly that offset.
// Argument substitutions keep their caller spans, so `m(x)`-shaped macros
// whose bodies only use args/literals are unaffected (covered green in
// semantic_tokens.rs::macro_def_name_and_call_site_color_macro).
//
// The assertions are INVERTED: they pass today because the bug exists. If
// they fail with MACRO / empty modifiers, a fix landed (e.g. the Session
// capturing macro CALL spans at parse time as an authoritative overlay, the
// way it already captures defs) -- flip them and fold this into the macro
// cases in semantic_tokens.rs.

use bone_query::{semantic_tokens_legend, semantic_tokens_modifiers_legend, BoneWorkspace};
use lsp_types::{SemanticTokenModifier, SemanticTokenType};

const PATH: &str = "/probe/main.rock";

#[test]
fn macro_call_site_shadowed_by_retagged_template_global() {
    // The template body references a GLOBAL (not an argument).
    let src = "global SPEED: int = 10\n\
               macro get_speed() -> int {\n  SPEED\n}\n\
               fn f() -> int {\n  return get_speed()\n}\n";
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    let legend = semantic_tokens_legend();
    let mods_legend = semantic_tokens_modifiers_legend();

    // Find the token at line 5 col 9 (`get_speed` in `return get_speed()`).
    let (mut line, mut col) = (0u32, 0u32);
    let mut hit = None;
    for t in bone.semantic_tokens(PATH).data {
        if t.delta_line != 0 {
            line += t.delta_line;
            col = t.delta_start;
        } else {
            col += t.delta_start;
        }
        if (line, col) == (5, 9) {
            let mods: Vec<SemanticTokenModifier> = mods_legend
                .iter()
                .enumerate()
                .filter(|(i, _)| t.token_modifiers_bitset & (1 << i) != 0)
                .map(|(_, m)| m.clone())
                .collect();
            hit = Some((legend[t.token_type as usize].clone(), mods));
        }
    }
    let (ty, mods) = hit.expect("a token at the macro call site");

    assert_eq!(
        ty,
        SemanticTokenType::VARIABLE,
        "PINNED BUG: the call site colors as the retagged template global \
         (VARIABLE), not MACRO. If this is MACRO now, the fix landed -- flip \
         both assertions and fold into semantic_tokens.rs"
    );
    assert_eq!(
        mods,
        vec![SemanticTokenModifier::STATIC],
        "PINNED BUG: the template global's `static` modifier leaks onto the \
         call site (renders as a bold mutable global). Flip when fixed."
    );
}
