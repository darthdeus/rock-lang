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
    let items = bone.completion(PATH, pos);
    // Every corpus fixture doubles as an invariant-suite position
    // (docs/completion-v2-design.md M4): the position-blind response
    // invariants must hold wherever a ranking decision is pinned.
    let prefix = typed_prefix(src);
    let violations = bone_query::invariants::completion_response_violations(&items, &prefix);
    assert!(
        violations.is_empty(),
        "response invariants violated (prefix {prefix:?}): {violations:?}"
    );
    items.into_iter().map(|c| c.label).collect()
}

/// The raw identifier prefix directly before the `$0` cursor marker (what the
/// user typed).
fn typed_prefix(marked_src: &str) -> String {
    let off = marked_src.find("$0").expect("fixture must contain the `$0` cursor marker");
    let head = &marked_src.as_bytes()[..off];
    let start = head
        .iter()
        .rposition(|b| !(b.is_ascii_alphanumeric() || *b == b'_'))
        .map_or(0, |i| i + 1);
    String::from_utf8_lossy(&head[start..]).into_owned()
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

#[test]
fn contiguous_prefix_run_beats_scattered_all_boundary_match() {
    // `LOOTPA` matches `LOOT_PANEL` as a contiguous "loot" run + "pa", and
    // `LATER_OUT_OF_TOWN_PERSON_ASTEROID` as six scattered word-boundary hits
    // (L-ater O-ut O-f T-own P-erson A-steroid) with no run. Both are band-2
    // clean matches, so the fine score decides -- and the boundary-rich long
    // name must NOT win: LOOT_PANEL is the obvious intent. (nvim
    // completion_ranking_spec.)
    let labels = labels_at(
        "global LOOT_PANEL: int = 0\n\
         global LATER_OUT_OF_TOWN_PERSON_ASTEROID: int = 0\n\
         fn r() {\n  let x = LOOTPA$0\n}\n",
    );
    let loot = rank_of(&labels, "LOOT_PANEL");
    let later = rank_of(&labels, "LATER_OUT_OF_TOWN_PERSON_ASTEROID");
    assert!(loot.is_some(), "LOOT_PANEL must appear for `LOOTPA`: {labels:?}");
    assert!(loot < later, "LOOT_PANEL (contiguous prefix run) must outrank the scattered match: {labels:?}");
}

#[test]
fn contiguous_run_beats_interior_word_boundary_hop() {
    // `pixelreadnew` matches BOTH `pixel_readback_new` and the longer
    // `pixel_readback_depth_new`. The only difference is the query `d` (end of
    // "read"): it can continue the run inside `rea<d>back` or jump to the `d`
    // that STARTS `<d>epth`. A boundary hit worth more than a contiguous
    // continuation made the DP break the run for that stray boundary, so the
    // longer name -- which the query never mentions `depth` in -- scored +2 and
    // ranked first. A run must never be worth LESS than a one-char boundary hop.
    // (mag: `HOVERED_EID_SLOT = pixelreadnew` surfaced `_depth_new` on top.)
    let labels = labels_at(
        "fn pixel_readback_new() -> int { 0 }\n\
         fn pixel_readback_depth_new() -> int { 0 }\n\
         fn r() {\n  let x = pixelreadnew$0\n}\n",
    );
    let plain = rank_of(&labels, "pixel_readback_new");
    let depth = rank_of(&labels, "pixel_readback_depth_new");
    assert!(plain.is_some(), "pixel_readback_new must appear for `pixelreadnew`: {labels:?}");
    assert!(plain < depth, "pixel_readback_new (shorter, no interior-word hop) must outrank pixel_readback_depth_new: {labels:?}");
}

#[test]
fn match_arm_return_ranks_constructor_over_recursive_self_call() {
    // A match arm whose fn returns PartitionStyle: typing `PartSty` must surface
    // the PartitionStyle CONSTRUCTOR, not `partition_style(k)` -- a recursive call
    // to the fn being defined, promoted by the +5000 confident-autofill firing on
    // the fn's own param `k`. Two coupled bugs: the enclosing fn was promoted
    // inside its own body, and the match-arm body was not recognized as return
    // position (so wrong-type synths never demoted). (mag blood.rock:16:39.)
    let labels = labels_at(
        "enum PxKind { Blood, Dust }\n\
         value PartitionStyle { gravity: float, damp: float }\n\
         fn partition_style(k: PxKind) -> PartitionStyle {\n\
         \x20 match k {\n\
         \x20   PxKind::Blood => PartSty$0\n\
         \x20   PxKind::Dust => PartSty\n\
         \x20 }\n\
         }\n",
    );
    let ps = rank_of(&labels, "PartitionStyle");
    let call = rank_of(&labels, "partition_style(k)");
    assert!(ps.is_some(), "PartitionStyle must appear for `PartSty`: {labels:?}");
    assert!(
        ps < call || call.is_none(),
        "PartitionStyle (constructor) must outrank the recursive partition_style(k): {labels:?}"
    );
}

#[test]
fn match_arm_body_sees_the_expected_return_type() {
    // A match-arm body in a fn returning `Sty` is a return slot -- the expected
    // type must be known there (as it is for `return X` / `let x: Sty = X`), so a
    // value OF `Sty` outranks a same-prefix fn whose call produces the WRONG type.
    // Pre-fix the arm body had no expected type, so `sty_maker(x)` (a confident
    // autofill returning int) sat at the top instead of demoting. Isolates the
    // tail/match-arm expected-type detection from the enclosing-fn exclusion.
    let labels = labels_at(
        "value Sty { a: float }\n\
         global sty_val: Sty = Sty { a: 0.0 }\n\
         fn sty_maker(x: int) -> int { 0 }\n\
         fn use_it(x: int) -> Sty {\n\
         \x20 match x {\n\
         \x20   _ => sty$0\n\
         \x20 }\n\
         }\n",
    );
    let val = rank_of(&labels, "sty_val");
    let maker = rank_of(&labels, "sty_maker(x)");
    assert!(val.is_some(), "sty_val must appear for `sty`: {labels:?}");
    assert!(
        val < maker || maker.is_none(),
        "the Sty value must outrank the wrong-type sty_maker(x) autofill: {labels:?}"
    );
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
    // RankKey encoding (rank_key.rs): digit 0 = exact, digit 1 = type_rank --
    // the direct-vs-synth bucket here (0 promoted, 4 bucketed last).
    let st = it.sort_text.as_deref().unwrap_or("");
    assert!(
        st.chars().nth(1) == Some('0'),
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

// -- `_`-prefix soft-private: hidden from the general list unless the user is
//    explicitly typing a `_` prefix (same convention as member completion). --

#[test]
fn underscore_prefixed_symbols_hidden_from_general_list() {
    // `_helper` (fn), `_PRIV` (global), and `_loc` (local) are soft-private;
    // a plain prefix must not surface them, and stdlib `__` internals stay
    // out the same way.
    let labels = labels_at(
        "fn _helper() {}\n\
         global _PRIV: int = 1\n\
         fn visible_helper() {}\n\
         fn r() {\n  let _loc = 2\n  let visible_loc = 3\n  he$0\n}\n",
    );
    assert!(labels.iter().any(|l| l == "visible_helper"), "{labels:?}");
    assert!(
        !labels.iter().any(|l| l == "_helper" || l == "_PRIV" || l == "_loc"),
        "soft-private symbols must not surface on a plain prefix: {labels:?}"
    );
}

#[test]
fn typing_underscore_surfaces_soft_private_symbols() {
    let labels = labels_at(
        "fn _helper() {}\n\
         global _PRIV: int = 1\n\
         fn r() {\n  _$0\n}\n",
    );
    assert!(labels.iter().any(|l| l == "_helper"), "{labels:?}");
    assert!(labels.iter().any(|l| l == "_PRIV"), "{labels:?}");
}

#[test]
fn reach_path_flood_is_kept_and_ranked_not_dropped() {
    // A local `s: S` with 15 int fields fills the `feed(<int>)` arg slot with
    // 15 dotted reach paths (`s.a`..`s.o`). They all rank as right-type values
    // and stay browsable -- the response is capped, never silently thinned.
    // (The old array-only diversity pass was deleted: the real client sorts
    // array order dead-last, so it never took effect -- unfuck-backlog #30.)
    let labels = labels_at(
        "fn feed(x: int) {}\n\
         struct S { a: int, b: int, c: int, d: int, e: int, f: int, g: int, h: int, i: int, j: int, k: int, l: int, m: int, n: int, o: int }\n\
         fn r(s: S) {\n  feed($0)\n}\n",
    );
    let paths_total = labels.iter().filter(|l| l.contains('.')).count();
    assert!(paths_total >= 15, "all reach paths offered: {paths_total} paths total");
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
fn typed_slot_local_beats_global_of_same_type() {
    // Inside `feed(<cursor>)` expecting int, a right-type LOCAL must outrank a
    // right-type GLOBAL even when the global's name scores higher -- locality
    // is a rung of the ladder in the typed layout too (design doc principle 4).
    // Names chosen so the global's fine score wins (case-exact prefix of the
    // query) while the local is a boundary subsequence: only the kind rung can
    // put the local first.
    let labels = labels_at(
        "global nomad_x: int = 3\n\
         fn feed(x: int) {}\n\
         fn r() {\n  let nx_offset_mad = 5\n  feed(nomad$0)\n}\n",
    );
    let local = rank_of(&labels, "nx_offset_mad");
    let global = rank_of(&labels, "nomad_x");
    assert!(local.is_some() && global.is_some(), "both offered: {labels:?}");
    assert!(
        global < local,
        "case-exact-prefix global still beats a scattered local (band before kind): {labels:?}"
    );
}

#[test]
fn typed_slot_kind_breaks_same_band_ties() {
    // Same band, same type: the local wins over the global on the locality
    // rung (today the fine score decides and can flip it to the global).
    let labels = labels_at(
        "global target_ref_grid_max: int = 3\n\
         fn feed(x: int) {}\n\
         fn r() {\n  let target_grid = 5\n  feed(targrid$0)\n}\n",
    );
    let local = rank_of(&labels, "target_grid");
    let global = rank_of(&labels, "target_ref_grid_max");
    assert!(local.is_some() && global.is_some(), "both offered: {labels:?}");
    assert!(
        local < global,
        "same-band right-type local must outrank the global on the kind rung: {labels:?}"
    );
}

#[test]
fn member_completion_prefix_field_beats_boundary_richer_field() {
    // MEMBER path banding: `s.co$0` -- the case-exact-prefix field `cost` must
    // outrank `charge_over_time`, whose extra word boundaries give it a higher
    // fine score (`c` + `_o`). Pre-M1 the member encoders were score-first
    // (no exact/band tiers), so the boundary-richer name won on raw points.
    let labels = member_labels_by_sort(
        "struct S { cost: int, charge_over_time: int }\n\
         fn r(s: S) {\n  let x = s.co$0\n}\n",
    );
    let pfx = labels.iter().position(|l| l.starts_with("cost"));
    let boundary = labels.iter().position(|l| l.starts_with("charge_over_time"));
    assert!(pfx.is_some() && boundary.is_some(), "both fields offered: {labels:?}");
    assert!(
        pfx < boundary,
        "prefix-band field must outrank a boundary-richer higher-scoring one: {labels:?}"
    );
}

#[test]
fn direct_global_beats_global_rooted_reach_chain() {
    // Typed slot: a direct right-type GLOBAL must outrank a reach chain rooted
    // at some OTHER global -- both produce the type, but the chain rides the
    // same (global) locality rung and loses on distance. Pinned from the M1
    // sweep review: reach paths briefly took kind 0 and deep global chains
    // outranked direct globals.
    let labels = labels_at(
        "struct Holder { grid_size: int }\n\
         global grip_count: int = 3\n\
         global grid_holder: Holder = Holder { grid_size: 1 }\n\
         fn feed(x: int) {}\n\
         fn r() {\n  feed(gr$0)\n}\n",
    );
    let direct = rank_of(&labels, "grip_count");
    let chain = labels.iter().position(|l| l.starts_with("grid_holder.grid_size"));
    assert!(direct.is_some() && chain.is_some(), "both offered: {labels:?}");
    assert!(
        direct < chain,
        "direct right-type global must outrank a global-rooted reach chain: {labels:?}"
    );
}

#[test]
fn local_rooted_reach_path_beats_right_type_global() {
    // Typed slot, SAME match band (both case-exact `h` prefixes): a path
    // seeded from a LOCAL (`h.grid_size`) is a top-tier candidate -- above a
    // direct right-type GLOBAL (design doc principle 3: local-seeded before
    // globals). Match quality still leads: a better-band global beats a
    // worse-band path (covered by `direct_global_beats_global_rooted_reach_chain`).
    let labels = labels_at(
        "struct Holder { grid_size: int }\n\
         global h_count: int = 3\n\
         fn feed(x: int) {}\n\
         fn r(h: Holder) {\n  feed(h$0)\n}\n",
    );
    let path = labels.iter().position(|l| l.starts_with("h.grid_size"));
    let global = rank_of(&labels, "h_count");
    assert!(path.is_some() && global.is_some(), "both offered: {labels:?}");
    assert!(
        path < global,
        "same-band local-rooted right-type path must outrank the right-type global: {labels:?}"
    );
}

#[test]
fn member_completion_type_directs_from_assignment_lhs() {
    // `mob.ai_state = other.$0` -- the slot-anchored expected type (assign
    // LHS) must reach the MEMBER path: `other`'s MobAiState-typed field ranks
    // above its int field. Names chosen so the pre-fix order (no expected
    // type -> alphabetical at empty prefix) puts the WRONG field first.
    let labels = member_labels_by_sort(
        "enum MobAiState { Wander, Combat }\n\
         struct Mob { ai_state: MobAiState }\n\
         struct Other { zz_state: MobAiState, aa_count: int }\n\
         fn r(mob: Mob, other: Other) {\n  mob.ai_state = other.$0\n}\n",
    );
    let right = labels.iter().position(|l| l.starts_with("zz_state"));
    let wrong = labels.iter().position(|l| l.starts_with("aa_count"));
    assert!(right.is_some() && wrong.is_some(), "both fields offered: {labels:?}");
    assert!(
        right < wrong,
        "slot-typed field must type-direct in member completion: {labels:?}"
    );
}

#[test]
fn member_completion_type_directs_from_return_slot() {
    // `return other.$0` in a `-> MobAiState` fn: same drift class, return
    // anchor.
    let labels = member_labels_by_sort(
        "enum MobAiState { Wander, Combat }\n\
         struct Other { zz_state: MobAiState, aa_count: int }\n\
         fn pick(other: Other) -> MobAiState {\n  return other.$0\n}\n",
    );
    let right = labels.iter().position(|l| l.starts_with("zz_state"));
    let wrong = labels.iter().position(|l| l.starts_with("aa_count"));
    assert!(right.is_some() && wrong.is_some(), "both fields offered: {labels:?}");
    assert!(
        right < wrong,
        "return-slot type must type-direct in member completion: {labels:?}"
    );
}

#[test]
fn member_arg_slot_field_beats_boundary_richer_method() {
    // `feed(s.targrid$0)` with both members producing int (type-match tie),
    // both in the boundary band: the FIELD `target_grid` must beat the method
    // `target_ref_grid()` whose extra `_ref_` boundary out-scores it on fine
    // points. Pre-M1 `member_arg_sort_text` was score-first past the
    // type-match digit, so the method won.
    let labels = member_labels_by_sort(
        "struct S { target_grid: int }\n\
         fn target_ref_grid(s: S) -> int { return 0 }\n\
         fn feed(x: int) {}\n\
         fn r(s: S) {\n  feed(s.targrid$0)\n}\n",
    );
    let field = labels.iter().position(|l| l.starts_with("target_grid"));
    let method = labels.iter().position(|l| l.starts_with("target_ref_grid"));
    assert!(field.is_some() && method.is_some(), "both members offered: {labels:?}");
    assert!(
        field < method,
        "same-band type-matching field must outrank the boundary-richer method: {labels:?}"
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

// -- test_* / tests-dir demotion outside test files ---------------------------

fn rank_in(labels: &[String], needle: &str) -> usize {
    labels
        .iter()
        .position(|l| l == needle)
        .unwrap_or_else(|| panic!("no completion `{needle}` in {labels:?}"))
}

/// The `kind` digit (RankKey position 4) of `label`'s sort_text at the
/// cursor -- the demotion rung under test, independent of cap truncation
/// and match band.
fn kind_digit(path: &str, src: &str, label: &str) -> char {
    let (s, pos) = marked(src);
    let mut bone = BoneWorkspace::new();
    bone.update(path, s);
    let items = bone.completion(path, pos);
    let it = items
        .iter()
        .find(|c| c.label == label)
        .unwrap_or_else(|| panic!("no completion `{label}`"));
    it.sort_text.as_deref().expect("sort_text").as_bytes()[4] as char
}

#[test]
fn test_prefixed_fn_demotes_outside_test_files() {
    // In a NON-test file, a `test_`-named fn rides the bottom kind bucket
    // (4) even when explicitly surfaced by a matching prefix.
    let src = "fn test_apply() {}
fn r() {
  test_ap$0
}
";
    assert_eq!(
        kind_digit(PATH, src, "test_apply"),
        '4',
        "test_-prefixed fn must ride kind bucket 4 outside test files"
    );
}

#[test]
fn test_prefixed_fn_keeps_normal_kind_inside_test_files() {
    // Same fixture completed in a file under a `tests/` segment: no
    // demotion -- test symbols rank normally among their own.
    let src = "fn test_apply() {}
fn r() {
  test_ap$0
}
";
    assert_eq!(
        kind_digit("/tmp/tests/rock_lsp_ranking_t.rock", src, "test_apply"),
        '2',
        "no demotion inside a test file"
    );
}

#[test]
fn typing_test_prefix_still_surfaces_test_fns() {
    // The band rung IS the escape hatch: an explicit `test_ap` prefix is a
    // case-exact prefix match on the test fn, which outranks every scattered
    // non-test candidate -- no demotion override needed.
    let labels = labels_at(
        "fn test_apply() {}\n\
         fn zap_thing() {}\n\
         fn r() {\n  test_ap$0\n}\n",
    );
    assert_eq!(
        rank_in(&labels, "test_apply"),
        0,
        "explicitly typed test_ prefix must surface the test fn first: {labels:?}"
    );
}

/// A game file + a `tests/` sibling: the tests-dir fn demotes in the game
/// file and ranks normally at home.
fn tests_dir_project() -> (tempfile::TempDir, String, String) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("stick.toml"), "[package]\nname = \"tp\"\n").unwrap();
    std::fs::create_dir(dir.path().join("tests")).unwrap();
    std::fs::write(
        dir.path().join("main.rock"),
        "fn zzz_real() {}\nfn game() {\n  aaa_h\n}\nfn game2() {\n  zzz_r\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("tests").join("helpers.rock"),
        "fn aaa_helper() {}\nfn sim_run() {\n  aaa_h\n}\n",
    )
    .unwrap();
    let main = bone_query::canonicalize_lenient(&dir.path().join("main.rock").to_string_lossy());
    let helper = bone_query::canonicalize_lenient(
        &dir.path().join("tests").join("helpers.rock").to_string_lossy(),
    );
    (dir, main, helper)
}

fn kind_digit_at(bone: &mut BoneWorkspace, path: &str, pos: Position, label: &str) -> char {
    let items = bone.completion(path, pos);
    let it = items
        .iter()
        .find(|c| c.label == label)
        .unwrap_or_else(|| panic!("no completion `{label}`"));
    it.sort_text.as_deref().expect("sort_text").as_bytes()[4] as char
}

#[test]
fn tests_dir_symbols_demote_in_game_files() {
    let (_dir, main, _helper) = tests_dir_project();
    let mut bone = BoneWorkspace::new();
    bone.update(&main, std::fs::read_to_string(&main).unwrap());
    // Cursor after the `aaa_h` prefix in `game`'s body (line 2, col 7).
    let pos = Position::new(2, 7);
    assert_eq!(
        kind_digit_at(&mut bone, &main, pos, "aaa_helper"),
        '4',
        "tests-dir fn must ride kind bucket 4 in a game file"
    );
    assert_eq!(
        kind_digit_at(&mut bone, &main, Position::new(5, 7), "zzz_real"),
        '2',
        "game fns keep their normal kind"
    );
}

#[test]
fn tests_dir_symbols_rank_normally_inside_test_files() {
    let (_dir, _main, helper) = tests_dir_project();
    let mut bone = BoneWorkspace::new();
    bone.update(&helper, std::fs::read_to_string(&helper).unwrap());
    let pos = Position::new(2, 7);
    assert_eq!(
        kind_digit_at(&mut bone, &helper, pos, "aaa_helper"),
        '2',
        "no demotion inside a test file"
    );
}

// -- Priors: proximity + scope recency (late tiebreak, post-fine) ------------

/// sort_text digit at `idx` (0-based) for `label` at the cursor.
fn key_digit(path: &str, src: &str, label: &str, idx: usize) -> char {
    let (s, pos) = marked(src);
    let mut bone = BoneWorkspace::new();
    bone.update(path, s);
    let items = bone.completion(path, pos);
    let it = items
        .iter()
        .find(|c| c.label == label)
        .unwrap_or_else(|| panic!("no completion `{label}`"));
    it.sort_text.as_deref().expect("sort_text").as_bytes()[idx] as char
}

const PROX_DIGIT: usize = 12;
const REC_DIGIT: usize = 14;

#[test]
fn same_file_fn_outranks_stdlib_fns_at_empty_prefix() {
    // Empty-prefix browse: the file's own fn (alphabetically LAST, so only
    // the proximity rung can lift it past the cap and the stdlib) ranks
    // above stdlib fns like `abs`.
    let labels = labels_at("fn zzz_mine() {}\nfn r() {\n  $0\n}\n");
    assert!(
        rank_in(&labels, "zzz_mine") < rank_in(&labels, "abs"),
        "same-file fn must outrank stdlib fns at empty prefix: {:?}...",
        &labels[..labels.len().min(12)]
    );
}

#[test]
fn prox_digits_same_file_vs_stdlib() {
    let src = "fn zzz_mine() {}\nfn r() {\n  $0\n}\n";
    assert_eq!(key_digit(PATH, src, "zzz_mine", PROX_DIGIT), '0', "same-file fn is prox 0");
    assert_eq!(key_digit(PATH, src, "abs", PROX_DIGIT), '3', "stdlib fn is prox 3");
}

#[test]
fn sibling_project_file_is_prox_one() {
    let (_dir, main, _helper) = tests_dir_project();
    let mut bone = BoneWorkspace::new();
    bone.update(&main, std::fs::read_to_string(&main).unwrap());
    // `aaa_helper` is defined in tests/helpers.rock -- same project root.
    let items = bone.completion(&main, Position::new(2, 7));
    let it = items.iter().find(|c| c.label == "aaa_helper").expect("offered");
    assert_eq!(
        it.sort_text.as_deref().expect("sort_text").as_bytes()[PROX_DIGIT] as char,
        '1',
        "same-project sibling file is prox 1"
    );
}

#[test]
fn recent_local_beats_enclosing_param_on_ties() {
    // `d` is a whole-prefix match on both; every match rung ties until the
    // recency digit -- the fresh `let dmg` (closest declaration above the
    // cursor) must beat the param `dt`, which today wins on shorter length.
    let labels = labels_at("fn r(dt: float) {\n  let dmg = 1.0\n  d$0\n}\n");
    assert!(
        rank_in(&labels, "dmg") < rank_in(&labels, "dt"),
        "the most recent binding must win the tie: {labels:?}"
    );
}

#[test]
fn rec_digits_order_locals_by_declaration_position() {
    let src = "fn r(dt: float) {\n  let alpha = 1\n  let dmg = 1.0\n  $0\n}\n";
    // Closest declaration above the cursor first: dmg 0, alpha 1, dt 2.
    assert_eq!(key_digit(PATH, src, "dmg", REC_DIGIT), '0');
    assert_eq!(key_digit(PATH, src, "alpha", REC_DIGIT), '1');
    assert_eq!(key_digit(PATH, src, "dt", REC_DIGIT), '2');
}

const FREQ_DIGIT: usize = 13;

#[test]
fn hot_fn_outranks_cold_fn_at_empty_prefix() {
    // `zz_hot` referenced 16x, `aa_cold` never -- ci-alphabetical order
    // OPPOSES, so only the frequency rung can flip it (same file, same kind).
    let mut body = String::new();
    for _ in 0..16 {
        body.push_str("  zz_hot()\n");
    }
    let src = format!("fn zz_hot() {{}}\nfn aa_cold() {{}}\nfn r() {{\n{body}  $0\n}}\n");
    let labels = labels_at(&src);
    assert!(
        rank_in(&labels, "zz_hot") < rank_in(&labels, "aa_cold"),
        "hot fn must outrank the never-referenced fn at empty prefix: {:?}...",
        &labels[..labels.len().min(10)]
    );
}

#[test]
fn freq_digits_bucket_by_reference_count() {
    let mut body = String::new();
    for _ in 0..16 {
        body.push_str("  zz_hot()\n");
    }
    for _ in 0..3 {
        body.push_str("  mm_warm()\n");
    }
    let src = format!(
        "fn zz_hot() {{}}\nfn mm_warm() {{}}\nfn aa_cold() {{}}\nfn r() {{\n{body}  $0\n}}\n"
    );
    assert_eq!(key_digit(PATH, &src, "zz_hot", FREQ_DIGIT), '0', "16 refs = hot bucket");
    assert_eq!(key_digit(PATH, &src, "mm_warm", FREQ_DIGIT), '1', "3 refs = warm bucket");
    assert_eq!(key_digit(PATH, &src, "aa_cold", FREQ_DIGIT), '2', "0 refs = rare bucket");
}

#[test]
fn assignment_rhs_to_a_local_type_directs_like_typed_let() {
    // `dest = $0` where `dest: Widget`: the RHS slot expects Widget, so a Widget
    // local outranks a Widget? or int local. Regression: a bare-ident assign
    // target is typed `()` by inference (place position), so the anchor gave no
    // type direction and every local tied at type_rank 0 -- a `Widget?` could
    // surface top in a slot that only accepts `Widget` (uncompilable). Field and
    // typed-let targets never had this; only bare local/param targets.
    let labels = labels_at(
        "struct Widget { id: int }\n\
         fn f(dest: Widget, maybe_w: Widget?, sure_w: Widget, num: int) {\n\
         dest = $0\n\
         }\n",
    );
    let sure = rank_of(&labels, "sure_w").expect("sure_w offered");
    let maybe = rank_of(&labels, "maybe_w").expect("maybe_w offered");
    let num = rank_of(&labels, "num").expect("num offered");
    assert!(sure < maybe, "Widget must outrank Widget? at assign RHS: {labels:?}");
    assert!(sure < num, "Widget must outrank int at assign RHS: {labels:?}");
}
