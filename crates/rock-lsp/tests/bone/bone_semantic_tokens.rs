//! Tokens must decode against the legend the server advertises, or every
//! client miscolors.

use crate::common::{bone_ws, uri};
use lsp_types::*;
use rock_lsp::semantic_tokens::{compute_semantic_tokens_workspace, get_semantic_tokens_legend};

fn tokens(src: &str) -> Vec<SemanticToken> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.rock");
    let mut ws = bone_ws(&path, src);
    compute_semantic_tokens_workspace(&mut ws, &uri(&path))
        .unwrap()
        .data
}

/// Decode against the advertised legend, as the editor does.
#[test]
fn tokens_decode_against_the_advertised_legend() {
    let legend = get_semantic_tokens_legend();
    let nullable = "fn f(x: int?) -> int {\n  log(x)\n  return 0\n}\n";
    let refined = "fn f(x: int?) -> int {\n  if x != null {\n    return x\n  }\n  return 0\n}\n";
    for (source, line, col, kind, modifiers) in [
        (
            "fn foo() {}\n",
            0,
            0,
            Some(SemanticTokenType::KEYWORD),
            &[][..],
        ),
        (
            "fn foo() {}\n",
            0,
            3,
            Some(SemanticTokenType::FUNCTION),
            &[],
        ),
        (
            "struct Pt { x: int }\n",
            0,
            7,
            Some(SemanticTokenType::STRUCT),
            &[],
        ),
        (nullable, 0, 5, None, &["nullable"]),
        (refined, 2, 11, None, &["refined"]),
    ] {
        let tokens = crate::fixture::decode_tokens(&tokens(source));
        let (_, _, token) = tokens
            .iter()
            .find(|(l, c, _)| (*l, *c) == (line, col))
            .unwrap_or_else(|| panic!("no token at {line}:{col} in {source}"));
        if let Some(kind) = kind {
            assert_eq!(
                legend.token_types[token.token_type as usize], kind,
                "{source}"
            );
        }
        let decoded: Vec<_> = legend
            .token_modifiers
            .iter()
            .enumerate()
            .filter(|(i, _)| token.token_modifiers_bitset & (1 << i) != 0)
            .map(|(_, m)| m.as_str())
            .collect();
        assert_eq!(decoded, modifiers, "{source} {line}:{col}");
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
