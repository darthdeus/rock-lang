//! Context-AWARE completion invariants (docs/completion-v2-design.md M4):
//! fixtures that know their slot assert the typed-slot properties the
//! position-blind checker (`bone_query::invariants`, hooked into every
//! corpus helper and `rock completion-sweep --check`) cannot see.
//!
//! Not here: response-under-budget (`just compl-perf-gate` is that gate).

use bone_query::BoneWorkspace;
use lsp_types::Position;

const PATH: &str = "/tmp/rock_lsp_compl_invariants.rock";

/// Split a `$0`-marked source into (source, cursor Position).
fn marked(src: &str) -> (String, Position) {
    let off = src.find("$0").expect("fixture must contain the `$0` cursor marker");
    let before = &src[..off];
    let line = before.matches('\n').count() as u32;
    let col = (before.len() - before.rfind('\n').map(|i| i + 1).unwrap_or(0)) as u32;
    (src.replacen("$0", "", 1), Position::new(line, col))
}

fn items_at(src: &str) -> Vec<lsp_types::CompletionItem> {
    let (s, pos) = marked(src);
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, s);
    bone.completion(PATH, pos)
}

fn labels_at(src: &str) -> Vec<String> {
    items_at(src).into_iter().map(|c| c.label).collect()
}

fn rank_of(labels: &[String], needle: &str) -> Option<usize> {
    labels.iter().position(|l| l == needle)
}

// -- Invariant: a clean-matching right-type local is in the top K when the
//    slot type is known. -----------------------------------------------------

#[test]
fn right_type_local_in_top_three_of_typed_slot() {
    // The slot wants a Color; the local `col` IS one. However many same-prefix
    // fns/globals exist, the right-type local must sit in the visible top 3.
    let labels = labels_at(
        "value Color { r: float }\n\
         global COLD_START: int = 1\n\
         fn collect() {}\n\
         fn collide() {}\n\
         fn colorize() {}\n\
         fn paint(c: Color) {}\n\
         fn r() {\n  let col = Color { r: 1.0 }\n  paint(col$0)\n}\n",
    );
    let c = rank_of(&labels, "col").expect("local `col` must be offered");
    assert!(c < 3, "right-type local `col` must rank in the top 3 of a Color slot: {labels:?}");
}

#[test]
fn no_wrong_type_value_above_right_type_in_typed_slot() {
    // Same prefix, both locals: the Color-typed one outranks the int-typed one
    // in a Color slot -- type relevance dominates fine-score differences.
    let labels = labels_at(
        "value Color { r: float }\n\
         fn paint(c: Color) {}\n\
         fn r() {\n  let tint = Color { r: 1.0 }\n  let tin = 3\n  paint(ti$0)\n}\n",
    );
    let right = rank_of(&labels, "tint").expect("`tint: Color` must be offered");
    let wrong = rank_of(&labels, "tin").expect("`tin: int` must be offered");
    assert!(
        right < wrong,
        "right-type `tint` must outrank wrong-type `tin` in a Color slot: {labels:?}"
    );
}

// -- Invariant (SENTINEL): no Values-universe candidate in a TypePos slot. ----

#[test]
fn values_leak_into_param_type_slot_sentinel() {
    // PINNED BUG: a param TYPE slot (`w: W|`) still offers value-universe
    // candidates -- the param `w` itself, value-typed globals, and every
    // fuzzy-matching fn -- because `crate::query::completion` only uses
    // `expr_position` to switch enum rendering, not to gate the universe.
    // Correct behavior: a type slot offers types (structs/enums/newtypes/
    // type synths) only. If this assertion ever fails, the universe gating
    // landed -- flip both assertions to `!labels.contains(...)` and move
    // this test out of the sentinel section.
    let labels = labels_at(
        "struct Weapon { dmg: float }\n\
         global wcount: int = 3\n\
         fn wield(x: float) {}\n\
         fn use_weapon(w: W$0) {}\n",
    );
    assert!(
        labels.iter().any(|l| l == "wcount"),
        "PINNED BEHAVIOR (BUG): the value-typed global `wcount` is offered in a \
         param TYPE slot. If it is now absent, universe gating landed -- flip \
         this assertion and the one below."
    );
    assert!(
        labels.iter().any(|l| l == "wield"),
        "PINNED BEHAVIOR (BUG): the fn `wield` is offered in a param TYPE slot. \
         If it is now absent, universe gating landed -- flip this assertion."
    );
    // The type itself is present either way -- this half is the permanent
    // (non-sentinel) guard.
    assert!(labels.iter().any(|l| l == "Weapon"), "the struct must be offered: {labels:?}");
}
