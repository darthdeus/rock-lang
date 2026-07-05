//! Semantic-token classification richness: cross-file type names color as
//! types, field accesses as `property` (not bare `variable`).

use bone_query::{semantic_tokens_legend, semantic_tokens_modifiers_legend, BoneWorkspace};
use lsp_types::{Position, Range, SemanticTokenModifier, SemanticTokenType};

/// `semantic_tokens_range` must agree with `semantic_tokens` over the whole file
/// (same tokens, same delta encoding) and emit strictly fewer for a viewport --
/// it's the `textDocument/semanticTokens/range` path nvim uses for open/scroll.
#[test]
fn range_matches_full_over_whole_file_and_subsets_for_a_viewport() {
    let src = "struct Vec2 { x: int }\nfn a() -> int { let q = 1\n  q }\nfn b() -> int { let w = 2\n  w }\n";
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    let full = bone.semantic_tokens(PATH).data;
    let whole = Range { start: Position::new(0, 0), end: Position::new(100, 0) };
    assert_eq!(
        bone.semantic_tokens_range(PATH, whole).data,
        full,
        "range over the whole file must be byte-identical to full"
    );
    let top = Range { start: Position::new(0, 0), end: Position::new(2, 0) };
    let rng_top = bone.semantic_tokens_range(PATH, top).data;
    assert!(!rng_top.is_empty(), "a non-empty viewport must still produce tokens");
    assert!(rng_top.len() < full.len(), "a 2-line viewport must emit fewer tokens than the whole file");
}

const PATH: &str = "/tmp/rock_lsp_bone_semtok.rock";

/// Decode the delta-encoded tokens to `(line, col, type)`.
fn decode(src: &str) -> Vec<(u32, u32, SemanticTokenType)> {
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    let legend = semantic_tokens_legend();
    let (mut line, mut col) = (0u32, 0u32);
    bone.semantic_tokens(PATH)
        .data
        .into_iter()
        .map(|t| {
            if t.delta_line != 0 {
                line += t.delta_line;
                col = t.delta_start;
            } else {
                col += t.delta_start;
            }
            (line, col, legend[t.token_type as usize].clone())
        })
        .collect()
}

fn type_at(toks: &[(u32, u32, SemanticTokenType)], pos: Position) -> Option<SemanticTokenType> {
    toks.iter()
        .find(|(l, c, _)| *l == pos.line && *c == pos.character)
        .map(|(_, _, t)| t.clone())
}

#[test]
fn struct_reference_colors_as_struct_not_variable() {
    let src = "struct Vec2 { x: int, y: int }\nfn run() {\n  let v = Vec2 { x: 1, y: 2 }\n}\n";
    let toks = decode(src);
    // `Vec2` in the literal head, line 2 col 10.
    assert_eq!(
        type_at(&toks, Position::new(2, 10)),
        Some(SemanticTokenType::STRUCT),
        "struct reference should be STRUCT, got {:?}",
        type_at(&toks, Position::new(2, 10))
    );
}

#[test]
fn field_access_colors_as_property_not_variable() {
    let src = "struct Vec2 { x: int, y: int }\nfn run() {\n  let v = Vec2 { x: 1, y: 2 }\n  let n = v.x\n}\n";
    let toks = decode(src);
    // `x` after `v.` on line 3: "  let n = v.x" -> x at col 12.
    assert_eq!(
        type_at(&toks, Position::new(3, 12)),
        Some(SemanticTokenType::PROPERTY),
        "field access should be PROPERTY, got {:?}",
        type_at(&toks, Position::new(3, 12))
    );
}

#[test]
fn parameter_colors_as_parameter_at_decl_and_use() {
    let src = "struct Vec2 { x: int }\nfn f(a: Vec2) -> int {\n  a.x\n}\n";
    let toks = decode(src);
    // param decl `a` (line 1 col 5) and its use `a` (line 2 col 2).
    assert_eq!(
        type_at(&toks, Position::new(1, 5)),
        Some(SemanticTokenType::PARAMETER),
        "param decl, got {:?}",
        type_at(&toks, Position::new(1, 5))
    );
    assert_eq!(
        type_at(&toks, Position::new(2, 2)),
        Some(SemanticTokenType::PARAMETER),
        "param use, got {:?}",
        type_at(&toks, Position::new(2, 2))
    );
}

#[test]
fn struct_field_names_color_as_property_at_decl_and_literal() {
    let src = "struct Vec2 { x: int, y: int }\nfn run() {\n  let v = Vec2 { x: 1, y: 2 }\n}\n";
    let toks = decode(src);
    // field DECL `x` (line 0 col 14) and literal field name `x` (line 2 col 17).
    assert_eq!(
        type_at(&toks, Position::new(0, 14)),
        Some(SemanticTokenType::PROPERTY),
        "field decl, got {:?}",
        type_at(&toks, Position::new(0, 14))
    );
    assert_eq!(
        type_at(&toks, Position::new(2, 17)),
        Some(SemanticTokenType::PROPERTY),
        "struct-literal field name, got {:?}",
        type_at(&toks, Position::new(2, 17))
    );
}

// ---------------------------------------------------------------------------
// Nullable / refined modifiers (parity with rock's semantic_tokens).
// A binding whose inferred type is `T?` carries the custom `nullable` modifier
// at its declaration and at any use where flow has NOT narrowed it; a use the
// flow has narrowed to `T` (inside `if x != null { .. }`) carries `refined`
// instead. The two are mutually exclusive on one token.
// ---------------------------------------------------------------------------

/// Decode tokens to `(line, col, type, modifier-set)`.
fn decode_full(src: &str) -> Vec<(u32, u32, SemanticTokenType, Vec<SemanticTokenModifier>)> {
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    let legend = semantic_tokens_legend();
    let mods_legend = semantic_tokens_modifiers_legend();
    let (mut line, mut col) = (0u32, 0u32);
    bone.semantic_tokens(PATH)
        .data
        .into_iter()
        .map(|t| {
            if t.delta_line != 0 {
                line += t.delta_line;
                col = t.delta_start;
            } else {
                col += t.delta_start;
            }
            let mods = mods_legend
                .iter()
                .enumerate()
                .filter(|(i, _)| t.token_modifiers_bitset & (1 << i) != 0)
                .map(|(_, m)| m.clone())
                .collect();
            (line, col, legend[t.token_type as usize].clone(), mods)
        })
        .collect()
}

fn mods_at(
    toks: &[(u32, u32, SemanticTokenType, Vec<SemanticTokenModifier>)],
    pos: Position,
) -> Option<Vec<SemanticTokenModifier>> {
    toks.iter()
        .find(|(l, c, _, _)| *l == pos.line && *c == pos.character)
        .map(|(_, _, _, m)| m.clone())
}

#[test]
fn nullable_param_decl_and_unrefined_use_carry_nullable() {
    let nullable = SemanticTokenModifier::new("nullable");
    // `x: int?` -- decl and a use outside any null-check are both `nullable`.
    let src = "fn f(x: int?) -> int {\n  log(x)\n  return 0\n}\n";
    let toks = decode_full(src);
    // param decl `x` at line 0 col 5.
    assert_eq!(
        mods_at(&toks, Position::new(0, 5)),
        Some(vec![nullable.clone()]),
        "nullable param decl should carry `nullable`, got {:?}",
        mods_at(&toks, Position::new(0, 5))
    );
    // unrefined use `x` inside `log(x)` at line 1 col 6.
    assert_eq!(
        mods_at(&toks, Position::new(1, 6)),
        Some(vec![nullable]),
        "unrefined nullable use should carry `nullable`, got {:?}",
        mods_at(&toks, Position::new(1, 6))
    );
}

#[test]
fn narrowed_use_carries_refined_not_nullable() {
    let refined = SemanticTokenModifier::new("refined");
    // Inside `if x != null`, the use of `x` is narrowed to `int` -> `refined`.
    let src = "fn f(x: int?) -> int {\n  if x != null {\n    return x\n  }\n  return 0\n}\n";
    let toks = decode_full(src);
    // narrowed use `x` in `return x` at line 2 col 11.
    assert_eq!(
        mods_at(&toks, Position::new(2, 11)),
        Some(vec![refined]),
        "narrowed nullable use should carry `refined` (not `nullable`), got {:?}",
        mods_at(&toks, Position::new(2, 11))
    );
}

#[test]
fn non_nullable_binding_carries_neither_modifier() {
    // A plain `int` param carries neither nullable nor refined.
    let src = "fn f(x: int) -> int {\n  return x\n}\n";
    let toks = decode_full(src);
    assert_eq!(
        mods_at(&toks, Position::new(0, 5)),
        Some(vec![]),
        "non-nullable param decl should carry no modifier, got {:?}",
        mods_at(&toks, Position::new(0, 5))
    );
    assert_eq!(
        mods_at(&toks, Position::new(1, 9)),
        Some(vec![]),
        "non-nullable param use should carry no modifier, got {:?}",
        mods_at(&toks, Position::new(1, 9))
    );
}

#[test]
fn nullable_let_binding_decl_carries_nullable() {
    let nullable = SemanticTokenModifier::new("nullable");
    let src = "fn f(x: int?) -> int {\n  let y: int? = x\n  return 0\n}\n";
    let toks = decode_full(src);
    // let-binding decl `y` at line 1 col 6.
    assert_eq!(
        mods_at(&toks, Position::new(1, 6)),
        Some(vec![nullable]),
        "nullable let decl should carry `nullable`, got {:?}",
        mods_at(&toks, Position::new(1, 6))
    );
}

#[test]
fn cross_file_struct_name_colors_as_struct() {
    // The struct is defined in another file of the project.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("stick.toml"), "[package]\nname = \"xp\"\n").unwrap();
    std::fs::write(dir.path().join("a.rock"), "struct Mob { hp: int }\n").unwrap();
    let bpath = dir.path().join("b.rock");
    let bsrc = "fn make() -> Mob {\n  Mob { hp: 1 }\n}\n";
    std::fs::write(&bpath, bsrc).unwrap();

    let mut bone = BoneWorkspace::new();
    let bp = bpath.to_str().unwrap();
    bone.update(bp, bsrc.to_string());
    let legend = semantic_tokens_legend();
    let (mut line, mut col) = (0u32, 0u32);
    let toks: Vec<_> = bone
        .semantic_tokens(bp)
        .data
        .into_iter()
        .map(|t| {
            if t.delta_line != 0 {
                line += t.delta_line;
                col = t.delta_start;
            } else {
                col += t.delta_start;
            }
            (line, col, legend[t.token_type as usize].clone())
        })
        .collect();
    // `Mob` return type on line 0 (col 13) -- defined in a.rock.
    assert_eq!(
        type_at(&toks, Position::new(0, 13)),
        Some(SemanticTokenType::STRUCT),
        "cross-file struct name should be STRUCT, got {:?}",
        type_at(&toks, Position::new(0, 13))
    );
}

#[test]
fn ufcs_method_call_colors_as_function_not_property() {
    // `m.resolve_strike()` -- a UFCS method call -- must color the method name
    // as FUNCTION, not PROPERTY (the `.`-member heuristic must defer to the
    // name map for known function names). mag `mobs.rock` reported it as
    // `property`.
    let src = "struct Mob { hp: int }\nfn resolve_strike(m: Mob) {}\nfn run(m: Mob) {\n  m.resolve_strike()\n}\n";
    let toks = decode(src);
    assert_eq!(
        type_at(&toks, Position::new(3, 4)),
        Some(SemanticTokenType::FUNCTION),
        "UFCS method call should be FUNCTION, got {:?}",
        type_at(&toks, Position::new(3, 4))
    );
}

#[test]
fn contextual_co_colors_as_keyword_before_fn_but_variable_otherwise() {
    // `co` is a contextual keyword: KEYWORD in `co fn`, a plain VARIABLE
    // identifier everywhere else (so coroutine values can be bound to `co`).
    let src = "co fn gen() -> Coroutine<int> {\n  yield 1\n}\nfn run() {\n  let co = gen()\n  let r = co.resume()\n}\n";
    let toks = decode(src);
    // `co` heading the declaration on line 0 col 0 -> KEYWORD.
    assert_eq!(
        type_at(&toks, Position::new(0, 0)),
        Some(SemanticTokenType::KEYWORD),
        "`co` before `fn` should be KEYWORD, got {:?}",
        type_at(&toks, Position::new(0, 0))
    );
    // `co` bound as a local on line 4 col 6 -> VARIABLE (not keyword).
    assert_eq!(
        type_at(&toks, Position::new(4, 6)),
        Some(SemanticTokenType::VARIABLE),
        "`co` as an identifier should be VARIABLE, got {:?}",
        type_at(&toks, Position::new(4, 6))
    );
}

/// A `.field` key-path literal (`FieldOf<T,V>` arg) colors its field name as
/// PROPERTY, same as a receiver field access -- the leading `.` marks it a
/// member in the token stream, and the HIR SiblingRef must not override that
/// to `variable`.
#[test]
fn field_ref_key_path_colors_field_as_property() {
    let src = "struct Mob { health: int, attack_co: int? }\n\
               fn poll_slot(f: FieldOf<Mob, int?>) {}\n\
               fn main() {\n  poll_slot(.attack_co)\n}\n";
    let toks = decode(src);
    // `attack_co` starts after `  poll_slot(.`
    let col = "  poll_slot(.".len() as u32;
    assert_eq!(
        type_at(&toks, Position::new(3, col)),
        Some(SemanticTokenType::PROPERTY),
        "key-path `.attack_co` should be PROPERTY, got {:?}",
        type_at(&toks, Position::new(3, col))
    );
}
