//! F2 corpus: type-directed arg ranking across cases where the RENDERED type
//! strings differ but the types are call-compatible (generics, nullable
//! widening). Ranking runs bone's real `coerce` on TyIds (`ExpectedArg`,
//! docs/assignable-plan.md), so each case pins that a call-compatible
//! candidate takes the expected-type bucket over a type-incompatible decoy.

use bone_query::BoneWorkspace;
use lsp_types::Position;

const PATH: &str = "/tmp/rock_lsp_type_directed.rock";

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
