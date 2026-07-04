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
