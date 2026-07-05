//! Data-driven completion RANKING corpus. Each case pins a specific scoring
//! outcome: a query at a cursor must surface / rank a particular candidate. This
//! is the safety net that lets the fuzzy score, the smart-case tiebreak, the
//! type-directed buckets, and the reachability expansion be tuned without silent
//! regressions -- add a case here whenever a "wrong thing ranked first" bug is
//! found. Cursor is marked with `$0` in the fixture.

use bone_query::BoneWorkspace;
use lsp_types::Position;

const PATH: &str = "/tmp/rock_lsp_ranking.rock";

/// Split a `$0`-marked source into (source, cursor Position).
fn marked(src: &str) -> (String, Position) {
    let off = src.find("$0").expect("fixture must contain the `$0` cursor marker");
    let before = &src[..off];
    let line = before.matches('\n').count() as u32;
    let col = (before.len() - before.rfind('\n').map(|i| i + 1).unwrap_or(0)) as u32;
    (src.replacen("$0", "", 1), Position::new(line, col))
}

fn labels_at(src: &str) -> Vec<String> {
    let (s, pos) = marked(src);
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, s);
    bone.completion(PATH, pos).into_iter().map(|c| c.label).collect()
}

/// Index of the first label equal to `needle`.
fn rank_of(labels: &[String], needle: &str) -> Option<usize> {
    labels.iter().position(|l| l == needle)
}

// -- Case penalty: a strong match on a non-lowercase name beats a weak match on
//    a lowercase one. Match quality leads; case only breaks near-ties. ---------

#[test]
fn prefix_match_on_pascal_name_beats_boundary_match_on_snake_name() {
    // `spellb` is a whole-prefix (+1000) of the struct `SpellBindingX` but only a
    // word-boundary match on the fn `spell_bank` (`spell_` then `b`). The struct
    // must win despite its PascalCase -- pre-fix the leading case penalty buried
    // it below every case-clean lowercase candidate. (mag: `speba`/`spellb`.)
    let labels = labels_at(
        "struct SpellBindingX { key: string }\n\
         fn spell_bank() {}\n\
         fn spell_clear() {}\n\
         fn r() {\n  spellb$0\n}\n",
    );
    let s = rank_of(&labels, "SpellBindingX");
    let b = rank_of(&labels, "spell_bank");
    assert!(s.is_some(), "SpellBindingX must appear for `spellb`: {labels:?}");
    assert!(s < b, "SpellBindingX (prefix match) must rank above spell_bank (boundary match): {labels:?}");
}

#[test]
fn strong_const_match_beats_weaker_snake_match() {
    // `fon` is a whole-prefix (+1000) of `FONT_SMALL` but only a boundary match
    // on the fn `format_new` (`fo` then `_n`). The const must rank first --
    // pre-fix the leading case penalty demoted it below the case-clean fn even
    // with the dominant prefix bonus. (mag: lowercase `fon`/`fosma` should
    // surface FONT_SMALL, not bury it.)
    let labels = labels_at(
        "const FONT_SMALL: float = 12.0\n\
         fn format_new() {}\n\
         fn r() {\n  fon$0\n}\n",
    );
    let c = rank_of(&labels, "FONT_SMALL");
    let d = rank_of(&labels, "format_new");
    assert!(c.is_some(), "FONT_SMALL must appear for `fon`: {labels:?}");
    assert!(c < d || d.is_none(), "FONT_SMALL (prefix match) must rank above format_new: {labels:?}");
}

// -- Type-directed arg completion: UFCS receiver is param 0, so the arg index
//    into the callee's params is comma_count + 1. ----------------------------

#[test]
fn ufcs_arg_completion_uses_the_correct_param_type() {
    // `ui.text("x", 12.0, WHI)` -- the color slot. UFCS makes `ui` param 0, so
    // `WHI` is param 3 (`color: Color`), NOT param 2 (`size: float`). Pre-fix the
    // off-by-one made the expected type `float`, so the reachability expansion
    // flooded the popup with float-typed paths (`WHITE_PIXEL.image_dims()...`)
    // and buried the actual `WHITE: Color`. With the right type only Color values
    // are surfaced.
    // Two `text` overloads with DIFFERENT param-3 types (Color vs float): the
    // UFCS receiver `ui: UiRect` must select the UiRect overload, so the slot is
    // `Color` -- NOT the union `{Color, float}` that would re-admit the float
    // reach paths. (Requires an inferable receiver; a simple param is.)
    let labels = labels_at(
        "value Color { r: float, g: float, b: float, a: float }\n\
         struct UiRect { x: float }\n\
         struct Image { w: int }\n\
         global WHITE: Color = Color { r: 1.0, g: 1.0, b: 1.0, a: 1.0 }\n\
         global WHITE_PIXEL: Image = Image { w: 1 }\n\
         fn image_dims(i: Image) -> float { 1.0 }\n\
         fn text(r: UiRect, s: string, size: float, color: Color) -> UiRect { r }\n\
         fn text(d: Image, s: string, size: float, alpha: float) -> Image { d }\n\
         fn draw(ui: UiRect) {\n  ui.text(\"x\", 12.0, WHI$0)\n}\n",
    );
    let white = rank_of(&labels, "WHITE");
    assert!(white.is_some(), "WHITE (Color) must be offered: {labels:?}");
    // The float-typed reachability explosion (`WHITE_PIXEL.image_dims()...`) must
    // be gone -- no dotted / method reach paths in a Color slot. (A bare
    // `WHITE_PIXEL` global still fuzzy-matches `whi` and is fine.)
    assert!(
        !labels.iter().any(|l| l.contains("image_dims") || l.contains("->") || l.contains('.')),
        "float-typed reach paths must not appear in a Color slot: {labels:?}"
    );
    // The Color value ranks above the Image global in a Color slot.
    let pixel = rank_of(&labels, "WHITE_PIXEL");
    assert!(white < pixel || pixel.is_none(), "WHITE (Color) must rank above WHITE_PIXEL (Image): {labels:?}");
}

#[test]
fn ufcs_chain_receiver_overload_filter_uses_last_method_return_type() {
    // Receiver is a method CHAIN `ui.fill().text(..., WHI)`. The broken trailing
    // arg means the statement won't infer, so `type_at` on the receiver `)` gives
    // the callee fn type, not the call result. The last method's UNIQUE return
    // type (UiRect, resolved structurally) selects the UiRect `text` overload ->
    // Color slot, so the float reach explosion stays gone even for a chain.
    let labels = labels_at(
        "value Color { r: float }\n\
         struct UiRect { x: float }\n\
         struct Image { w: int }\n\
         global WHITE: Color = Color { r: 1.0 }\n\
         global WHITE_PIXEL: Image = Image { w: 1 }\n\
         fn image_dims(i: Image) -> float { 1.0 }\n\
         fn fill(r: UiRect) -> UiRect { r }\n\
         fn text(r: UiRect, s: string, color: Color) -> UiRect { r }\n\
         fn text(d: Image, s: string, alpha: float) -> Image { d }\n\
         fn draw(ui: UiRect) {\n  ui.fill().text(\"x\", WHI$0)\n}\n",
    );
    assert!(rank_of(&labels, "WHITE").is_some(), "WHITE offered for a chain receiver: {labels:?}");
    assert!(
        !labels.iter().any(|l| l.contains("image_dims") || l.contains('.') || l.contains("->")),
        "chain receiver must resolve to UiRect (no float reach paths): {labels:?}"
    );
}

#[test]
fn completion_recovers_in_lambda_body_inside_struct_literal_in_array() {
    // Cursor in a lambda body that is a struct-literal field VALUE inside an array
    // global-init: `PanelReg { is_open: () => SPE| }`. The field-name completion
    // must not eat this -- a top-level `:` precedes the cursor in the segment even
    // though the value is a complex `() => ..` expression -- so it falls through
    // to normal completion.
    let labels = labels_at(
        "struct PanelReg { is_open: fn() -> bool }\n\
         global SPELL_OPEN: bool = false\n\
         global PANELS: [PanelReg] = [\n  PanelReg { is_open: () => SPE$0 }\n]\n",
    );
    assert!(rank_of(&labels, "SPELL_OPEN").is_some(), "recovery in a nested lambda body: {labels:?}");
}

// -- Struct field VALUE position is a value slot, not a field-name slot. -------

#[test]
fn struct_field_value_position_is_not_eaten_by_field_name_completion() {
    // `SpellBindingX { key: "k", kind: Spell }` -- the cursor is in the `kind:`
    // VALUE. Pre-fix `struct_literal_context` still treated it as a field-name
    // slot, filtered the field names by `Spell` (none match), and returned an
    // empty list -- killing all completion. It must fall through to normal
    // (type-directed) completion instead.
    let labels = labels_at(
        "enum SpellKind { Fire, Harm }\n\
         struct SpellBindingX { key: string, kind: SpellKind }\n\
         global BINDS: [SpellBindingX] = [\n  SpellBindingX { key: \"k\", kind: Spell$0 }\n]\n",
    );
    assert!(!labels.is_empty(), "field-value position must not return empty completion");
    assert!(
        rank_of(&labels, "SpellKind").is_some() || labels.iter().any(|l| l.contains("SpellKind")),
        "the field's type SpellKind (or its variants) must be offered: {labels:?}"
    );
}

// -- Confident call-through: a UNIQUE in-scope producer of the param type is an
//    unambiguous arg, so `f(arg)` is offered even when the fn name fully covers
//    the query (normally suppressed as fn-name browsing). ---------------------

#[test]
fn confident_unique_local_arg_autofills_call_through() {
    // `can` fully matches `can_cast` and there's exactly one in-scope value of
    // its param type (`s: SpellX`), so `can_cast(s)` is offered alongside the
    // bare `can_cast`.
    let labels = labels_at(
        "struct SpellX { kind: int }\n\
         fn can_cast(s: SpellX) -> bool { true }\n\
         fn probe(s: SpellX) {\n  let a = can$0\n}\n",
    );
    assert!(rank_of(&labels, "can_cast").is_some(), "bare can_cast present: {labels:?}");
    assert!(rank_of(&labels, "can_cast(s)").is_some(), "unique-arg autofill can_cast(s): {labels:?}");
}

#[test]
fn confident_unique_field_path_arg_autofills_call_through() {
    // `spehol` fully matches `spell_hotkey_label(kind: int)`. The only in-scope
    // producer of `int` is the field path `s.kind`, so `spell_hotkey_label(s.kind)`
    // is offered (field-path autofill through the unique producer).
    let labels = labels_at(
        "struct SpellX { kind: int }\n\
         fn spell_hotkey_label(kind: int) -> string { \"\" }\n\
         fn probe(s: SpellX) {\n  let b = spehol$0\n}\n",
    );
    assert!(
        rank_of(&labels, "spell_hotkey_label(s.kind)").is_some(),
        "field-path autofill spell_hotkey_label(s.kind): {labels:?}"
    );
}

#[test]
fn confident_autofill_suppressed_when_arg_is_ambiguous() {
    // TWO in-scope values of the param type -> the arg is ambiguous, so NO
    // autofill (guards against flooding the popup with `f(a)`/`f(b)` noise when
    // the fn name already fully matches). The bare fn still completes.
    let labels = labels_at(
        "struct SpellX { kind: int }\n\
         fn can_cast(s: SpellX) -> bool { true }\n\
         fn probe(s1: SpellX, s2: SpellX) {\n  let a = can$0\n}\n",
    );
    assert!(rank_of(&labels, "can_cast").is_some(), "bare can_cast present: {labels:?}");
    assert!(
        !labels.iter().any(|l| l.starts_with("can_cast(")),
        "ambiguous arg must not autofill: {labels:?}"
    );
}

#[test]
fn struct_field_value_offers_expected_enum_variant() {
    // `kind: SpellKind` -- typing `har` in the value slot should surface the
    // `SpellKind::Harm` variant (type-directed: the field's type is the enum).
    let labels = labels_at(
        "enum SpellKind { Fire, Harm }\n\
         struct SpellBindingX { key: string, kind: SpellKind }\n\
         global BINDS: [SpellBindingX] = [\n  SpellBindingX { key: \"k\", kind: har$0 }\n]\n",
    );
    assert!(
        labels.iter().any(|l| l.contains("Harm")),
        "SpellKind::Harm must be offered for `har` in the enum-typed field slot: {labels:?}"
    );
}
