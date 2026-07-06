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
fn lambda_body_type_directs_on_field_return_type() {
    // `is_open: fn() -> bool` -- the lambda body `() => sp|` returns the field's
    // function-type RETURN type (`bool`), so a bool candidate must type-direct
    // above an int one. Names chosen so the shorter-first tiebreak would put the
    // int candidate first WITHOUT type direction (the RED state).
    let labels = labels_at(
        "struct PanelReg { is_open: fn() -> bool }\n\
         global SPELL_IS_OPEN: bool = false\n\
         global SPELL_ID: int = 0\n\
         global PANELS: [PanelReg] = [\n  PanelReg { is_open: () => sp$0 }\n]\n",
    );
    let b = rank_of(&labels, "SPELL_IS_OPEN");
    let i = rank_of(&labels, "SPELL_ID");
    assert!(b.is_some() && i.is_some(), "both offered: {labels:?}");
    assert!(b < i, "bool candidate must type-direct above the int in a `-> bool` lambda body: {labels:?}");
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

// -- after_yield: coroutine-fn boost fires only on a REAL yield operand -------

#[test]
fn yield_operand_ranks_coroutine_fn_first() {
    // Positive control for the comment case below: after a real `yield*`,
    // the coroutine fn must outrank the plain fn that would otherwise win
    // (shorter name, same prefix).
    let labels = labels_at(
        "co fn frame_co() -> Coroutine<int> {\n  yield 1\n}\n\
         fn frame() {}\n\
         co fn t() -> Coroutine<int> {\n  yield* fr$0\n}\n",
    );
    let co = rank_of(&labels, "frame_co");
    let plain = rank_of(&labels, "frame");
    assert!(co.is_some() && plain.is_some(), "both offered: {labels:?}");
    assert!(co < plain, "coroutine fn must rank first after `yield*`: {labels:?}");
}

#[test]
fn yield_inside_a_comment_gets_no_coroutine_boost() {
    // The byte scanner is comment-blind: `// yield fr|` classified as a
    // yield operand and promoted coroutine fns. The AST classifier sees no
    // yield expr at the cursor.
    let labels = labels_at(
        "co fn frame_co() -> Coroutine<int> {\n  yield 1\n}\n\
         fn frame() {}\n\
         fn t() {\n  // yield fr$0\n}\n",
    );
    let co = rank_of(&labels, "frame_co");
    let plain = rank_of(&labels, "frame");
    assert!(co.is_some() && plain.is_some(), "both offered: {labels:?}");
    assert!(
        plain < co,
        "no coroutine boost inside a comment -- `frame` (shorter, same prefix) must rank first: {labels:?}"
    );
}

// == Type-directed arg ranking (F2 corpus) ====================================
// Cases where the RENDERED type strings differ but the types are call-compatible
// (generics, nullable widening). Ranking runs bone's real `coerce` on TyIds
// (`ExpectedArg`, docs/assignable-plan.md), so each case pins that a
// call-compatible candidate takes the expected-type bucket over an incompatible
// decoy.

fn assert_ranks_above(src: &str, want: &str, decoy: &str) {
    let labels = labels_at(src);
    let w = rank_of(&labels, want);
    let d = rank_of(&labels, decoy);
    assert!(w.is_some(), "`{want}` must appear: {labels:?}");
    assert!(
        w < d,
        "`{want}` (expected-type match) must rank above `{decoy}`: {labels:?}"
    );
}

// -- Control: concrete param type, rendered strings equal on both sides. This
//    anchors the harness -- proves the boost is detectable by these fixtures. -

#[test]
fn concrete_param_type_boosts_matching_local() {
    // Expected `int`; `n: int` must beat the alphabetically-earlier string
    // decoy purely on the type bucket.
    assert_ranks_above(
        "fn feed(x: int) {}\n\
         fn r() {\n  let aa_decoy = \"hello\"\n  let n = 5\n  feed($0)\n}\n",
        "n",
        "aa_decoy",
    );
}

#[test]
fn ufcs_arg_completion_on_local_receiver_does_not_panic() {
    // The receiver is an UN-ANNOTATED let-local, so its type is `TyId::Local`
    // -- rendering it needs the body's inference. Pre-fix,
    // `method_receiver_type` passed `None` and panicked
    // ("Local type needs its BodyInference") on every such keystroke.
    let labels = labels_at(
        "fn add_all<T>(xs: [T], ys: [T]) {}\n\
         fn r() {\n  let nums = [1, 2]\n  nums.add_all($0)\n}\n",
    );
    assert!(!labels.is_empty(), "completion must survive a local UFCS receiver");
}

// -- Compatible types, different renders. -------------------------------------

#[test]
fn generic_param_boosts_instantiated_vec() {
    // Param renders `[T]`, candidate `nums` renders `[int]` -- assignable
    // under instantiation, so `nums` takes the expected-type bucket. Fns with
    // a generic return (`clone`, `vec_new`) rank in the fn bucket below it by
    // actually instantiating their own generics, not by render coincidence.
    assert_ranks_above(
        "fn apply_all<T>(xs: [T]) {}\n\
         fn r() {\n  let aa_decoy = \"hello\"\n  let nums = [1, 2, 3]\n  apply_all($0)\n}\n",
        "nums",
        "aa_decoy",
    );
}

#[test]
fn nullable_param_boosts_bare_value() {
    // Param `int?` accepts `n: int` (T -> T? widening), so `n` ranks first.
    assert_ranks_above(
        "fn feed(x: int?) {}\n\
         fn r() {\n  let aa_decoy = \"hello\"\n  let n = 5\n  feed($0)\n}\n",
        "n",
        "aa_decoy",
    );
}

#[test]
fn generic_ufcs_receiver_overload_not_dropped() {
    // `nums.add_all($0)`: the receiver `[int]` coerces into the UFCS first
    // param `[T]` and its binding is KEPT, so the second slot expects `[int]`
    // and `more` ranks first.
    assert_ranks_above(
        "fn add_all<T>(xs: [T], ys: [T]) {}\n\
         fn r() {\n  let aa_decoy = \"x\"\n  let nums = [1, 2]\n  let more = [3]\n  nums.add_all($0)\n}\n",
        "more",
        "aa_decoy",
    );
}

// -- F3 flip 5: arg context from the AST classifier ---------------------------

#[test]
fn fresh_arg_slot_between_named_args_type_directs() {
    // `draw(p, size: 2.0,| color: c)` -- a fresh arg typed between existing
    // NAMED args resolves to slot 2 (the classifier's between-args arm; the
    // sentinel munges all break here and the raw parse wins), so the string
    // param type-directs. Parity control for the arg_ctx wiring: a slot
    // miscount or a lost context breaks the `sv` boost.
    assert_ranks_above(
        "fn draw(p: int, size: float, name: string, color: int) {}\n\
         fn t(c: int) {\n  let sv: string = \"x\"\n  let iv = 7\n  draw(1, size: 2.0,$0 color: c)\n}\n",
        "sv",
        "iv",
    );
}

#[test]
fn chain_receiver_with_string_paren_still_overload_filters() {
    // `mk("(x").m(|` -- the byte paren-matcher walks into the string's stray
    // `(` and loses the chain callee, so the receiver type was unresolved and
    // BOTH overloads' params ranked. The AST has the receiver as a node.
    // With the filter, only the A overload's `x: int` is expected -- the int
    // local must outrank the string local (named to lose the tie if the
    // string overload leaks in).
    assert_ranks_above(
        "struct A { t: int }\nstruct B { t: int }\n\
         fn m(a: A, x: int) {}\n\
         fn m(b: B, s: string) {}\n\
         fn mk(tag: string) -> A { return A { t: 1 } }\n\
         fn t() {\n  let aa_s: string = \"y\"\n  let zz_i = 7\n  mk(\"(x\").m($0)\n}\n",
        "zz_i",
        "aa_s",
    );
}

// -- F2 phase 2: member-slot type direction runs bone's coerce ----------------

/// Member completion returns items in declaration order; the client sorts by
/// `sort_text`. Rank the way the client sees it.
fn member_labels_by_sort(src: &str) -> Vec<String> {
    let (s, pos) = marked(src);
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, s);
    let mut items = bone.completion(PATH, pos);
    items.sort_by(|a, b| a.sort_text.cmp(&b.sort_text));
    items.into_iter().map(|c| c.label).collect()
}

// == Ranking ladder (docs/completion-ranking-design.md) =======================

#[test]
fn expr_position_local_beats_boundary_richer_fn() {
    // `targrid` matches a local `target_grid: IVec2` and a fn
    // `target_ref_grid(t) -> IVec2?`. The fn's extra `_ref_` gives it one more
    // word-boundary hit -> a higher RAW fuzzy score (79 vs 77), but both are
    // word-boundary subsequence matches -- same coarse band -- so the local
    // (kind 0) must win over the function (kind 2). RED pre-banding: raw score
    // led and the fn won.
    let labels = labels_at(
        "struct IVec2 { x: int, y: int }\n\
         fn target_ref_grid(t: int) -> IVec2? { return IVec2 { x: 0, y: 0 } }\n\
         fn use_it() {\n  let target_grid = IVec2 { x: 1, y: 2 }\n  let tdist = targrid$0\n}\n",
    );
    let local = rank_of(&labels, "target_grid");
    let func = rank_of(&labels, "target_ref_grid");
    assert!(local.is_some() && func.is_some(), "both offered: {labels:?}");
    assert!(local < func, "local var must beat boundary-richer fn for `targrid`: {labels:?}");
}

#[test]
fn local_seeded_call_through_ranks_above_unrelated_global() {
    // Typing `att` with a local `mob: Mob` in scope and a fn `attack(m: Mob)`:
    // the call-through `attack(mob)` (a fn accepting the local, seeded from a var
    // of the right type) must rank ABOVE an unrelated prefix-matching global
    // `attention_span`. Pre-fix, ALL call-through was bucketed last (below every
    // direct), so the unrelated global won.
    let labels = labels_at(
        "struct Mob { hp: int }\n\
         global attention_span: int = 0\n\
         fn attack(m: Mob) {}\n\
         fn r(mob: Mob) {\n  att$0\n}\n",
    );
    let ct = rank_of(&labels, "attack(mob)");
    let global = rank_of(&labels, "attention_span");
    assert!(ct.is_some(), "attack(mob) call-through offered: {labels:?}");
    assert!(global.is_some(), "attention_span offered: {labels:?}");
    assert!(ct < global, "local-seeded call-through must outrank unrelated global: {labels:?}");
}

#[test]
fn partial_fn_name_with_local_arg_promotes_call_through() {
    // `givemob`: the fn name `give` absorbs `give`, the remainder `mob` matches
    // the in-scope local `mob: Mob` (the param type), so `give(mob)` is a
    // partial-match call-through seeded from a LOCAL of the right type. It must
    // be promoted out of the last bucket (sort_text bucket 0, not 4).
    let labels = labels_at(
        "struct Mob { hp: int }\n\
         fn give(m: Mob) {}\n\
         fn r(mob: Mob) {\n  givemob$0\n}\n",
    );
    let (s, pos) = marked(
        "struct Mob { hp: int }\n\
         fn give(m: Mob) {}\n\
         fn r(mob: Mob) {\n  givemob$0\n}\n",
    );
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, s);
    let it = bone
        .completion(PATH, pos)
        .into_iter()
        .find(|c| c.label == "give(mob)")
        .unwrap_or_else(|| panic!("give(mob) call-through must be offered: {labels:?}"));
    let st = it.sort_text.as_deref().unwrap_or("");
    assert!(
        st.starts_with('0'),
        "partial local-seeded call-through must be promoted (bucket 0, not 4): {st}"
    );
}

#[test]
fn relaxed_guard_offers_local_seeded_call_through_on_short_fn_prefix() {
    // `gmob`: the fn `give` absorbs only `g` (< half the 4-char query), but the
    // residual `mob` matches the in-scope local `mob: Mob` (the param type). The
    // noise guard normally suppresses a fn absorbing < half the query; for a
    // LOCAL-seeded arg it's relaxed, so `give(mob)` is offered.
    let labels = labels_at(
        "struct Mob { hp: int }\n\
         fn give(m: Mob) {}\n\
         fn r(mob: Mob) {\n  gmob$0\n}\n",
    );
    assert!(
        labels.iter().any(|l| l == "give(mob)"),
        "relaxed guard must offer give(mob) for `gmob`: {labels:?}"
    );
}

#[test]
fn diversity_caps_reach_path_flood_in_the_visible_top() {
    // A local `s: S` with 15 int fields floods the `feed(<int>)` arg slot with 15
    // dotted reach paths (`s.a`..`s.o`). Without a diversity cap they fill the
    // visible top and bury every other kind; the cap keeps at most 3 dotted paths
    // in the top window, deferring the rest just below (kept, never dropped).
    let labels = labels_at(
        "fn feed(x: int) {}\n\
         struct S { a: int, b: int, c: int, d: int, e: int, f: int, g: int, h: int, i: int, j: int, k: int, l: int, m: int, n: int, o: int }\n\
         fn r(s: S) {\n  feed($0)\n}\n",
    );
    let paths_in_window = labels.iter().take(12).filter(|l| l.contains('.')).count();
    let paths_total = labels.iter().filter(|l| l.contains('.')).count();
    assert!(
        paths_in_window <= 3,
        "at most 3 reach paths in the visible top-12: {:?}",
        &labels[..labels.len().min(14)]
    );
    assert!(paths_total >= 15, "overflow paths deferred, not dropped: {paths_total} paths total");
}

#[test]
fn exact_name_match_outranks_type_directed_candidate() {
    // Inside `feed(<cursor>)` expecting `int`, a local `n: int` is type-directed,
    // but a local named exactly `feed`-arg... here: typing `xs` where a local
    // `xs: string` (WRONG type) exactly matches the name must still outrank the
    // int-typed `xsize`. Exact name is the strongest signal, above the expected
    // type bucket.
    let labels = labels_at(
        "fn feed(x: int) {}\n\
         fn r() {\n  let xsize = 5\n  let xs = \"hi\"\n  feed(xs$0)\n}\n",
    );
    let exact = rank_of(&labels, "xs");
    let typed = rank_of(&labels, "xsize");
    assert!(exact.is_some() && typed.is_some(), "both offered: {labels:?}");
    assert!(exact < typed, "exact name `xs` must outrank type-directed `xsize`: {labels:?}");
}

#[test]
fn assignment_rhs_prioritizes_expected_enum_variant() {
    // `mob.ai_state = combat` -- the LHS type is `MobAiState`, so the variant
    // `MobAiState::Combat` must outrank a same-prefix wrong-type fn
    // `combat_target_grid(mob) -> int`. RED pre-fix: assignment RHS is not an
    // expected-type context, so the case-exact-prefix fn won on raw score.
    let labels = labels_at(
        "enum MobAiState { Wander, Combat }\n\
         struct Mob { ai_state: MobAiState }\n\
         fn combat_target_grid(mob: Mob) -> int { return 0 }\n\
         fn use_it(mob: Mob) {\n  mob.ai_state = combat$0\n}\n",
    );
    let variant = labels.iter().position(|l| l.contains("MobAiState") && l.contains("Combat"));
    let func = rank_of(&labels, "combat_target_grid");
    assert!(variant.is_some(), "MobAiState::Combat offered: {labels:?}");
    assert!(
        func.is_none() || variant < func,
        "expected enum variant must outrank wrong-type fn in assignment RHS: {labels:?}"
    );
}

#[test]
fn return_value_prioritizes_expected_enum_variant() {
    // `return combat` in a fn declared `-> MobAiState` -- the return slot's
    // type is the fn's declared return type, so `MobAiState::Combat` must
    // outrank the case-exact-prefix wrong-type fn `combat_target_grid`.
    let labels = labels_at(
        "enum MobAiState { Wander, Combat }\n\
         fn combat_target_grid(x: int) -> int { return 0 }\n\
         fn pick() -> MobAiState {\n  return combat$0\n}\n",
    );
    let variant = labels.iter().position(|l| l.contains("MobAiState") && l.contains("Combat"));
    let func = rank_of(&labels, "combat_target_grid");
    assert!(variant.is_some(), "MobAiState::Combat offered: {labels:?}");
    assert!(
        func.is_none() || variant < func,
        "expected enum variant must outrank wrong-type fn in return position: {labels:?}"
    );
}

#[test]
fn typed_let_init_prioritizes_expected_enum_variant() {
    // `let s: MobAiState = combat` -- the annotation types the slot, so the
    // variant must outrank the case-exact-prefix wrong-type fn.
    let labels = labels_at(
        "enum MobAiState { Wander, Combat }\n\
         fn combat_target_grid(x: int) -> int { return 0 }\n\
         fn r() {\n  let s: MobAiState = combat$0\n}\n",
    );
    let variant = labels.iter().position(|l| l.contains("MobAiState") && l.contains("Combat"));
    let func = rank_of(&labels, "combat_target_grid");
    assert!(variant.is_some(), "MobAiState::Combat offered: {labels:?}");
    assert!(
        func.is_none() || variant < func,
        "expected enum variant must outrank wrong-type fn in a typed-let init: {labels:?}"
    );
}

#[test]
fn comparison_rhs_prioritizes_expected_enum_variant() {
    // `mob.ai_state == combat` -- the compared-against LHS types the slot, so
    // the variant must outrank the case-exact-prefix wrong-type fn.
    let labels = labels_at(
        "enum MobAiState { Wander, Combat }\n\
         struct Mob { ai_state: MobAiState }\n\
         fn combat_target_grid(mob: Mob) -> int { return 0 }\n\
         fn use_it(mob: Mob) {\n  if mob.ai_state == combat$0 {\n  }\n}\n",
    );
    let variant = labels.iter().position(|l| l.contains("MobAiState") && l.contains("Combat"));
    let func = rank_of(&labels, "combat_target_grid");
    assert!(variant.is_some(), "MobAiState::Combat offered: {labels:?}");
    assert!(
        func.is_none() || variant < func,
        "expected enum variant must outrank wrong-type fn in a comparison RHS: {labels:?}"
    );
}

#[test]
fn null_coalesce_rhs_prioritizes_expected_enum_variant() {
    // `state ?? combat` with `state: MobAiState?` -- the coalesce RHS expects
    // the LHS type (nullable stripped by the nominal-head check), so the
    // variant must outrank the case-exact-prefix wrong-type fn.
    let labels = labels_at(
        "enum MobAiState { Wander, Combat }\n\
         fn combat_target_grid(x: int) -> int { return 0 }\n\
         fn r(state: MobAiState?) {\n  let s = state ?? combat$0\n}\n",
    );
    let variant = labels.iter().position(|l| l.contains("MobAiState") && l.contains("Combat"));
    let func = rank_of(&labels, "combat_target_grid");
    assert!(variant.is_some(), "MobAiState::Combat offered: {labels:?}");
    assert!(
        func.is_none() || variant < func,
        "expected enum variant must outrank wrong-type fn in a `??` RHS: {labels:?}"
    );
}

#[test]
fn wrong_type_call_through_synth_defers_to_expected_type_variant() {
    // In a `MobAiState` slot, a local-seeded call-through synth whose fn does
    // NOT produce the slot type (`compare(mob) -> int`) must not outrank the
    // type-directed `MobAiState::Combat`. RED pre-fix: the local-seeded
    // promotion (type_rank 0/1) never consulted the expected type.
    let labels = labels_at(
        "enum MobAiState { Wander, Combat }\n\
         struct Mob { ai_state: MobAiState }\n\
         fn compare(mob: Mob) -> int { return 0 }\n\
         fn use_it(mob: Mob) {\n  mob.ai_state = com$0\n}\n",
    );
    let variant = labels.iter().position(|l| l.contains("MobAiState") && l.contains("Combat"));
    let synth = rank_of(&labels, "compare(mob)");
    assert!(variant.is_some(), "MobAiState::Combat offered: {labels:?}");
    assert!(
        synth.is_none() || variant < synth,
        "wrong-type call-through synth must not outrank the expected-type variant: {labels:?}"
    );
}

#[test]
fn right_type_call_through_synth_keeps_promotion_in_expected_slot() {
    // The counterpart: a local-seeded synth whose fn DOES produce the slot type
    // (`make_state(mob) -> MobAiState`) keeps its promotion -- it must outrank
    // a wrong-type same-prefix fn.
    let labels = labels_at(
        "enum MobAiState { Wander, Combat }\n\
         struct Mob { ai_state: MobAiState }\n\
         fn make_state(mob: Mob) -> MobAiState { return MobAiState::Wander }\n\
         fn makeshift_grid(x: int) -> int { return 0 }\n\
         fn use_it(mob: Mob) {\n  mob.ai_state = makes$0\n}\n",
    );
    let synth = rank_of(&labels, "make_state(mob)");
    let wrong = rank_of(&labels, "makeshift_grid");
    assert!(synth.is_some(), "make_state(mob) offered: {labels:?}");
    assert!(
        wrong.is_none() || synth < wrong,
        "right-type call-through synth must outrank a wrong-type fn: {labels:?}"
    );
}

#[test]
fn member_field_coercing_to_nullable_slot_ranks_first() {
    // `wants(s.|)` with `wants(x: int?)`: the field `zog: int` COERCES into
    // `int?` (nullable widening) so it must rank above `aaa: string` -- but the
    // string compare `"int" == "int?"` misses it, leaving both untyped and the
    // alphabetically-earlier `aaa` on top. Names chosen so only the typed
    // (coerce) boost flips the order.
    let labels = member_labels_by_sort(
        "struct S { zog: int, aaa: string }\n\
         fn wants(x: int?) {}\n\
         fn r() {\n  let s = S { zog: 1, aaa: \"y\" }\n  wants(s.$0)\n}\n",
    );
    let z = rank_of(&labels, "zog: int");
    let a = rank_of(&labels, "aaa: string");
    assert!(z.is_some() && a.is_some(), "both fields offered: {labels:?}");
    assert!(z < a, "int field must type-direct above string in an int? slot: {labels:?}");
}
