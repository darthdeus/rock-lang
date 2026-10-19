//! Context-AWARE completion invariants (completion pipeline contract M4):
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

// -- Invariant: no Values-universe candidate in a TypePos slot. ---------------

#[test]
fn param_type_slot_offers_types_only() {
    // A param TYPE slot (`w: W|`) offers the type universe only: structs /
    // bare enums / newtypes / builtins / type synths. Values (the param `w`
    // itself, value-typed globals) and functions are gated out.
    let labels = labels_at(
        "struct Weapon { dmg: float }\n\
         global wcount: int = 3\n\
         fn wield(x: float) {}\n\
         fn use_weapon(w: W$0) {}\n",
    );
    assert!(labels.iter().any(|l| l == "Weapon"), "the struct must be offered: {labels:?}");
    assert!(
        !labels.iter().any(|l| l == "wcount" || l == "w"),
        "value-universe candidates must not appear in a param TYPE slot: {labels:?}"
    );
    assert!(
        !labels.iter().any(|l| l == "wield"),
        "functions must not appear in a param TYPE slot: {labels:?}"
    );
}

#[test]
fn let_annotation_type_slot_offers_types_only() {
    // Same gate through a typed-`let` annotation slot (`let x: W| = ..`).
    let labels = labels_at(
        "struct Weapon { dmg: float }\n\
         global wcount: int = 3\n\
         fn wield(x: float) {}\n\
         fn r() {\n  let x: w$0 = 0\n}\n",
    );
    assert!(labels.iter().any(|l| l == "Weapon"), "the struct must be offered: {labels:?}");
    assert!(
        !labels.iter().any(|l| l == "wcount" || l == "wield"),
        "values/functions must not appear in a let-annotation TYPE slot: {labels:?}"
    );
}
