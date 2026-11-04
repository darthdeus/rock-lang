//! Bone semantic tokens must decode against the legend the server advertises.
//!
//! The server advertises ONE `SemanticTokensLegend` at initialize
//! (`get_semantic_tokens_legend`). bone classifies tokens into its own 9-type
//! legend and emits indices into THAT; if those indices aren't translated to the
//! advertised legend, the client decodes every token as the wrong type (bone's
//! `0`=KEYWORD reads as the advertised `0`=NAMESPACE, etc.) -> pervasive
//! miscoloring. The `.rock.lsp` fixtures miss this because they decode with
//! bone's own legend; only the live server hits the mismatch.

use lsp_types::*;
use rock_lsp::LspWorkspace;
use rock_lsp::semantic_tokens::{compute_semantic_tokens_workspace, get_semantic_tokens_legend};
use std::str::FromStr;

fn tokens(src: &str) -> Vec<SemanticToken> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.rock");
    std::fs::write(&path, src).unwrap();
    let mut ws = LspWorkspace::new();
    ws.bone.update(path.to_str().unwrap(), src.to_string());
    let params = SemanticTokensParams {
        text_document: TextDocumentIdentifier {
            uri: Uri::from_str(&format!("file://{}", path.display())).unwrap(),
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };
    match compute_semantic_tokens_workspace(&mut ws, params).unwrap() {
        Some(SemanticTokensResult::Tokens(t)) => t.data,
        other => panic!("expected token data, got {other:?}"),
    }
}

/// The `SemanticTokenType` a token decodes to under the advertised legend --
/// the same decode the editor performs.
fn decoded(tok: &SemanticToken) -> SemanticTokenType {
    get_semantic_tokens_legend().token_types[tok.token_type as usize].clone()
}

/// The set of `SemanticTokenModifier`s a token decodes to under the advertised
/// legend -- the same decode the editor performs. bone emits bits into its OWN
/// modifier legend (nullable=0, refined=1); the server must remap them to the
/// advertised legend (nullable=10, refined=11) or themes mis-style the token.
fn decoded_mods(tok: &SemanticToken) -> Vec<SemanticTokenModifier> {
    let legend = get_semantic_tokens_legend().token_modifiers;
    legend
        .iter()
        .enumerate()
        .filter(|(i, _)| tok.token_modifiers_bitset & (1 << i) != 0)
        .map(|(_, m)| m.clone())
        .collect()
}

/// Decode tokens to absolute `(line, col)` so a modifier assertion can target a
/// specific identifier.
fn abs(toks: &[SemanticToken]) -> Vec<(u32, u32, SemanticToken)> {
    let (mut line, mut col) = (0u32, 0u32);
    toks.iter()
        .map(|t| {
            if t.delta_line != 0 {
                line += t.delta_line;
                col = t.delta_start;
            } else {
                col += t.delta_start;
            }
            (line, col, t.clone())
        })
        .collect()
}

#[test]
fn nullable_modifier_remaps_to_advertised_legend() {
    // bone emits nullable on bit 0 of ITS legend; the server must remap it to the
    // advertised `nullable` modifier so the editor decodes it correctly.
    let toks = abs(&tokens("fn f(x: int?) -> int {\n  log(x)\n  return 0\n}\n"));
    let (_, _, decl) = toks
        .iter()
        .find(|(l, c, _)| *l == 0 && *c == 5)
        .expect("param decl token at 0:5");
    assert_eq!(
        decoded_mods(decl),
        vec![SemanticTokenModifier::new("nullable")],
        "nullable param decl must decode as `nullable` under the advertised legend"
    );
}

#[test]
fn refined_modifier_remaps_to_advertised_legend() {
    // Inside `if x != null`, the use is narrowed -> `refined`, remapped to the
    // advertised legend's `refined` modifier.
    let toks = abs(&tokens(
        "fn f(x: int?) -> int {\n  if x != null {\n    return x\n  }\n  return 0\n}\n",
    ));
    let (_, _, use_tok) = toks
        .iter()
        .find(|(l, c, _)| *l == 2 && *c == 11)
        .expect("narrowed use token at 2:11");
    assert_eq!(
        decoded_mods(use_tok),
        vec![SemanticTokenModifier::new("refined")],
        "narrowed nullable use must decode as `refined` under the advertised legend"
    );
}

#[test]
fn keyword_decodes_as_keyword_via_advertised_legend() {
    let toks = tokens("fn foo() {}\n");
    assert!(!toks.is_empty(), "expected tokens for `fn foo() {{}}`");
    // first token is the `fn` keyword
    assert_eq!(
        decoded(&toks[0]),
        SemanticTokenType::KEYWORD,
        "`fn` must decode as KEYWORD under the advertised legend"
    );
}

#[test]
fn function_name_decodes_as_function() {
    let toks = tokens("fn foo() {}\n");
    // second classified token is the function name `foo`
    assert_eq!(
        decoded(&toks[1]),
        SemanticTokenType::FUNCTION,
        "`foo` must decode as FUNCTION under the advertised legend"
    );
}

#[test]
fn struct_name_decodes_as_struct() {
    let toks = tokens("struct Pt { x: int }\n");
    // `struct` keyword then `Pt`
    assert_eq!(decoded(&toks[0]), SemanticTokenType::KEYWORD);
    assert_eq!(
        decoded(&toks[1]),
        SemanticTokenType::STRUCT,
        "`Pt` must decode as STRUCT under the advertised legend"
    );
}
