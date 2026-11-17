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
use rock_lsp::semantic_tokens::{compute_semantic_tokens_workspace, get_semantic_tokens_legend};

fn tokens(src: &str) -> Vec<SemanticToken> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.rock");
    let mut ws = crate::common::bone_ws(&path, src);
    let uri = Uri::from_file_path(path.to_str().unwrap()).unwrap();
    compute_semantic_tokens_workspace(&mut ws, &uri)
        .unwrap()
        .data
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

#[test]
fn modifiers_remap_to_the_advertised_legend() {
    for (source, line, col, modifier) in [
        (
            "fn f(x: int?) -> int {\n  log(x)\n  return 0\n}\n",
            0,
            5,
            "nullable",
        ),
        (
            "fn f(x: int?) -> int {\n  if x != null {\n    return x\n  }\n  return 0\n}\n",
            2,
            11,
            "refined",
        ),
    ] {
        let tokens = crate::fixture::decode_tokens(&tokens(source));
        let (_, _, token) = tokens
            .iter()
            .find(|(l, c, _)| (*l, *c) == (line, col))
            .expect("target token");
        assert_eq!(decoded_mods(token), [SemanticTokenModifier::new(modifier)]);
    }
}

#[test]
fn token_types_decode_against_the_advertised_legend() {
    for (source, kind) in [
        ("fn foo() {}\n", SemanticTokenType::FUNCTION),
        ("struct Pt { x: int }\n", SemanticTokenType::STRUCT),
    ] {
        let tokens = tokens(source);
        assert!(!tokens.is_empty(), "expected tokens for {source}");
        assert_eq!(decoded(&tokens[0]), SemanticTokenType::KEYWORD);
        assert_eq!(decoded(&tokens[1]), kind);
    }
}

/// A delta must reproduce the new array when applied the way a client does,
/// when the edit changes the number of lines and tokens.
#[test]
fn delta_edits_apply_across_a_multi_line_edit() {
    use rock_lsp::semantic_tokens::semantic_tokens_delta_edits;

    let before = "fn f() {\n    /* a\n       b */\n    let x = 1\n    print(x)\n}\n";
    let after = "fn f() {\n    let inserted = 2\n    /* a\n    b\n    c */\n    let x = 1\n\n    print(x + inserted)\n}\n";
    for (old, new) in [
        (tokens(before), tokens(after)),
        (tokens(after), tokens(before)),
    ] {
        let flatten = |toks: &[SemanticToken]| -> Vec<u32> {
            toks.iter()
                .flat_map(|t| {
                    [
                        t.delta_line,
                        t.delta_start,
                        t.length,
                        t.token_type,
                        t.token_modifiers_bitset,
                    ]
                })
                .collect()
        };
        let mut applied = flatten(&old);
        for edit in semantic_tokens_delta_edits(&old, &new).into_iter().rev() {
            let start = edit.start as usize;
            let inserted = flatten(&edit.data.unwrap_or_default());
            applied.splice(start..start + edit.delete_count as usize, inserted);
        }
        assert_eq!(applied, flatten(&new));
    }
}
