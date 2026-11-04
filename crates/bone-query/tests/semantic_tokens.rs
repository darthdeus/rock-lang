//! Semantic-token classification richness: cross-file type names color as
//! types, field accesses as `property` (not bare `variable`).

use bone_query::{BoneWorkspace, semantic_tokens_legend, semantic_tokens_modifiers_legend};
use lsp_types::{Position, Range, SemanticToken, SemanticTokenModifier, SemanticTokenType};

/// `semantic_tokens_range` must agree with `semantic_tokens` over the whole file
/// (same tokens, same delta encoding) and emit strictly fewer for a viewport --
/// it's the `textDocument/semanticTokens/range` path nvim uses for open/scroll.
#[test]
fn range_matches_full_over_whole_file_and_subsets_for_a_viewport() {
    let src = "struct FixVec2 { x: int }\nfn a() -> int { let q = 1\n  q }\nfn b() -> int { let w = 2\n  w }\n";
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    let full = bone.semantic_tokens(PATH).data;
    let whole = Range {
        start: Position::new(0, 0),
        end: Position::new(100, 0),
    };
    assert_eq!(
        bone.semantic_tokens_range(PATH, whole).data,
        full,
        "range over the whole file must be byte-identical to full"
    );
    let top = Range {
        start: Position::new(0, 0),
        end: Position::new(2, 0),
    };
    let rng_top = bone.semantic_tokens_range(PATH, top).data;
    assert!(
        !rng_top.is_empty(),
        "a non-empty viewport must still produce tokens"
    );
    assert!(
        rng_top.len() < full.len(),
        "a 2-line viewport must emit fewer tokens than the whole file"
    );
}

const PATH: &str = "/tmp/rock_lsp_bone_semtok.rock";

/// One decoded semantic token: absolute 0-based `(line, char-col)`, its char
/// length, the legend index of its type and its raw modifier bitset.
///
/// The per-kind tests project type or modifiers out of this adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DecodedToken {
    line: u32,
    col: u32,
    length: u32,
    token_type: u32,
    modifiers: u32,
}

/// Walk a delta-encoded stream into absolute tokens.
fn decode_tokens(tokens: Vec<SemanticToken>) -> Vec<DecodedToken> {
    let (mut line, mut col) = (0u32, 0u32);
    tokens
        .into_iter()
        .map(|t| {
            if t.delta_line != 0 {
                line += t.delta_line;
                col = t.delta_start;
            } else {
                col += t.delta_start;
            }
            DecodedToken {
                line,
                col,
                length: t.length,
                token_type: t.token_type,
                modifiers: t.token_modifiers_bitset,
            }
        })
        .collect()
}

/// Decode a file's full token stream through an existing workspace.
fn decode_workspace(bone: &BoneWorkspace, path: &str) -> Vec<DecodedToken> {
    decode_tokens(bone.semantic_tokens(path).data)
}

/// Decode the delta-encoded tokens to `(line, col, type)`.
fn decode(src: &str) -> Vec<(u32, u32, SemanticTokenType)> {
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    let legend = semantic_tokens_legend();
    decode_workspace(&bone, PATH)
        .into_iter()
        .map(|t| (t.line, t.col, legend[t.token_type as usize].clone()))
        .collect()
}

fn type_at(toks: &[(u32, u32, SemanticTokenType)], pos: Position) -> Option<SemanticTokenType> {
    toks.iter()
        .find(|(l, c, _)| *l == pos.line && *c == pos.character)
        .map(|(_, _, t)| t.clone())
}

#[test]
fn struct_reference_colors_as_struct_not_variable() {
    let src =
        "struct FixVec2 { x: int, y: int }\nfn run() {\n  let v = FixVec2 { x: 1, y: 2 }\n}\n";
    let toks = decode(src);
    // `FixVec2` in the literal head, line 2 col 10.
    assert_eq!(
        type_at(&toks, Position::new(2, 10)),
        Some(SemanticTokenType::STRUCT),
        "struct reference should be STRUCT, got {:?}",
        type_at(&toks, Position::new(2, 10))
    );
}

#[test]
fn field_access_colors_as_property_not_variable() {
    let src = "struct FixVec2 { x: int, y: int }\nfn run() {\n  let v = FixVec2 { x: 1, y: 2 }\n  let n = v.x\n}\n";
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
    let src = "struct FixVec2 { x: int }\nfn f(a: FixVec2) -> int {\n  a.x\n}\n";
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
    let src =
        "struct FixVec2 { x: int, y: int }\nfn run() {\n  let v = FixVec2 { x: 1, y: 2 }\n}\n";
    let toks = decode(src);
    // field DECL `x` (line 0 col 17) and literal field name `x` (line 2 col 20).
    assert_eq!(
        type_at(&toks, Position::new(0, 17)),
        Some(SemanticTokenType::PROPERTY),
        "field decl, got {:?}",
        type_at(&toks, Position::new(0, 17))
    );
    assert_eq!(
        type_at(&toks, Position::new(2, 20)),
        Some(SemanticTokenType::PROPERTY),
        "struct-literal field name, got {:?}",
        type_at(&toks, Position::new(2, 20))
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
    decode_workspace(&bone, PATH)
        .into_iter()
        .map(|t| {
            let mods = mods_legend
                .iter()
                .enumerate()
                .filter(|(i, _)| t.modifiers & (1 << i) != 0)
                .map(|(_, m)| m.clone())
                .collect();
            (t.line, t.col, legend[t.token_type as usize].clone(), mods)
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
    let toks: Vec<_> = decode_workspace(&bone, bp)
        .into_iter()
        .map(|t| (t.line, t.col, legend[t.token_type as usize].clone()))
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
    // name map for known function names). a receiver call reported it as
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

/// A `.field` key-path literal (`FieldRef<T,V>` arg) colors its field name as
/// PROPERTY, same as a receiver field access -- the leading `.` marks it a
/// member in the token stream, and the HIR SiblingRef must not override that
/// to `variable`.
#[test]
fn field_ref_key_path_colors_field_as_property() {
    let src = "struct Mob { health: int, attack_co: int? }\n\
               fn poll_slot(f: FieldRef<Mob, int?>) {}\n\
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

// ---------------------------------------------------------------------------
// Globals: distinct from locals (bare `variable`) via a modifier -- `const`
// declarations carry `readonly`, mutable `global`s carry `static` (readonly
// on a mutable global would be a lie). The nvim plugin bolds both
// `@lsp.typemod.variable.readonly.rock` and `.static.rock`, so either way
// HOT_RELOAD / config globals visually pop against local variables.
// ---------------------------------------------------------------------------

#[test]
fn global_decl_and_use_carry_static_locals_do_not() {
    let stat = SemanticTokenModifier::STATIC;
    let src = "global SPEED: int = 10\n\
               fn f() -> int {\n  let local_v = SPEED\n  return local_v\n}\n";
    let toks = decode_full(src);
    // decl `SPEED` at line 0 col 7.
    let decl = mods_at(&toks, Position::new(0, 7));
    assert_eq!(
        decl,
        Some(vec![stat.clone()]),
        "mutable global decl should carry `static`, got {decl:?}"
    );
    // use `SPEED` at line 2 col 16 -- also VARIABLE + static.
    let use_mods = mods_at(&toks, Position::new(2, 16));
    assert_eq!(
        use_mods,
        Some(vec![stat]),
        "mutable global use should carry `static`, got {use_mods:?}"
    );
    let toks_ty = decode(src);
    assert_eq!(
        type_at(&toks_ty, Position::new(2, 16)),
        Some(SemanticTokenType::VARIABLE),
        "global use stays VARIABLE (the modifier is the distinguisher)"
    );
    // local decl + use carry no modifiers.
    assert_eq!(
        mods_at(&toks, Position::new(2, 6)),
        Some(vec![]),
        "local decl unmodified"
    );
    assert_eq!(
        mods_at(&toks, Position::new(3, 9)),
        Some(vec![]),
        "local use unmodified"
    );
}

#[test]
fn const_decl_and_use_carry_readonly() {
    let readonly = SemanticTokenModifier::READONLY;
    let src = "const LIMIT: int = 10\nfn f() -> int {\n  return LIMIT\n}\n";
    let toks = decode_full(src);
    // decl `LIMIT` at line 0 col 6; use at line 2 col 9.
    let decl = mods_at(&toks, Position::new(0, 6));
    assert_eq!(
        decl,
        Some(vec![readonly.clone()]),
        "const decl should carry `readonly`, got {decl:?}"
    );
    let use_mods = mods_at(&toks, Position::new(2, 9));
    assert_eq!(
        use_mods,
        Some(vec![readonly]),
        "const use should carry `readonly`, got {use_mods:?}"
    );
}

#[test]
fn nullable_global_decl_carries_static_and_nullable() {
    let stat = SemanticTokenModifier::STATIC;
    let nullable = SemanticTokenModifier::new("nullable");
    let src = "global TARGET: int? = null\nfn f() {\n  log(TARGET)\n}\n";
    let toks = decode_full(src);
    // decl `TARGET` at line 0 col 7: both modifiers (legend bit order).
    let decl = mods_at(&toks, Position::new(0, 7));
    assert_eq!(
        decl,
        Some(vec![nullable.clone(), stat.clone()]),
        "nullable global decl should carry `nullable` + `static`, got {decl:?}"
    );
    // unrefined use `TARGET` in `log(TARGET)` at line 2 col 6.
    let use_mods = mods_at(&toks, Position::new(2, 6));
    assert_eq!(
        use_mods,
        Some(vec![nullable, stat]),
        "unrefined nullable global use should carry `nullable` + `static`, got {use_mods:?}"
    );
}

#[test]
fn narrowed_global_use_carries_refined_and_static() {
    let stat = SemanticTokenModifier::STATIC;
    let refined = SemanticTokenModifier::new("refined");
    let src = "global TARGET: int? = null\n\
               fn f() -> int {\n  if TARGET != null {\n    return TARGET\n  }\n  return 0\n}\n";
    let toks = decode_full(src);
    // `TARGET` inside the null-checked block, line 3 col 11: narrowed to `int`.
    let use_mods = mods_at(&toks, Position::new(3, 11));
    assert_eq!(
        use_mods,
        Some(vec![refined, stat]),
        "narrowed global use should carry `refined` + `static`, got {use_mods:?}"
    );
}

// ---------------------------------------------------------------------------
// Enum members: variant names color ENUM_MEMBER in every position (decl,
// qualified expr, bare expr, match pattern) -- previously bare `variable`.
// ---------------------------------------------------------------------------

#[test]
fn enum_variant_decl_and_expr_use_color_enum_member() {
    let src = "enum Color { Red, Green }\nfn f() -> Color {\n  return Color::Red\n}\n";
    let toks = decode(src);
    // decl `Red` at line 0 col 13, `Green` at col 18.
    assert_eq!(
        type_at(&toks, Position::new(0, 13)),
        Some(SemanticTokenType::ENUM_MEMBER),
        "variant decl should be ENUM_MEMBER, got {:?}",
        type_at(&toks, Position::new(0, 13))
    );
    // qualified use: `Color::Red` on line 2 -- `Color` col 9 (ENUM), `Red` col 16.
    assert_eq!(
        type_at(&toks, Position::new(2, 9)),
        Some(SemanticTokenType::ENUM)
    );
    assert_eq!(
        type_at(&toks, Position::new(2, 16)),
        Some(SemanticTokenType::ENUM_MEMBER),
        "qualified variant use should be ENUM_MEMBER, got {:?}",
        type_at(&toks, Position::new(2, 16))
    );
}

#[test]
fn enum_variant_in_match_pattern_colors_enum_member() {
    let src = "enum Color { Red, Green }\n\
               fn f(c: Color) -> int {\n  return match c {\n    Color::Red => 1\n    Color::Green => 2\n  }\n}\n";
    let toks = decode(src);
    // pattern `Color::Red` on line 3: `Red` at col 11.
    assert_eq!(
        type_at(&toks, Position::new(3, 11)),
        Some(SemanticTokenType::ENUM_MEMBER),
        "pattern variant should be ENUM_MEMBER, got {:?}",
        type_at(&toks, Position::new(3, 11))
    );
    assert_eq!(
        type_at(&toks, Position::new(4, 11)),
        Some(SemanticTokenType::ENUM_MEMBER),
        "second arm variant should be ENUM_MEMBER, got {:?}",
        type_at(&toks, Position::new(4, 11))
    );
}

// ---------------------------------------------------------------------------
// String-interpolation holes: `"v={x}"` colors the hole expression instead of
// rendering the whole literal as one flat STRING token. Literal segments
// (incl. the braces) stay STRING; the hole colors as the expression's own
// classification (param/variable/global-with-modifiers).
// ---------------------------------------------------------------------------

#[test]
fn interpolation_hole_colors_as_the_expression() {
    let src = "fn f(x: int) {\n  let s = \"v={x} w={x}\"\n}\n";
    let toks = decode(src);
    // line 1: `  let s = "v={x} w={x}"` -- first `x` col 14, second col 20.
    assert_eq!(
        type_at(&toks, Position::new(1, 14)),
        Some(SemanticTokenType::PARAMETER),
        "hole `x` should color as PARAMETER, got {:?}",
        type_at(&toks, Position::new(1, 14))
    );
    assert_eq!(
        type_at(&toks, Position::new(1, 20)),
        Some(SemanticTokenType::PARAMETER),
        "second hole `x` should color as PARAMETER, got {:?}",
        type_at(&toks, Position::new(1, 20))
    );
    // The literal segments around the holes stay STRING: opening `"v={` at
    // col 10, middle `} w={` at col 15, closing `}"` at col 21.
    for col in [10, 15, 21] {
        assert_eq!(
            type_at(&toks, Position::new(1, col)),
            Some(SemanticTokenType::STRING),
            "segment at col {col} should stay STRING, got {:?}",
            type_at(&toks, Position::new(1, col))
        );
    }
}

#[test]
fn interpolation_hole_global_carries_modifiers() {
    let stat = SemanticTokenModifier::STATIC;
    let src = "global SPEED: int = 10\nfn f() {\n  let s = \"speed={SPEED}\"\n}\n";
    let toks = decode_full(src);
    // line 2: `  let s = "speed={SPEED}"` -- SPEED hole at col 18.
    let mods = mods_at(&toks, Position::new(2, 18));
    assert_eq!(
        mods,
        Some(vec![stat]),
        "global in a hole should carry `static`, got {mods:?}"
    );
}

// ---------------------------------------------------------------------------
// Generic type parameters: `<T>` decls and `T` uses in type positions color
// TYPE_PARAMETER -- previously bare `variable`.
// ---------------------------------------------------------------------------

#[test]
fn fn_generic_decl_and_uses_color_type_parameter() {
    let src = "fn id<T>(x: T) -> T {\n  return x\n}\n";
    let toks = decode(src);
    // `fn id<T>(x: T) -> T` -- decl T col 6, param-type T col 12, return-type T col 18.
    for col in [6, 12, 18] {
        assert_eq!(
            type_at(&toks, Position::new(0, col)),
            Some(SemanticTokenType::TYPE_PARAMETER),
            "generic T at col {col} should be TYPE_PARAMETER, got {:?}",
            type_at(&toks, Position::new(0, col))
        );
    }
}

#[test]
fn struct_generic_decl_and_field_use_color_type_parameter() {
    let src = "struct Box<T> { v: T }\nfn f(b: Box<int>) {}\n";
    let toks = decode(src);
    // `struct Box<T> { v: T }` -- decl T col 11, field-type use T col 19.
    for col in [11, 19] {
        assert_eq!(
            type_at(&toks, Position::new(0, col)),
            Some(SemanticTokenType::TYPE_PARAMETER),
            "struct generic T at col {col} should be TYPE_PARAMETER, got {:?}",
            type_at(&toks, Position::new(0, col))
        );
    }
}

// ---------------------------------------------------------------------------
// Decorators / annotations: `@name` idents and `deco` def names color as
// DECORATOR. The `@` trailing-lambda operator must NOT catch its operand.
// ---------------------------------------------------------------------------

#[test]
fn annotation_and_deco_def_name_color_decorator() {
    let src = "deco init capture {\n}\n@capture\nfn f() {}\n@nomangle\nfn g() {}\n";
    let toks = decode(src);
    // `capture` in the def header, line 0 col 10.
    assert_eq!(
        type_at(&toks, Position::new(0, 10)),
        Some(SemanticTokenType::DECORATOR),
        "deco def name should be DECORATOR, got {:?}",
        type_at(&toks, Position::new(0, 10))
    );
    // `@capture` use, ident at line 2 col 1.
    assert_eq!(
        type_at(&toks, Position::new(2, 1)),
        Some(SemanticTokenType::DECORATOR),
        "annotation ident should be DECORATOR, got {:?}",
        type_at(&toks, Position::new(2, 1))
    );
    // builtin annotation `@nomangle`, line 4 col 1.
    assert_eq!(
        type_at(&toks, Position::new(4, 1)),
        Some(SemanticTokenType::DECORATOR),
        "builtin annotation should be DECORATOR, got {:?}",
        type_at(&toks, Position::new(4, 1))
    );
}

#[test]
fn at_operator_operand_does_not_color_decorator() {
    let src = "fn run(g: fn() -> int) -> int { g() }\nfn f(x: int) {\n  let s = run() @ x\n}\n";
    let toks = decode(src);
    // `x` after the trailing-lambda `@`, line 2 col 18 -- a PARAMETER use.
    assert_eq!(
        type_at(&toks, Position::new(2, 18)),
        Some(SemanticTokenType::PARAMETER),
        "@-operator operand should stay PARAMETER, got {:?}",
        type_at(&toks, Position::new(2, 18))
    );
}

// ---------------------------------------------------------------------------
// Macros: def names and call sites color MACRO. Macros are expanded away
// before bone lowering, so the names come from the Session's parse-time
// capture (UserFile.macros), not the workspace HIR.
// ---------------------------------------------------------------------------

#[test]
fn macro_def_name_and_call_site_color_macro() {
    let src = "macro twice(x: int) -> int {\n  x * 2\n}\nfn f() -> int {\n  return twice(3)\n}\n";
    let toks = decode(src);
    // def name `twice` at line 0 col 6.
    assert_eq!(
        type_at(&toks, Position::new(0, 6)),
        Some(SemanticTokenType::MACRO),
        "macro def name should be MACRO, got {:?}",
        type_at(&toks, Position::new(0, 6))
    );
    // call site `twice` at line 4 col 9.
    assert_eq!(
        type_at(&toks, Position::new(4, 9)),
        Some(SemanticTokenType::MACRO),
        "macro call should be MACRO, got {:?}",
        type_at(&toks, Position::new(4, 9))
    );
}

#[test]
fn static_keyword_colors_as_keyword_not_variable() {
    // `static` is a contextual keyword (lexed as an ident, recognized before the
    // binding name). It must color as KEYWORD, not VARIABLE like the binding.
    let src = "fn counter() -> int {\n    static n = 0\n    return n\n}\n";
    let toks = decode(src);
    assert_eq!(
        type_at(&toks, Position::new(1, 4)),
        Some(SemanticTokenType::KEYWORD),
        "the `static` keyword must color as keyword: {toks:?}"
    );
}

#[test]
fn named_arg_labels_color_as_parameter_not_a_colliding_function() {
    // A named-argument label (`width:` in `make(width: 3, ..)`) names a
    // PARAMETER, and must color as one consistently -- even when a same-named
    // global fn exists (`fn width()`), which the workspace name-map would
    // otherwise claim as FUNCTION. Pre-fix: `width:` colored FUNCTION while
    // `height:` colored VARIABLE, so the labels were inconsistent (a struct initializer
    // trail_new call: `width` differed from the rest). Both must be PARAMETER.
    let src = "fn width() -> int { return 5 }\n\
               fn make(width: int, height: int) -> int { return width + height }\n\
               fn r() -> int {\n    return make(\n        width: 3,\n        height: 4,\n    )\n}\n";
    let toks = decode(src);
    assert_eq!(
        type_at(&toks, Position::new(4, 8)),
        Some(SemanticTokenType::PARAMETER),
        "named-arg label `width:` must be PARAMETER, not the colliding fn: {toks:?}"
    );
    assert_eq!(
        type_at(&toks, Position::new(5, 8)),
        Some(SemanticTokenType::PARAMETER),
        "named-arg label `height:` must be PARAMETER: {toks:?}"
    );
}

#[test]
fn magic_rock_directive_comment_colors_as_macro_not_comment() {
    // A `// rock: allow(...)` directive comment is highlighted as MACRO so
    // themes distinguish it from a plain comment; an ordinary comment stays
    // COMMENT.
    let src = "fn run() {\n  // rock: allow(unused_var)\n  // just a note\n  let x = 1\n}\n";
    let toks = decode(src);
    assert_eq!(
        type_at(&toks, Position::new(1, 2)),
        Some(SemanticTokenType::MACRO),
        "magic rock directive should be MACRO, got {:?}",
        type_at(&toks, Position::new(1, 2))
    );
    assert_eq!(
        type_at(&toks, Position::new(2, 2)),
        Some(SemanticTokenType::COMMENT),
        "a plain comment stays COMMENT, got {:?}",
        type_at(&toks, Position::new(2, 2))
    );
}
