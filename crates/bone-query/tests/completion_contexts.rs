//! Completion contexts on the bone facade: struct-literal fields, and member
//! access (fields + UFCS methods). See `docs/lsp-bone-migration-plan.md`.

use bone_query::BoneWorkspace;
use lsp_types::{CompletionItem, Position};

const PATH: &str = "/tmp/rock_lsp_bone_completion_ctx.rock";

fn labels(src: &str, line: u32, ch: u32) -> Vec<String> {
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    let items = bone.completion(PATH, Position::new(line, ch));
    check_invariants(src, line, ch, &items);
    let mut out: Vec<String> = items.into_iter().map(|c| c.label).collect();
    out.sort();
    out
}

/// The completion item whose label is `label`, at the given cursor.
fn item(src: &str, line: u32, ch: u32, label: &str) -> CompletionItem {
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    let items = bone.completion(PATH, Position::new(line, ch));
    check_invariants(src, line, ch, &items);
    items
        .into_iter()
        .find(|c| c.label == label)
        .unwrap_or_else(|| panic!("no completion `{label}` at {line}:{ch}"))
}

/// Every context fixture doubles as an invariant-suite position
/// (docs/completion-v2-design.md M4): the position-blind response invariants
/// must hold in every completion context this file exercises.
fn check_invariants(src: &str, line: u32, ch: u32, items: &[CompletionItem]) {
    let off: usize =
        src.lines().take(line as usize).map(|l| l.len() + 1).sum::<usize>() + ch as usize;
    let head = &src.as_bytes()[..off.min(src.len())];
    let start = head
        .iter()
        .rposition(|b| !(b.is_ascii_alphanumeric() || *b == b'_'))
        .map_or(0, |i| i + 1);
    let prefix = String::from_utf8_lossy(&head[start..]).into_owned();
    let violations = bone_query::invariants::completion_response_violations(items, &prefix);
    assert!(
        violations.is_empty(),
        "response invariants violated at {line}:{ch} (prefix {prefix:?}): {violations:?}"
    );
}

#[test]
fn struct_literal_required_fields_rank_above_defaulted() {
    // In `Cfg { |` the fields WITHOUT defaults (`name`, `id`) must rank
    // above the defaulted ones (`scale`, `volume`) -- the literal is
    // incomplete until the required fields are supplied. Within each tier,
    // declaration order holds.
    let src = "struct Cfg { scale: float = 1.0, name: string, volume: float = 0.5, id: int }\n\
               fn r() {\n  let c = Cfg { \n}\n";
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    let items = bone.completion(PATH, Position::new(2, 16));
    check_invariants(src, 2, 16, &items);
    let ordered: Vec<String> = items.into_iter().map(|c| c.label).collect();
    let pos = |l: &str| ordered.iter().position(|x| x == l);
    let (name, id) = (pos("name: string"), pos("id: int"));
    let (scale, volume) = (pos("scale: float"), pos("volume: float"));
    assert!(name.is_some() && id.is_some() && scale.is_some() && volume.is_some(), "{ordered:?}");
    assert!(
        name < scale && name < volume && id < scale && id < volume,
        "required fields must rank above defaulted ones: {ordered:?}"
    );
    assert!(name < id, "declaration order within the required tier: {ordered:?}");
    assert!(scale < volume, "declaration order within the defaulted tier: {ordered:?}");
}

#[test]
fn member_ufcs_overloads_dedup_to_one_item() {
    // Two overloads of `rotate` on the receiver struct must surface as ONE
    // member item (same label, same insert -- the duplicate is popup noise;
    // the general path already dedups overloads by name, first-wins).
    let src = "struct FixVec2 { x: float, y: float }\n\
               fn rotate(v: FixVec2, angle: float) -> FixVec2 { v }\n\
               fn rotate(v: FixVec2, rhs: FixVec2) -> FixVec2 { v }\n\
               fn r(p: FixVec2) {\n  p.\n}\n";
    let l = labels(src, 4, 4);
    assert_eq!(l.iter().filter(|s| s.as_str() == "rotate").count(), 1, "{l:?}");
}

#[test]
fn struct_literal_offers_remaining_fields() {
    // `FixVec2 { ` -> the struct's fields (was: the global namespace).
    let src = "struct FixVec2 { x: float, y: float }\nfn r() {\n  let v = FixVec2 { \n}\n";
    let l = labels(src, 2, 19);
    assert!(l.contains(&"x: float".to_string()), "{l:?}");
    assert!(l.contains(&"y: float".to_string()), "{l:?}");
    assert!(!l.iter().any(|s| s == "Arena"), "must not leak global types: {l:?}");
}

#[test]
fn struct_literal_skips_already_written_fields() {
    let src = "struct FixVec2 { x: float, y: float }\nfn r() {\n  let v = FixVec2 { x: 1.0, \n}\n";
    // cursor after `x: 1.0, ` -> only `y` remains.
    let l = labels(src, 2, 27);
    assert!(l.contains(&"y: float".to_string()), "{l:?}");
    assert!(!l.contains(&"x: float".to_string()), "x already written: {l:?}");
}

#[test]
fn member_completion_offers_transitive_path_in_arg_position() {
    // At `tile_center(player.|)` -- member access in an arg position expecting
    // IVec2 -- offer the nested suffix path `movable.grid` (whose type matches),
    // not only the direct fields. Inserting it after `player.` yields
    // `player.movable.grid`.
    let src = "struct IVec2 { x: int }\n\
               struct Movable { grid: IVec2 }\n\
               struct Player { movable: Movable }\n\
               fn tile_center(g: IVec2) {}\n\
               fn r(player: Player) {\n  tile_center(player.)\n}\n";
    // cursor right after `player.`
    let it = item(src, 5, 21, "movable.grid: IVec2");
    assert_eq!(it.insert_text.as_deref(), Some("movable.grid"), "{it:?}");
}

#[test]
fn member_completion_offers_mixed_field_then_method_path_in_arg_position() {
    // At `tile_center(player.|)` expecting IVec2: a field-then-METHOD suffix
    // `movable.to_ivec()` (UFCS method on the intermediate `Movable`) is offered
    // alongside the field paths -- the reachability index walks mixed edges
    // through the receiver, not just fields.
    let src = "struct IVec2 { x: int }\n\
               struct Movable { grid: int }\n\
               struct Player { movable: Movable }\n\
               fn to_ivec(m: Movable) -> IVec2 { IVec2 { x: 0 } }\n\
               fn tile_center(g: IVec2) {}\n\
               fn r(player: Player) {\n  tile_center(player.)\n}\n";
    // cursor right after `player.`
    let it = item(src, 6, 21, "movable.to_ivec() -> IVec2");
    assert_eq!(it.insert_text.as_deref(), Some("movable.to_ivec()"), "{it:?}");
}

#[test]
fn member_field_completion_inserts_only_the_name() {
    // `player.mov` -> the popup label is `movable: Movable` (informative), but
    // accepting it must insert just `movable`, not the whole `movable: Movable`.
    let src = "struct Movable { grid: int }\nstruct Player { movable: Movable }\nfn r(player: Player) {\n  let z = player.\n}\n";
    let it = item(src, 3, 17, "movable: Movable");
    assert_eq!(it.insert_text.as_deref(), Some("movable"), "{it:?}");
}

#[test]
fn decorator_position_offers_declared_decorators() {
    // `@late_dra|` must complete to the declared `late_draw` decorator. The `@`
    // isn't part of the ident, so the word under the cursor is `late_dra` -- the
    // decorator context is detected by the preceding `@`, and candidates come
    // from the workspace's `deco` declarations (not the global fn/type namespace).
    let src = "deco init late_draw {}\ndeco init late_update {}\ndeco init update {}\n@late_dra\nfn my_ui() {}\n";
    // cursor at end of `@late_dra` (line 3, char 9).
    let l = labels(src, 3, 9);
    assert!(l.contains(&"late_draw".to_string()), "decorator completion: {l:?}");
    // Prefix filters: `late_update` also matches `late_`? no -- prefix is
    // `late_dra`, so only `late_draw` survives; `update` must not appear.
    assert!(!l.contains(&"update".to_string()), "prefix must filter out `update`: {l:?}");
    // Must not leak the global fn namespace into decorator position.
    assert!(!l.contains(&"my_ui".to_string()), "must not offer fns in decorator position: {l:?}");
}

#[test]
fn decorator_completion_inserts_only_the_name() {
    // Accepting `@late_dra` -> late_draw inserts the bare name (the `@` is
    // already typed), not `@late_draw` and not a call snippet.
    let src = "deco init late_draw {}\n@late_dra\nfn my_ui() {}\n";
    let it = item(src, 1, 9, "late_draw");
    assert_eq!(it.insert_text.as_deref(), Some("late_draw"), "{it:?}");
}

#[test]
fn member_access_offers_fields_and_ufcs_methods() {
    let src = "struct FixVec2 { x: float, y: float }\nfn dist(a: FixVec2) -> float { a.x }\nfn r() {\n  let v = FixVec2 { x: 1.0, y: 2.0 }\n  v.\n}\n";
    // cursor after `v.` (line 4).
    let l = labels(src, 4, 4);
    assert!(l.contains(&"x: float".to_string()), "field: {l:?}");
    assert!(l.contains(&"dist".to_string()), "UFCS method: {l:?}");
}

#[test]
fn member_completion_on_a_vec_receiver() {
    // `xs.<cursor>` where xs: [int]. `[T]` is bone's `Array` ctor, resolved as
    // the stdlib `Vec` struct ({data, len}); its `.len` field + Vec UFCS methods
    // must be offered. Regression: `struct_name_of_ty` returned None for `Array`
    // (not a `Nominal` ctor), so member completion came up empty and fell
    // through. Parked as the `.len` bug (`global POS_HIST: [FixVec2]`).
    //
    // Also pins the quality gates: the internal `_data` field is hidden (soft-
    // private via the `_` prefix -- no Vec special-case), and UFCS is
    // element-aware -- a `[int]` fn matches, a `[Foo]` fn does not.
    let src = "struct Foo { a: int }\n\
               fn takes_foos(fs: [Foo]) {}\n\
               fn takes_ints(v: [int]) {}\n\
               fn r(xs: [int]) {\n  xs.\n}\n";
    let l = labels(src, 4, 5); // after `xs.`
    assert!(l.contains(&"len: int".to_string()), "vec .len field: {l:?}");
    assert!(l.contains(&"push".to_string()), "generic Vec UFCS: {l:?}");
    assert!(l.contains(&"takes_ints".to_string()), "[int] UFCS matches element: {l:?}");
    assert!(!l.iter().any(|s| s.starts_with("_data")), "internal Vec `_data` hidden: {l:?}");
    assert!(!l.contains(&"takes_foos".to_string()), "[Foo] UFCS excluded by element: {l:?}");
}

#[test]
fn member_completion_on_a_plain_call_receiver() {
    // `find_spell().<cursor>` must offer the RETURN struct's fields. Regression:
    // `member_path` walked only Ident/FieldAccess receivers, returning None on a
    // FunctionCall receiver -- so member completion never fired and the request
    // fell through to scope completion (200 locals). Capture:
    // 1783458255-player.rock (`spell.find_spell().cast_tim`).
    let src = "struct SpellDef { cast_time: float, mana: int }\n\
               fn find_spell() -> SpellDef { SpellDef { cast_time: 1.0, mana: 2 } }\n\
               fn r() {\n  find_spell().\n}\n";
    let l = labels(src, 3, 15); // after `find_spell().`
    assert!(l.contains(&"cast_time: float".to_string()), "call-receiver fields: {l:?}");
    assert!(l.contains(&"mana: int".to_string()), "call-receiver fields: {l:?}");
}

#[test]
fn member_completion_on_a_ufcs_call_receiver() {
    // The real-capture shape: `spell.find_spell().<cursor>` where `find_spell`
    // is a UFCS method (first param is the receiver). The chained call's return
    // struct fields must be offered.
    let src = "struct SpellDef { cast_time: float, mana: int }\n\
               fn find_spell(kind: int) -> SpellDef { SpellDef { cast_time: 1.0, mana: 2 } }\n\
               fn r(spell: int) {\n  spell.find_spell().\n}\n";
    let l = labels(src, 3, 21); // after `spell.find_spell().`
    assert!(l.contains(&"cast_time: float".to_string()), "ufcs-call-receiver fields: {l:?}");
    assert!(l.contains(&"mana: int".to_string()), "ufcs-call-receiver fields: {l:?}");
}

#[test]
fn member_completion_offers_iterator_combinators() {
    // `it.<cursor>` where `it: VecIter<T>` (result of `xs.iter()`). The generic
    // `Iterator`-bounded combinators (`enumerate<I: Iterator<T>, T>(it: I)`,
    // `map`, `filter`, `collect`, ...) are UFCS-callable on ANY iterator receiver,
    // so completion must offer them alongside the struct's own `next`. Regression:
    // `ufcs_methods` matched only NOMINAL first params, so a generic first param
    // never surfaced -> `it.enu` returned 0 items. Capture:
    // 1784133159-wisp_intent.rock (`m.trail.iter().enu()`).
    let src = "fn r(xs: [int]) {\n  let it = xs.iter()\n  it.\n  let _n = 0\n}\n";
    let l = labels(src, 2, 5); // after `it.`
    assert!(l.contains(&"enumerate".to_string()), "iterator combinator: {l:?}");
    assert!(l.contains(&"map".to_string()), "iterator combinator: {l:?}");
    assert!(l.contains(&"filter".to_string()), "iterator combinator: {l:?}");
    assert!(l.contains(&"collect".to_string()), "iterator terminal: {l:?}");
    assert!(l.contains(&"next".to_string()), "direct struct method: {l:?}");
}

#[test]
fn iterator_combinators_offered_on_user_defined_iterator() {
    // A user struct with a conforming `next(recv) -> (bool, E)` satisfies
    // `Iterator` structurally -- the combinators are UFCS-callable on it too,
    // same rule bone uses for the bound.
    let src = "struct Counter { n: int }\n\
               fn Counter.next(c) -> (bool, int) { return (c.n <= 0, c.n) }\n\
               fn r() {\n  let c = Counter { n: 3 }\n  c.\n  let _x = 0\n}\n";
    let l = labels(src, 4, 4); // after `c.`
    assert!(l.contains(&"next".to_string()), "own method: {l:?}");
    assert!(l.contains(&"enumerate".to_string()), "combinator on user iterator: {l:?}");
    assert!(l.contains(&"map".to_string()), "combinator on user iterator: {l:?}");
}

#[test]
fn iterator_combinators_not_offered_when_next_is_nonconforming() {
    // A `next` that does NOT return `(bool, E)` (here `-> int`) does not make the
    // receiver an `Iterator` -- no combinators. Mirrors `bound_next_elem`.
    let src = "struct Fake { n: int }\n\
               fn Fake.next(f) -> int { return f.n }\n\
               fn r() {\n  let f = Fake { n: 3 }\n  f.\n  let _x = 0\n}\n";
    let l = labels(src, 4, 4); // after `f.`
    assert!(l.contains(&"next".to_string()), "own method: {l:?}");
    assert!(!l.contains(&"enumerate".to_string()), "no combinator on non-iterator next: {l:?}");
    assert!(!l.contains(&"map".to_string()), "no combinator on non-iterator next: {l:?}");
}

#[test]
fn iterator_combinators_not_offered_on_non_iterator() {
    // The combinator UFCS surface is gated on the receiver structurally
    // satisfying `Iterator` (a `next(recv) -> (bool, E)` in scope). A plain
    // struct with no `next` must NOT get `enumerate`/`map`/`collect`.
    let src = "struct Plain { a: int }\n\
               fn r() {\n  let p = Plain { a: 1 }\n  p.\n  let _n = 0\n}\n";
    let l = labels(src, 3, 4); // after `p.`
    assert!(l.contains(&"a: int".to_string()), "field present: {l:?}");
    assert!(!l.contains(&"enumerate".to_string()), "no combinator on non-iterator: {l:?}");
    assert!(!l.contains(&"map".to_string()), "no combinator on non-iterator: {l:?}");
    assert!(!l.contains(&"collect".to_string()), "no combinator on non-iterator: {l:?}");
}

#[test]
fn member_in_fn_returning_a_struct_is_not_a_struct_literal() {
    // Regression: `fn c() -> Result { return res. }` -- the `-> Result {` body
    // brace must NOT be read as a `Result { }` literal; `res.` is member access.
    let src = "struct Result { value: int, ok: bool }\nfn c() -> Result {\n  let res = Result { value: 1, ok: true }\n  return res.\n}\n";
    let l = labels(src, 3, 13);
    assert!(l.contains(&"value: int".to_string()), "member fields: {l:?}");
    assert!(l.contains(&"ok: bool".to_string()), "member fields: {l:?}");
}

#[test]
fn function_completion_carries_an_inline_signature() {
    // A function-name completion shows its signature: the full `fn ..` in the
    // detail panel, and the concise `(params) -> ret` inline (labelDetails) next
    // to the name in the popup.
    // `separation`, not a stdlib UFCS method name (`distance`/`dot`/... now
    // live in stdlib/vec.rock), so only the local fn completes here.
    let src = "fn separation(a: int, b: int) -> int { a }\nfn run() {\n  sep\n}\n";
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    let item = bone
        .completion(PATH, Position::new(2, 5))
        .into_iter()
        .find(|c| c.label == "separation")
        .expect("`separation` should be completed");
    assert_eq!(
        item.detail.as_deref(),
        Some("fn separation(a: int, b: int) -> int"),
        "detail panel = full signature"
    );
    // Accepts as a snippet that inserts `separation()` with the cursor inside.
    assert_eq!(item.insert_text.as_deref(), Some("separation($0)"));
    assert_eq!(item.insert_text_format, Some(lsp_types::InsertTextFormat::SNIPPET));
    let inline = item.label_details.and_then(|d| d.detail);
    assert_eq!(
        inline.as_deref(),
        Some("(a: int, b: int) -> int"),
        "inline labelDetails = signature minus `fn name`"
    );
}

#[test]
fn callee_completion_reuses_existing_open_paren_instead_of_doubling() {
    // `cw` over the callee in `world_to_screen(tile_center(c.grid))` leaves the
    // cursor directly before the `(`. Completing to `world_to_design` must NOT
    // insert the `name($0)` snippet (which yields `world_to_design()(...)`) --
    // the trailing `(` is already there, so insert the bare name.
    let src = "fn world_to_design(g: int) -> int { g }\n\
               fn r() {\n  world_to_des(tile_center)\n}\n";
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    // cursor after `world_to_des` (col 2 + 12), directly before `(`.
    let it = bone
        .completion(PATH, Position::new(2, 14))
        .into_iter()
        .find(|c| c.label == "world_to_design")
        .expect("`world_to_design` should be completed");
    assert!(
        it.insert_text.as_deref().map_or(true, |t| !t.contains('(')),
        "must not add call parens when a `(` already follows: {:?}",
        it.insert_text
    );
    assert_ne!(
        it.insert_text_format,
        Some(lsp_types::InsertTextFormat::SNIPPET),
        "no snippet template when reusing the existing parens: {it:?}"
    );
}

#[test]
fn ufcs_method_completion_reuses_existing_open_paren() {
    // Same paren-doubling guard on the member path: `cw` over the method in
    // `mob.poke(3)` then completing must insert bare `poke`, not `poke($0)`.
    let src = "struct Mob { hp: int }\n\
               fn poke(m: Mob, n: int) -> int { n }\n\
               fn r(mob: Mob) {\n  mob.pok(3)\n}\n";
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    // cursor after `mob.pok` (col 2 + 7), directly before `(`.
    let it = bone
        .completion(PATH, Position::new(3, 9))
        .into_iter()
        .find(|c| c.label == "poke")
        .expect("`poke` UFCS method should be completed");
    assert!(
        it.insert_text.as_deref().map_or(true, |t| !t.contains('(')),
        "UFCS method must not add parens when a `(` follows: {:?}",
        it.insert_text
    );
    assert_ne!(it.insert_text_format, Some(lsp_types::InsertTextFormat::SNIPPET), "{it:?}");
}

// -- One-shot field-ref method pre-fill (docs/fieldof-method-completion-plan.md).
// A UFCS method with a `FieldOf<recv, V>` param fills the field arg at method-
// selection time: `mob.kil` -> `mob.kill_co(.attack_co)`. The receiver is a
// bare generic (`o: T`), so `ufcs_methods` never surfaces it -- these items are
// enumerated separately, gated on the receiver having a matching field.

#[test]
fn field_ref_method_prefills_single_field() {
    // `Mob` has exactly one `int?` field -> `mob.kil` completes to
    // `kill_co(.attack_co)` as a snippet, filling the FieldOf arg in one step.
    let src = "struct Mob { health: int, attack_co: int?, name: string }\n\
               fn kill_co<T>(o: T, f: FieldOf<T, int?>) {}\n\
               fn r(mob: Mob) {\n  mob.kil\n}\n";
    let it = item(src, 3, 9, "kill_co");
    assert_eq!(it.insert_text.as_deref(), Some("kill_co(.attack_co)"), "{it:?}");
    assert_eq!(it.insert_text_format, Some(lsp_types::InsertTextFormat::SNIPPET), "{it:?}");
}

#[test]
fn field_ref_method_prefills_choice_on_multiple_fields() {
    // Two `int?` fields -> a snippet CHOICE placeholder, in declaration order.
    let src = "struct Mob { health: int, attack_co: int?, cast_co: int?, name: string }\n\
               fn kill_co<T>(o: T, f: FieldOf<T, int?>) {}\n\
               fn r(mob: Mob) {\n  mob.kil\n}\n";
    let it = item(src, 3, 9, "kill_co");
    assert_eq!(it.insert_text.as_deref(), Some("kill_co(${1|.attack_co,.cast_co|})"), "{it:?}");
    assert_eq!(it.insert_text_format, Some(lsp_types::InsertTextFormat::SNIPPET), "{it:?}");
}

#[test]
fn field_ref_method_absent_when_no_matching_field() {
    // No `int?` field -> the generic-receiver method never surfaces (it isn't
    // callable: there's no field to pass). `min`/`max`-class generics likewise
    // never leak into member completion.
    let src = "struct Mob { health: int, name: string }\n\
               fn kill_co<T>(o: T, f: FieldOf<T, int?>) {}\n\
               fn r(mob: Mob) {\n  mob.kil\n}\n";
    let l = labels(src, 3, 9);
    assert!(!l.iter().any(|s| s == "kill_co"), "kill_co must not surface without a matching field: {l:?}");
}

#[test]
fn field_ref_method_reuses_existing_open_paren() {
    // A `(` already follows -> insert the bare name (the arg-position path fills
    // the field once the cursor is inside the parens); no doubled parens.
    let src = "struct Mob { health: int, attack_co: int?, name: string }\n\
               fn kill_co<T>(o: T, f: FieldOf<T, int?>) {}\n\
               fn r(mob: Mob) {\n  mob.kil()\n}\n";
    let it = item(src, 3, 9, "kill_co");
    assert_eq!(it.insert_text.as_deref(), Some("kill_co"), "{it:?}");
    assert_ne!(it.insert_text_format, Some(lsp_types::InsertTextFormat::SNIPPET), "{it:?}");
}

#[test]
fn field_ref_method_prefills_nontrailing_fieldof_arg() {
    // `set_co`'s FieldOf is arg 0 of `(field, next)`, not the trailing param --
    // the field lands in slot 0 and `next` becomes a numbered placeholder.
    let src = "struct Mob { health: int, attack_co: int?, name: string }\n\
               fn set_co<T>(o: T, f: FieldOf<T, int?>, next: int?) {}\n\
               fn r(mob: Mob) {\n  mob.se\n}\n";
    let it = item(src, 3, 8, "set_co");
    assert_eq!(it.insert_text.as_deref(), Some("set_co(.attack_co, ${1:next})"), "{it:?}");
    assert_eq!(it.insert_text_format, Some(lsp_types::InsertTextFormat::SNIPPET), "{it:?}");
}

#[test]
fn member_completion_resolves_a_cross_file_receiver_type() {
    // a.rock defines IVec2; b.rock has a param of that type. Completing `g.` in
    // b.rock must resolve IVec2's fields from a.rock -- the completion reparse
    // includes the whole project, not just the edited file (was: empty).
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("stick.toml"), "[package]\nname = \"xp\"\n").unwrap();
    std::fs::write(dir.path().join("a.rock"), "struct IVec2 { x: int, y: int }\n").unwrap();
    let bpath = dir.path().join("b.rock");
    let bsrc = "fn f(g: IVec2) {\n  g.\n}\n";
    std::fs::write(&bpath, bsrc).unwrap();

    let mut bone = BoneWorkspace::new();
    let bp = bpath.to_str().unwrap();
    bone.update(bp, bsrc.to_string());
    // `g.` on line 1, after the dot.
    let labels: Vec<String> = bone
        .completion(bp, Position::new(1, 4))
        .into_iter()
        .map(|c| c.label)
        .collect();
    assert!(labels.contains(&"x: int".to_string()), "cross-file field x: {labels:?}");
    assert!(labels.contains(&"y: int".to_string()), "cross-file field y: {labels:?}");
}

#[test]
fn match_block_is_not_a_struct_literal() {
    // `match e { <here> }` must fall through to global completion, not be read
    // as an `e { }` literal (e is lowercase -> rejected anyway, but guard it).
    let src = "enum E { A, B }\nfn r(e: E) {\n  match e {\n    \n  }\n}\n";
    let l = labels(src, 3, 4);
    // Global namespace is non-empty (stdlib + user); the key point is it's not
    // an empty struct-field list.
    assert!(!l.is_empty(), "match block -> global completion, got empty");
}

#[test]
fn block_after_enum_variant_condition_is_not_a_record_literal() {
    // Regression: `if x == Enum::Variant { <here> }` -- the `if`-block brace must
    // NOT be read as an `Enum::Variant { }` record literal. A unit/tuple variant
    // has no record fields, so the enum-record path found nothing and returned an
    // empty completion list, silently killing ALL completion inside the block.
    // It must fall through to global completion instead.
    let src = "enum SpellKind { Fire, Dispel }\nfn spell_clear() {}\nfn fire(kind: SpellKind) {\n  if kind == SpellKind::Dispel {\n    spell_c\n  }\n}\n";
    // cursor after `spell_c` on the bare-statement line inside the block.
    let l = labels(src, 4, 11);
    assert!(l.contains(&"spell_clear".to_string()), "block-head variant must not eat completion: {l:?}");
}

#[test]
fn enum_record_literal_offers_variant_fields() {
    // The legit record-literal context: `Effect::Bandage { <here> }` offers the
    // variant's record fields, not the global namespace. Guards the `Some(fields)`
    // side of the block-head fall-through.
    let src = "enum Effect { None, Bandage { duration: float, potency: int } }\nfn r() {\n  let e = Effect::Bandage {\n  }\n}\n";
    let l = labels(src, 3, 2);
    assert!(l.contains(&"duration: float".to_string()), "{l:?}");
    assert!(l.contains(&"potency: int".to_string()), "{l:?}");
    assert!(!l.iter().any(|s| s == "Arena"), "must not leak globals: {l:?}");
}

#[test]
fn record_variant_condition_head_no_longer_eats_completion() {
    // A record variant as a comparison head (`if e == Effect::Bandage {`) is
    // an if-block brace, not a record literal: scope completion must work
    // inside the block. The AST classifier resolved the block-head ambiguity
    // the byte scanner couldn't (was a sentinel, flipped when F3 landed).
    let src = "enum Effect { None, Bandage { duration: float } }\nfn spell_clear() {}\nfn fire(e: Effect) {\n  if e == Effect::Bandage {\n    spell_c\n  }\n}\n";
    let l = labels(src, 4, 11);
    assert!(
        l.contains(&"spell_clear".to_string()),
        "scope completion must survive a record-variant condition head: {l:?}"
    );
}

#[test]
fn member_completion_on_cross_file_global_receiver() {
    // A global defined in ANOTHER file (Rock has one global namespace) must
    // still resolve as a member-access receiver: `player.` offers Player's
    // fields even though `player` is declared in types.rock, not use.rock.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("stick.toml"), "[package]\nname = \"p\"\n").unwrap();
    std::fs::write(
        dir.path().join("types.rock"),
        "struct Movable { grid: int }\nstruct Player { movable: Movable }\nglobal player: Player\n",
    )
    .unwrap();
    let use_path = dir.path().join("use.rock");
    std::fs::write(&use_path, "fn r() {\n  let z = player\n}\n").unwrap();
    let up = use_path.to_string_lossy().to_string();

    let mut bone = BoneWorkspace::new();
    bone.update(&up, "fn r() {\n  let z = player.\n}\n".to_string());
    // cursor right after `player.`
    let col = "  let z = player.".chars().count() as u32;
    let l: Vec<String> = bone
        .completion(&up, Position::new(1, col))
        .into_iter()
        .map(|c| c.label)
        .collect();
    assert!(
        l.iter().any(|s| s.starts_with("movable")),
        "cross-file global receiver should offer its struct's fields: {l:?}"
    );
}

// -- type-directed ranking in call-argument position ----------------------
// Inside `f(|)`, candidates whose type matches f's expected param type are
// ranked first (a variable of that type), then functions returning it, then
// everything else -- via `sort_text`.

#[test]
fn arg_position_ranks_matching_type_then_returning_fn() {
    let src = "struct SpellDef { kind: int }\n\
               fn spell_make() -> SpellDef { SpellDef { kind: 0 } }\n\
               fn spell_dps(s: SpellDef) -> float { 0.0 }\n\
               fn use_spell(s: SpellDef) {}\n\
               fn r() {\n  let spell: SpellDef = SpellDef { kind: 0 }\n  use_spell(spe)\n}\n";
    // cursor after `spe` inside `use_spell(...)` -- expected param type SpellDef.
    let var = item(src, 6, 15, "spell"); // local of type SpellDef -> rank 0
    let mk = item(src, 6, 15, "spell_make"); // fn -> SpellDef        -> rank 1
    let dps = item(src, 6, 15, "spell_dps"); // fn -> float           -> rank 2
    let st = |it: &CompletionItem| it.sort_text.clone().unwrap_or_default();
    assert!(
        st(&var) < st(&mk),
        "variable of the expected type ranks before a fn returning it: {:?} vs {:?}",
        var.sort_text, mk.sort_text
    );
    assert!(
        st(&mk) < st(&dps),
        "fn returning the expected type ranks before an unrelated fn: {:?} vs {:?}",
        mk.sort_text, dps.sort_text
    );
}

// -- general (non-arg) ranking: kind priority --------------------------------
// Outside an argument position there's no expected type, so candidates rank by
// KIND: variables (locals/params, then globals) first, then functions/structs,
// then enum-member variants last. Enum members are only top-priority in a typed
// arg position that expects their enum.
#[test]
fn non_arg_ranks_variables_before_functions_and_enum_members() {
    let src = "enum AggressorRef { PlayerRef, NpcRef }\n\
               enum ClickTarget { Player, Ground }\n\
               struct Player { name: string }\n\
               global player: Player? = null\n\
               global player_dead: bool = false\n\
               fn play_sound() {}\n\
               fn r() -> int {\n  play\n  return 0\n}\n";
    // cursor after `play` on line 7.
    let st = |label: &str| item(src, 7, 6, label).sort_text.clone().unwrap_or_default();
    let player = st("player");
    let player_dead = st("player_dead");
    let play_sound = st("play_sound");
    let struct_p = st("Player");
    let variant1 = st("AggressorRef::PlayerRef");
    let variant2 = st("ClickTarget::Player");

    assert!(player < player_dead, "player before player_dead: {player} vs {player_dead}");
    // variables outrank functions and structs.
    assert!(player_dead < play_sound, "variables before functions: {player_dead} vs {play_sound}");
    assert!(player_dead < struct_p, "variables before structs: {player_dead} vs {struct_p}");
    // the reported bug: enum-member variants must rank LAST -- below variables,
    // functions, and structs alike.
    assert!(player < variant1, "global var before enum member: {player} vs {variant1}");
    assert!(player < variant2, "global var before enum member: {player} vs {variant2}");
    assert!(play_sound < variant1, "function before enum member: {play_sound} vs {variant1}");
    assert!(struct_p < variant1, "struct before enum member: {struct_p} vs {variant1}");
}

// Regression: on an EMPTY prefix the MAX_COMPLETION_ITEMS cap must not truncate
// struct/type names away. Structs share the function bucket (not below it), so a
// type survives the cap even with the whole stdlib of functions present.
#[test]
fn empty_prefix_still_offers_struct_types() {
    let src = "struct Point { x: float, y: float }\n\
               fn create_point() -> Point { Point { x: 1.0, y: 2.0 } }\n\
               fn r() {\n  \n}\n";
    // cursor in the empty body, no prefix typed.
    let l = labels(src, 3, 2);
    assert!(l.contains(&"Point".to_string()), "struct type offered on empty prefix: {l:?}");
}

#[test]
fn arg_position_offers_field_path_of_matching_type() {
    // `sfx_spell` wants a SpellKind; `spell` (a SpellDef in scope) has a `kind:
    // SpellKind` field, so `spell.kind` should be offered as a completion (the
    // fuzzy query `speki` matches the path), not just the bare `spell`.
    let src = "struct SpellDef { kind: SpellKind }\n\
               enum SpellKind { Fire, Ice }\n\
               fn sfx_spell(k: SpellKind) {}\n\
               fn r() {\n  let spell: SpellDef = SpellDef { kind: SpellKind::Fire }\n  sfx_spell(speki)\n}\n";
    // cursor after `speki` inside `sfx_spell(...)`.
    // The label carries the type (`path: Type`); accepting inserts just the path.
    let it = item(src, 5, 17, "spell.kind: SpellKind");
    assert_eq!(it.insert_text.as_deref(), Some("spell.kind"), "{it:?}");
}

#[test]
fn arg_position_offers_recursive_field_path() {
    // `sfx(g: IVec2)` wants IVec2; `player.movable.grid` is IVec2 two levels deep.
    // The field-path search must recurse, not stop at depth 1.
    let src = "struct IVec2 { x: int }\n\
               struct Movable { grid: IVec2 }\n\
               struct Player { movable: Movable }\n\
               fn sfx(g: IVec2) {}\n\
               fn r(player: Player) {\n  sfx(plgr)\n}\n";
    // cursor after `plgr` inside `sfx(...)`.
    let it = item(src, 5, 10, "player.movable.grid: IVec2");
    assert_eq!(it.insert_text.as_deref(), Some("player.movable.grid"), "{it:?}");
}

#[test]
fn arg_position_offers_ufcs_method_path_of_matching_type() {
    // `spell_dps(s: SpellDef) -> float` is a UFCS method on SpellDef returning
    // float; `need_dps(d: float)` wants a float, so `spell.spell_dps()` (calling
    // the method on the in-scope `spell`) should be offered.
    let src = "struct SpellDef { kind: int }\n\
               fn spell_dps(s: SpellDef) -> float { 0.0 }\n\
               fn need_dps(d: float) {}\n\
               fn r() {\n  let spell: SpellDef = SpellDef { kind: 0 }\n  need_dps(spell_d)\n}\n";
    // cursor after `spell_d` inside `need_dps(...)`.
    let it = item(src, 5, 18, "spell.spell_dps() -> float");
    assert_eq!(it.insert_text.as_deref(), Some("spell.spell_dps()"), "{it:?}");
}

#[test]
fn arg_position_offers_mixed_field_then_method_path() {
    // `sfx(g: IVec2)` wants IVec2. `to_ivec(m: Movable) -> IVec2` is a UFCS
    // method on Movable, reachable as `player.movable.to_ivec()` (a FIELD hop
    // into `movable`, THEN a method call). The reachability index walks mixed
    // field-then-method edges, so this path is offered (a UFCS scan on the root
    // type alone never reaches a method on an intermediate field type). Fuzzy
    // query `plmoiv` subseq-matches the path.
    let src = "struct IVec2 { x: int }\n\
               struct Movable { grid: int }\n\
               struct Player { movable: Movable }\n\
               fn to_ivec(m: Movable) -> IVec2 { IVec2 { x: 0 } }\n\
               fn sfx(g: IVec2) {}\n\
               fn r(player: Player) {\n  sfx(plmoiv)\n}\n";
    // cursor after `plmoiv` inside `sfx(...)`.
    let it = item(src, 6, 12, "player.movable.to_ivec() -> IVec2");
    assert_eq!(it.insert_text.as_deref(), Some("player.movable.to_ivec()"), "{it:?}");
}

#[test]
fn reachability_index_rebuilds_on_an_interface_edit() {
    // The reachability index is rebuilt only on a non-BodyOnly edit. Adding a
    // field that reaches the expected type is an Interface edit, so a path that
    // didn't exist before must appear after the edit (stale index = miss).
    let path = "/tmp/rock_lsp_bone_reach_invalidate.rock";
    let before = "struct V { y: int }\nstruct W { a: int }\nfn need(v: V) {}\nfn r(w: W) {\n  need(wv)\n}\n";
    // W gains a `v: V` field -> `w.v` now reaches V.
    let after = "struct V { y: int }\nstruct W { v: V }\nfn need(v: V) {}\nfn r(w: W) {\n  need(wv)\n}\n";

    let mut bone = BoneWorkspace::new();
    bone.update(path, before.to_string());
    let has_wv = |bone: &mut BoneWorkspace| {
        bone.completion(path, Position::new(4, 9))
            .into_iter()
            .any(|c| c.insert_text.as_deref() == Some("w.v"))
    };
    assert!(!has_wv(&mut bone), "no `w.v` path before W has a V field");

    bone.update(path, after.to_string());
    assert!(has_wv(&mut bone), "`w.v` path appears after the interface edit (index rebuilt)");
}

#[test]
fn arg_position_ranks_prefix_match_above_scattered_subsequence() {
    // Typing `play` in `sfx(play)`: `player.movable.grid` (an exact prefix of the
    // root `player`) must rank ABOVE `PIXEL_LAYER.dims` (which only scatter-matches
    // `play` as p-i-x-e-L-...-l-A-Y-er) -- even though the PIXEL path is shorter.
    // Match quality dominates distance, and case is only a tiebreak (the UPPER_CASE
    // global must not win on ASCII order). Regression for the reported mis-ranking.
    let src = "struct IVec2 { x: int }\n\
               struct Movable { grid: IVec2 }\n\
               struct Player { movable: Movable }\n\
               struct Image { dims: IVec2 }\n\
               global PIXEL_LAYER: Image\n\
               fn sfx(g: IVec2) {}\n\
               fn r(player: Player) {\n  sfx(play)\n}\n";
    let pl = item(src, 7, 10, "player.movable.grid: IVec2");
    let px = item(src, 7, 10, "PIXEL_LAYER.dims: IVec2");
    assert!(
        pl.sort_text < px.sort_text,
        "prefix match should rank first: player={:?} pixel={:?}",
        pl.sort_text,
        px.sort_text
    );
}

#[test]
fn arg_position_offers_clean_path_and_filters_midword_scatter() {
    // Typing `pg` in `sfx(pg)`: `player.movable.grid` matches CLEANLY (p-layer .
    // g-rid -> word-boundary acronym), so it's offered. `prop.image_dims()` only
    // mid-word-scatters `pg` (pro-P / ima-G-e, the `g` is inside a word), so the
    // cleanliness filter drops it as noise rather than showing it ranked low.
    let src = "struct IVec2 { x: int }\n\
               struct Movable { grid: IVec2 }\n\
               struct Player { movable: Movable }\n\
               struct Prop { w: int }\n\
               fn image_dims(p: Prop) -> IVec2 { IVec2 { x: 0 } }\n\
               global prop: Prop\n\
               fn sfx(g: IVec2) {}\n\
               fn r(player: Player) {\n  sfx(pg)\n}\n";
    let l = labels(src, 8, 8);
    assert!(
        l.iter().any(|x| x == "player.movable.grid: IVec2"),
        "clean acronym path offered: {l:?}"
    );
    assert!(
        !l.iter().any(|x| x.starts_with("prop.image_dims")),
        "mid-word scatter path filtered as noise: {l:?}"
    );
}

#[test]
fn expr_position_completes_call_through_enum_variant_arg() {
    // Typing `rejtanog` in statement position completes the whole call
    // `reject_target(TargetError::NotGround)`: the fn name `reject_target`
    // absorbs `rejta`, and the remainder `nog` matches the enum-variant arg
    // `TargetError::NotGround`. Single-param fn, so no trailing snippet.
    let src = "enum TargetError { NotGround, OutOfRange }\n\
               fn reject_target(e: TargetError) {}\n\
               fn r() {\n  rejtanog\n}\n";
    // cursor after `rejtanog` (line 3, char 10).
    let it = item(src, 3, 10, "reject_target(TargetError::NotGround)");
    assert_eq!(
        it.insert_text.as_deref(),
        Some("reject_target(TargetError::NotGround)"),
        "{it:?}"
    );
}

#[test]
fn call_through_param_with_more_params_snippets_to_next_arg() {
    // A multi-param fn fills the first arg and snippets to the next slot:
    // `reject_target(TargetError::NotGround, $0)`.
    let src = "enum TargetError { NotGround }\n\
               fn reject_target(e: TargetError, reason: int) {}\n\
               fn r() {\n  rejtanog\n}\n";
    let it = item(src, 3, 10, "reject_target(TargetError::NotGround)");
    assert_eq!(
        it.insert_text.as_deref(),
        Some("reject_target(TargetError::NotGround, $0)"),
        "{it:?}"
    );
    assert_eq!(it.insert_text_format, Some(lsp_types::InsertTextFormat::SNIPPET), "{it:?}");
}

#[test]
fn call_through_param_fills_an_in_scope_value() {
    // The arg can also be an in-scope value of the param type, not just an enum
    // variant: `dt` (a float local) fills `apply_damage(amount: float)`.
    let src = "fn apply_damage(amount: float) {}\n\
               fn r() {\n  let dt: float = 1.0\n  apdt\n}\n";
    let it = item(src, 3, 6, "apply_damage(dt)");
    assert_eq!(it.insert_text.as_deref(), Some("apply_damage(dt)"), "{it:?}");
}

#[test]
fn fuzzy_bare_fn_matches_across_underscore_with_no_call_through_noise() {
    // `tilecen` finds the bare `tile_center` (subsequence across the `_`, which
    // strict-prefix would miss), carrying a sort_text. It produces NO
    // call-through: `tile_center` absorbs the whole query (a fn-NAME query), and
    // `table_new_with_size` matches `tilecen` only weakly (`ti` scattered, below
    // the half-of-query coverage the noise guard requires).
    let src = "global PIXEL_EDITOR_TOOL_CURRENT_X: int\n\
               fn tile_center(s: int) {}\n\
               fn table_new_with_size(n: int) {}\n\
               fn r() {\n  tilecen\n}\n";
    let bare = item(src, 4, 9, "tile_center");
    assert!(bare.sort_text.is_some(), "fuzzy bare fn offered + ranked: {bare:?}");
    let all = labels(src, 4, 9);
    assert!(
        !all.iter().any(|l| l.contains('(')),
        "no spurious call-through noise for a fn-name query: {all:?}"
    );
}

#[test]
fn call_through_suppressed_when_query_is_just_the_fn_name() {
    // `spawn` is fully absorbed by `spawn_thing`'s name -- the user is typing the
    // fn name, not into an arg -- so NO per-arg call-through flood
    // (`spawn_thing(N)` for every int in scope); the bare fn is offered (fuzzily)
    // and ranked instead.
    let src = "fn spawn_thing(n: int) {}\n\
               global N: int\n\
               fn r() {\n  spawn\n}\n";
    let l = labels(src, 3, 7);
    assert!(l.iter().any(|x| x == "spawn_thing"), "bare fn offered: {l:?}");
    assert!(
        !l.iter().any(|x| x.starts_with("spawn_thing(")),
        "no per-arg call-through flood: {l:?}"
    );
    // Every offered item is ranked (no None sort_text mixing).
    assert!(item(src, 3, 7, "spawn_thing").sort_text.is_some());
}

#[test]
fn call_through_fills_arg_from_a_tuple_variant() {
    // `dealfir` -> `deal(Damage::Fire($0))`: the arg is a TUPLE enum variant of
    // the param type, so it carries a payload snippet (cursor in the `($0)`).
    let src = "enum Damage { Fire(int), Cold }\n\
               fn deal(d: Damage) {}\n\
               fn r() {\n  dealfir\n}\n";
    let it = item(src, 3, 9, "deal(Damage::Fire(...))");
    assert_eq!(it.insert_text.as_deref(), Some("deal(Damage::Fire($0))"), "{it:?}");
    assert_eq!(it.insert_text_format, Some(lsp_types::InsertTextFormat::SNIPPET));
}

#[test]
fn call_through_fills_arg_from_a_record_variant() {
    // A RECORD variant arg carries a brace payload snippet: `apply(Cfg::Opts { $0 })`.
    let src = "enum Cfg { Opts { volume: int }, Mute }\n\
               fn apply(c: Cfg) {}\n\
               fn r() {\n  applyopt\n}\n";
    let it = item(src, 3, 10, "apply(Cfg::Opts { ... })");
    assert_eq!(it.insert_text.as_deref(), Some("apply(Cfg::Opts { $0 })"), "{it:?}");
    assert_eq!(it.insert_text_format, Some(lsp_types::InsertTextFormat::SNIPPET));
}

#[test]
fn call_through_tuple_variant_arg_with_more_params_uses_two_slots() {
    // Multi-param fn: the payload takes `$1`, the trailing param `$0`:
    // `deal(Damage::Fire($1), $0)`.
    let src = "enum Damage { Fire(int), Cold }\n\
               fn deal(d: Damage, n: int) {}\n\
               fn r() {\n  dealfir\n}\n";
    let it = item(src, 3, 9, "deal(Damage::Fire(...))");
    assert_eq!(it.insert_text.as_deref(), Some("deal(Damage::Fire($1), $0)"), "{it:?}");
}

#[test]
fn arg_position_ranks_expected_enum_variant_first() {
    // Inside `reject_target(nog)` expecting a `TargetError`, the matching variant
    // `TargetError::NotGround` (typed by variant name `nog`) is a VALUE of the
    // expected type, so it ranks in bucket 0 -- not buried in the catch-all
    // bucket 2 under unrelated functions.
    let src = "enum TargetError { NotGround, OutOfRange }\n\
               fn reject_target(e: TargetError) {}\n\
               fn r() {\n  reject_target(nog)\n}\n";
    let it = item(src, 3, 19, "TargetError::NotGround");
    // Arg layout is `{exact}{type_rank}..`; `nog` is not an exact name, so the
    // expected-type bucket is the second digit.
    assert!(
        it.sort_text.as_deref().is_some_and(|s| s.starts_with("00") || s.starts_with("10")),
        "expected-type variant should rank in type bucket 0: {:?}",
        it.sort_text
    );
}

#[test]
fn plain_completion_demotes_midword_scatter_below_clean_and_camelcase() {
    // `nog` (3 chars -> subseq admission) mid-word-scatters `stone_log`
    // (sto-N-e ... l-O-G): ADMITTED under the default subseq3 policy but DEMOTED
    // to band 3. It cleanly matches the camelCase variant `NotGround`
    // (N-o ... -G-round, the `G` a word boundary) -- band 2 -- which therefore
    // ranks ABOVE the scatter. Admission is permissive; the band does the work.
    let src = "fn stone_log() {}\n\
               enum E { NotGround }\n\
               fn r() {\n  nog\n}\n";
    // `item` panics if the label is absent, so reaching both proves admission.
    let ng = item(src, 3, 5, "E::NotGround");
    let sl = item(src, 3, 5, "stone_log");
    assert!(
        ng.sort_text < sl.sort_text,
        "clean camelCase must rank above mid-word scatter: {:?} vs {:?}",
        ng.sort_text,
        sl.sort_text,
    );
    // The same function is still found by a clean (word-boundary) query.
    let l2 = labels("fn stone_log() {}\nfn r() {\n  stlog\n}\n", 2, 7);
    assert!(l2.iter().any(|x| x == "stone_log"), "clean query finds the fn: {l2:?}");
}

#[test]
fn arg_position_ranks_expected_type_global_first() {
    // Inside `apply_cfg(gam)` expecting a `Cfg`, the global `GAME_CFG` (a Cfg) is
    // a value of the expected type, so it ranks in bucket 0 -- gaps must carry
    // the global's type in `detail` for the rank check to see it.
    let src = "struct Cfg { x: int }\n\
               global GAME_CFG: Cfg\n\
               fn apply_cfg(c: Cfg) {}\n\
               fn r() {\n  apply_cfg(gam)\n}\n";
    let it = item(src, 4, 15, "GAME_CFG");
    // Arg layout is `{exact}{type_rank}..`; `gam` is not an exact name, so the
    // expected-type bucket is the second digit.
    assert!(
        it.sort_text.as_deref().is_some_and(|s| s.starts_with("00") || s.starts_with("10")),
        "expected-type global should rank in type bucket 0: {:?}",
        it.sort_text
    );
}

#[test]
fn call_through_fills_arg_from_a_reach_path() {
    // `tilecpmg` -> `tile_center(player.movable.grid)`: the fn name absorbs
    // `tilec`, and the remainder `pmg` matches a REACH PATH to the param type
    // (player.movable.grid, an IVec2 reachable from the in-scope `player`), not
    // just a direct value / enum variant.
    let src = "struct IVec2 { x: int }\n\
               struct Movable { grid: IVec2 }\n\
               struct Player { movable: Movable }\n\
               fn tile_center(g: IVec2) {}\n\
               fn r(player: Player) {\n  tilecpmg\n}\n";
    let it = item(src, 5, 10, "tile_center(player.movable.grid)");
    assert_eq!(it.insert_text.as_deref(), Some("tile_center(player.movable.grid)"), "{it:?}");
}

// -- qualified enum variants in value position ----------------------------
// In a value/expression position the bare enum name is not a usable value, so
// completion offers the qualified `Enum::Variant` forms directly (and hides the
// bare enum). In a type position it's the inverse: the bare `Enum` is the type,
// and `Enum::Variant` is meaningless. Mirrors rock-lsp.

#[test]
fn value_position_offers_qualified_variants_not_bare_enum() {
    let src = "enum SpellState { Idle, Loaded(int) }\nfn r() {\n  let s = Spe\n}\n";
    let l = labels(src, 2, 13);
    assert!(l.contains(&"SpellState::Idle".to_string()), "{l:?}");
    assert!(l.contains(&"SpellState::Loaded".to_string()), "{l:?}");
    assert!(!l.contains(&"SpellState".to_string()), "bare enum hidden in value position: {l:?}");
}

#[test]
fn type_position_offers_bare_enum_not_qualified_variants() {
    // A struct-def field is a type slot: the bare `Enum` is the type, its
    // qualified variants are not. (Re-completing a valid, fully-typed name --
    // the type slot bone can infer; a partial type identifier doesn't resolve,
    // a separate gap.) The classifier's full case matrix is covered by the
    // `backend::position` unit tests.
    let src = "enum SpellState { Idle, Loaded(int) }\nstruct Holder { field: SpellState }\n";
    // cursor at the end of `SpellState`.
    let l = labels(src, 1, 33);
    assert!(l.contains(&"SpellState".to_string()), "{l:?}");
    assert!(!l.iter().any(|s| s.starts_with("SpellState::")), "no qualified in type position: {l:?}");
}

#[test]
fn value_position_qualified_variant_matches_variant_name() {
    // typing the variant name (not the enum) still surfaces the qualified form.
    let src = "enum SpellState { Idle, Loaded(int) }\nfn r() {\n  let s = idl\n}\n";
    let l = labels(src, 2, 13);
    assert!(l.contains(&"SpellState::Idle".to_string()), "variant-name-led match: {l:?}");
}

#[test]
fn value_position_qualified_tuple_variant_inserts_parens() {
    // the payload template applies to the qualified form too.
    let src = "enum SpellState { Idle, Loaded(int) }\nfn r() {\n  let s = Load\n}\n";
    let it = item(src, 2, 14, "SpellState::Loaded");
    assert_eq!(it.insert_text.as_deref(), Some("SpellState::Loaded($0)"), "{it:?}");
    assert_eq!(it.insert_text_format, Some(lsp_types::InsertTextFormat::SNIPPET));
}

#[test]
fn no_param_function_completion_parks_cursor_after_parens() {
    // `tick()` has no params -> insert `tick()` with the cursor AFTER the `)`,
    // not a snippet tabstop parked inside empty parens.
    let src = "fn tick() -> int { 0 }\nfn run() {\n  tic\n}\n";
    let it = item(src, 2, 5, "tick");
    assert_eq!(it.insert_text.as_deref(), Some("tick()"), "{it:?}");
    assert_eq!(
        it.insert_text_format, None,
        "no params -> plain insert, cursor lands after `)`: {it:?}"
    );
}

#[test]
fn no_arg_ufcs_method_completion_parks_cursor_after_parens() {
    // `c.reset()` -- the receiver fills the only param, so 0 args remain:
    // insert `reset()` with the cursor after `)`. `c.add(n)` still has an arg
    // left, so it stays a `add($0)` snippet.
    let src = "struct Counter { value: int }\nfn reset(c: Counter) -> int { 0 }\nfn add(c: Counter, n: int) -> int { 0 }\nfn run(c: Counter) {\n  c.\n}\n";
    let reset = item(src, 4, 4, "reset");
    assert_eq!(reset.insert_text.as_deref(), Some("reset()"), "{reset:?}");
    assert_eq!(reset.insert_text_format, None, "{reset:?}");
    let add = item(src, 4, 4, "add");
    assert_eq!(add.insert_text.as_deref(), Some("add($0)"), "{add:?}");
    assert_eq!(add.insert_text_format, Some(lsp_types::InsertTextFormat::SNIPPET));
}

// -- enum-variant payload templates (literal positions only) --------------
// Completing `Enum::Variant` in a *value* position fills the payload so the
// caller doesn't have to: a tuple variant inserts `Variant($0)`, a record
// variant inserts `Variant { $0 }`, both as snippets with the cursor parked
// inside. A unit variant inserts the bare name. In a *pattern* position
// (match arm) no template is added -- patterns bind, they don't construct.

#[test]
fn tuple_variant_in_value_position_inserts_parens() {
    let src = "enum SpellState { Idle, Loaded(int) }\nfn r() {\n  let s = SpellState::\n}\n";
    // cursor right after `SpellState::`.
    let it = item(src, 2, 22, "Loaded");
    assert_eq!(it.insert_text.as_deref(), Some("Loaded($0)"), "{it:?}");
    assert_eq!(it.insert_text_format, Some(lsp_types::InsertTextFormat::SNIPPET));
}

#[test]
fn record_variant_in_value_position_inserts_braces() {
    let src = "enum Effect { None, Bandage { duration: float } }\nfn r() {\n  let e = Effect::\n}\n";
    // cursor right after `Effect::`.
    let it = item(src, 2, 18, "Bandage");
    assert_eq!(it.insert_text.as_deref(), Some("Bandage { $0 }"), "{it:?}");
    assert_eq!(it.insert_text_format, Some(lsp_types::InsertTextFormat::SNIPPET));
}

#[test]
fn unit_variant_in_value_position_has_no_template() {
    let src = "enum SpellState { Idle, Loaded(int) }\nfn r() {\n  let s = SpellState::\n}\n";
    let it = item(src, 2, 22, "Idle");
    assert_eq!(it.insert_text, None, "unit variant inserts the bare name: {it:?}");
    assert_eq!(it.insert_text_format, None);
}

#[test]
fn tuple_variant_in_pattern_position_has_no_template() {
    // A match-arm pattern constructs nothing -- `Loaded($0)` would be wrong.
    let src = "enum SpellState { Idle, Loaded(int) }\nfn r(s: SpellState) {\n  match s {\n    SpellState:: => {}\n  }\n}\n";
    // cursor right after `SpellState::` in the arm pattern.
    let it = item(src, 3, 16, "Loaded");
    assert_eq!(it.insert_text, None, "pattern position: no payload template: {it:?}");
    assert_eq!(it.insert_text_format, None);
}

#[test]
fn tuple_variant_in_fresh_match_arm_has_no_template() {
    // A FRESH arm with no `=>` typed yet is still a pattern position: the forward
    // `=>` scan finds nothing, so the enclosing-`match` backward scan must catch
    // it. `Loaded($0)` would be wrong (patterns bind, they don't construct).
    let src = "enum SpellState { Idle, Loaded(int) }\nfn r(s: SpellState) {\n  match s {\n    SpellState::\n  }\n}\n";
    // cursor right after `SpellState::` in the fresh arm (no `=>`).
    let it = item(src, 3, 16, "Loaded");
    assert_eq!(it.insert_text, None, "fresh pattern position: no payload template: {it:?}");
    assert_eq!(it.insert_text_format, None);
}

#[test]
fn partial_variant_in_fresh_match_arm_has_no_template() {
    // Same, with a partially-typed variant name and no `=>` yet.
    let src = "enum SpellState { Idle, Loaded(int) }\nfn r(s: SpellState) {\n  match s {\n    SpellState::Loa\n  }\n}\n";
    // cursor right after `SpellState::Loa`.
    let it = item(src, 3, 19, "Loaded");
    assert_eq!(it.insert_text, None, "fresh partial pattern position: no template: {it:?}");
    assert_eq!(it.insert_text_format, None);
}

#[test]
fn variant_in_arm_body_after_fat_arrow_still_templates() {
    // `A => SpellState::|` is the arm BODY -- a value position -- even though the
    // enclosing block is a match. The `=>` before the path must keep it literal.
    let src = "enum SpellState { Idle, Loaded(int) }\nfn r(s: SpellState) {\n  match s {\n    SpellState::Idle => SpellState::\n  }\n}\n";
    // cursor right after the second `SpellState::` (the arm body).
    let it = item(src, 3, 36, "Loaded");
    assert_eq!(
        it.insert_text.as_deref(),
        Some("Loaded($0)"),
        "arm-body value position still templates: {it:?}"
    );
    assert_eq!(it.insert_text_format, Some(lsp_types::InsertTextFormat::SNIPPET));
}

#[test]
fn variant_at_block_start_outside_match_still_templates() {
    // Adversarial guard for the backward scan: a variant as the first statement
    // of a plain block (NOT a match arm) is a value position and MUST template.
    // The preceding `{` is the fn-body block, not a match open.
    let src = "enum SpellState { Idle, Loaded(int) }\nfn r() {\n  SpellState::\n}\n";
    // cursor right after `SpellState::`.
    let it = item(src, 2, 14, "Loaded");
    assert_eq!(
        it.insert_text.as_deref(),
        Some("Loaded($0)"),
        "block-start value position still templates: {it:?}"
    );
    assert_eq!(it.insert_text_format, Some(lsp_types::InsertTextFormat::SNIPPET));
}

#[test]
fn enum_qualifier_unmatched_prefix_offers_all_variants() {
    // `TextAlign::top` -- the in-progress `top` matches no variant, so a strict
    // prefix filter returns nothing and the client falls back to buffer words
    // (the "completion is failing" feeling). The enum is a closed set: surface
    // every variant so the user sees the valid options instead of editor noise.
    let src = "enum TextAlign { Left, Center, Right }\nfn r() {\n  let a = TextAlign::top\n}\n";
    // cursor at end of `top` (line 2, col 24).
    let ls = labels(src, 2, 24);
    assert_eq!(ls, vec!["Center", "Left", "Right"], "all variants shown: {ls:?}");
}

#[test]
fn enum_qualifier_unmatched_prefix_forces_items_visible() {
    // The fallback variants must carry `filter_text = <typed prefix>` so they
    // survive the CLIENT's own prefix filter (cmp re-matches `top` against each
    // item; without this they'd be hidden again). The list is `is_incomplete`,
    // so the client re-requests every keystroke and the prefix stays fresh.
    let src = "enum TextAlign { Left, Center, Right }\nfn r() {\n  let a = TextAlign::top\n}\n";
    let it = item(src, 2, 24, "Center");
    assert_eq!(
        it.filter_text.as_deref(),
        Some("top"),
        "forced-visible variant carries the typed prefix as filter_text: {it:?}"
    );
}

#[test]
fn enum_qualifier_matching_prefix_still_narrows() {
    // Regression guard: when the prefix DOES match, only matching variants show
    // (normal narrowing), and they keep the default filter_text (no forcing).
    let src = "enum TextAlign { Left, Center, Right }\nfn r() {\n  let a = TextAlign::Ce\n}\n";
    let ls = labels(src, 2, 23);
    assert_eq!(ls, vec!["Center"], "matching prefix narrows: {ls:?}");
    let it = item(src, 2, 23, "Center");
    assert_eq!(it.filter_text, None, "narrowed item keeps default filter_text");
}

// -- builtin types in a type slot ------------------------------------------
// `Coroutine` and `Map` are compiler builtins (not stdlib structs), so the
// workspace name-index can't surface them. In a TYPE slot the type-name
// builtins (`Coroutine`, `Map`, `int`, `float`, `string`, `bool`, `any`,
// `Unit`, NativeBuffer, `Ptr`) are first-class candidates.

#[test]
fn type_slot_offers_coroutine_builtin() {
    // `-> Co|` in a return-type slot expands the `Coroutine` builtin to its
    // generic snippet (`Coroutine<$0>`); the bare name is replaced (no Wait
    // struct here, so only the generic form is offered).
    let src = "co fn f() -> Co {\n}\n";
    let l = labels(src, 0, 15);
    assert!(l.contains(&"Coroutine<>".to_string()), "{l:?}");
    assert!(!l.contains(&"Coroutine".to_string()), "bare Coroutine replaced by the snippet: {l:?}");
}

#[test]
fn type_slot_offers_map_builtin() {
    // `let m: Ma|` offers the `Map` builtin.
    let src = "fn r() {\n  let m: Ma\n}\n";
    let l = labels(src, 1, 11);
    assert!(l.contains(&"Map".to_string()), "{l:?}");
}

#[test]
fn type_slot_offers_primitive_builtins() {
    // Param type slot offers primitive types. Assert on `string`/`bool` -- they
    // don't collide with stdlib function names (`int`/`float`/`any` do, so their
    // mere presence wouldn't prove the type builtin was added).
    let src = "fn f(x: stri) {\n}\n";
    let l = labels(src, 0, 12);
    assert!(l.contains(&"string".to_string()), "missing string: {l:?}");
    // And `bool` as a type-kind item (CLASS/KEYWORD/STRUCT), not the absent fn.
    let it = item("fn f(x: boo) {\n}\n", 0, 11, "bool");
    assert!(
        matches!(
            it.kind,
            Some(lsp_types::CompletionItemKind::CLASS | lsp_types::CompletionItemKind::KEYWORD | lsp_types::CompletionItemKind::STRUCT)
        ),
        "bool should be a type-kind completion: {it:?}"
    );
}

#[test]
fn expr_slot_does_not_offer_coroutine_builtin() {
    // In an EXPRESSION slot the bare type name `Coroutine` is not a value, so it
    // must not be offered. `let x = Co|`.
    let src = "fn r() {\n  let x = Co\n}\n";
    let l = labels(src, 1, 12);
    assert!(!l.contains(&"Coroutine".to_string()), "type builtin leaked into expr slot: {l:?}");
}

// -- yield / await inside a coroutine body ---------------------------------
// `yield` and `await` are keywords usable only inside a `co fn`. Offer them as
// keyword completions there (and NOT inside a plain `fn`). After the keyword,
// coroutine-returning functions rank first (`await frame(1)` delegation).

#[test]
fn co_fn_body_offers_yield_keyword() {
    let src = "co fn f() -> Coroutine<int> {\n  yi\n}\n";
    let l = labels(src, 1, 4);
    assert!(l.contains(&"yield".to_string()), "{l:?}");
}

#[test]
fn co_fn_body_offers_await_keyword() {
    let src = "co fn f() -> Coroutine<int> {\n  aw\n}\n";
    let l = labels(src, 1, 4);
    assert!(l.contains(&"await".to_string()), "{l:?}");
}

#[test]
fn plain_fn_body_does_not_offer_yield_keyword() {
    let src = "fn f() -> int {\n  yi\n}\n";
    let l = labels(src, 1, 4);
    assert!(!l.contains(&"yield".to_string()), "yield leaked into plain fn: {l:?}");
    assert!(!l.contains(&"await".to_string()), "await leaked into plain fn: {l:?}");
}

#[test]
fn await_ranks_coroutine_fn_first() {
    // `await fr|` -- a coroutine fn (`frame`) ranks above a same-prefix
    // non-coroutine fn (`fragment`). Both match the prefix; the co fn wins.
    let src = "co fn frame(n: int) -> Coroutine<int> {\n  yield 0\n}\n\
               fn fragment(n: int) -> int { n }\n\
               co fn g() -> Coroutine<int> {\n  await fr\n}\n";
    // line 5 (0-based): `  await fr`, cursor after `fr` = col 10.
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    let items: Vec<String> = bone
        .completion(PATH, Position::new(5, 10))
        .into_iter()
        .map(|c| c.label)
        .collect();
    let pframe = items.iter().position(|s| s == "frame");
    let pfrag = items.iter().position(|s| s == "fragment");
    assert!(pframe.is_some() && pfrag.is_some(), "both fns present: {items:?}");
    assert!(pframe < pfrag, "coroutine `frame` must rank above `fragment`: {items:?}");
}

// -- Coroutine<Wait> ergonomics -----------------------------------------
// `Wait` is the dominant yield type in Stone games (~75% of all
// `Coroutine<..>` annotations), so completion surfaces it: it ranks first in
// the first generic arg slot, the `Coroutine` type name expands to
// `Coroutine<Wait>`/`Coroutine<$0>` snippets, and a `co fn ()` signature
// offers the `-> Coroutine<Wait>` return type. All gated on a `Wait`
// struct existing (stone projects); otherwise the generic `Coroutine<$0>` form
// stands in.

const COFRAME: &str = "struct Wait { wait_frame: bool }\n";

#[test]
fn coframe_ranks_first_in_empty_coroutine_generic_slot() {
    let src = format!("{COFRAME}co fn f() -> Coroutine<> {{\n  yield 0\n}}\n");
    let col = "co fn f() -> Coroutine<".len() as u32;
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src);
    let items: Vec<String> = bone.completion(PATH, Position::new(1, col)).into_iter().map(|c| c.label).collect();
    assert_eq!(items.first().map(String::as_str), Some("Wait"),
        "Wait must rank first in Coroutine<|>: {:?}", &items[..items.len().min(8)]);
}

#[test]
fn coframe_not_synthesized_without_a_cofframe_struct() {
    // No Wait struct -> no special boost; normal type completion.
    let src = "co fn f() -> Coroutine<> {\n  yield 0\n}\n";
    let col = "co fn f() -> Coroutine<".len() as u32;
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    let items: Vec<String> = bone.completion(PATH, Position::new(0, col)).into_iter().map(|c| c.label).collect();
    assert!(items.first().map(String::as_str) != Some("Wait"), "no Wait boost without the struct: {items:?}");
}

#[test]
fn coroutine_name_expands_to_cofframe_and_generic_snippets() {
    let src = format!("{COFRAME}co fn f() -> Cor {{\n  yield 0\n}}\n");
    let col = "co fn f() -> Cor".len() as u32;
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src);
    let items = bone.completion(PATH, Position::new(1, col));
    let labels: Vec<&str> = items.iter().map(|c| c.label.as_str()).collect();
    assert_eq!(labels.first(), Some(&"Coroutine<Wait>"), "first: {labels:?}");
    assert_eq!(labels.get(1), Some(&"Coroutine<>"), "second: {labels:?}");
    assert!(!labels.contains(&"Coroutine"), "bare Coroutine must be suppressed: {labels:?}");
    let snip = items.iter().find(|c| c.label == "Coroutine<>").unwrap();
    assert_eq!(snip.insert_text.as_deref(), Some("Coroutine<$0>"), "{snip:?}");
    assert_eq!(snip.insert_text_format, Some(lsp_types::InsertTextFormat::SNIPPET), "{snip:?}");
    let cf = items.iter().find(|c| c.label == "Coroutine<Wait>").unwrap();
    assert_eq!(cf.insert_text.as_deref(), Some("Coroutine<Wait>"), "{cf:?}");
}

#[test]
fn coroutine_name_without_cofframe_offers_only_generic_snippet() {
    let src = "co fn f() -> Cor {\n  yield 0\n}\n";
    let col = "co fn f() -> Cor".len() as u32;
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    let labels: Vec<String> = bone.completion(PATH, Position::new(0, col)).into_iter().map(|c| c.label).collect();
    assert!(labels.iter().any(|s| s == "Coroutine<>"), "generic snippet present: {labels:?}");
    assert!(!labels.iter().any(|s| s == "Coroutine<Wait>"), "no Wait snippet without the struct: {labels:?}");
    assert!(!labels.iter().any(|s| s == "Coroutine"), "bare Coroutine suppressed: {labels:?}");
}

#[test]
fn co_fn_signature_offers_coroutine_return_type() {
    // `co fn spawn() |` (no return type) -> `-> Coroutine<Wait>` to the right.
    let src = format!("{COFRAME}co fn spawn() {{\n  yield 0\n}}\n");
    let col = "co fn spawn() ".len() as u32;
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src);
    let items = bone.completion(PATH, Position::new(1, col));
    let labels: Vec<&str> = items.iter().map(|c| c.label.as_str()).collect();
    assert_eq!(labels.first(), Some(&"-> Coroutine<Wait>"), "return-type snippet must rank first: {labels:?}");
    let it = items.iter().find(|c| c.label == "-> Coroutine<Wait>").unwrap();
    assert_eq!(it.insert_text.as_deref(), Some("-> Coroutine<Wait>"), "{it:?}");
}

#[test]
fn plain_fn_signature_does_not_offer_coroutine_return_type() {
    // Only `co fn` triggers the return-type snippet, not a plain `fn`.
    let src = format!("{COFRAME}fn spawn() {{\n}}\n");
    let col = "fn spawn() ".len() as u32;
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src);
    let labels: Vec<String> = bone.completion(PATH, Position::new(1, col)).into_iter().map(|c| c.label).collect();
    assert!(!labels.iter().any(|s| s.starts_with("-> Coroutine")), "plain fn must not offer the return type: {labels:?}");
}

// The real Stone `Wait` is an ENUM, not a struct -- the ergonomics must fire
// for the shape that actually ships, not just the throwaway struct above.
const WAIT_ENUM: &str = "enum Wait { Frame, Secs(float) }\n";

#[test]
fn wait_ergonomics_fire_for_an_enum_wait() {
    // `Coroutine<Wait>` / `-> Coroutine<Wait>` snippets are the unambiguous
    // synth signal -- ordinary type completion can never produce them (unlike a
    // bare `Wait`, which the enum symbol itself offers in a type slot).
    let src = format!("{WAIT_ENUM}co fn f() -> Cor {{\n  yield 0\n}}\n");
    let col = "co fn f() -> Cor".len() as u32;
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src);
    let labels: Vec<String> = bone.completion(PATH, Position::new(1, col)).into_iter().map(|c| c.label).collect();
    assert_eq!(labels.first().map(String::as_str), Some("Coroutine<Wait>"),
        "Coroutine<Wait> snippet must synth for an enum Wait: {labels:?}");
}

// -- param-name + fn-prefix type heuristics --------------------------------
// In a param TYPE slot, a param named `e`/`entity` suggests the `Entity` type,
// and the enclosing fn's name prefix picks the payload: `mob_attack_co` -> a
// `Mob` struct -> a synthesized `Entity<Mob>` ranked first. UFCS-style naming
// (`<type>_<verb>`) makes the prefix a reliable type signal.

#[test]
fn param_entity_hint_synthesizes_entity_of_fn_prefix_struct() {
    let src = "struct Mob { health: int }\nco fn mob_attack_co(e: ) -> Coroutine<int> {\n  yield 1\n}\n";
    let col = "co fn mob_attack_co(e: ".len() as u32;
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    let items: Vec<String> = bone.completion(PATH, Position::new(1, col)).into_iter().map(|c| c.label).collect();
    assert!(items.contains(&"Entity<Mob>".to_string()), "missing Entity<Mob>: {items:?}");
    assert!(items.contains(&"Entity".to_string()), "missing bare Entity: {items:?}");
    let p_em = items.iter().position(|s| s == "Entity<Mob>").unwrap();
    let p_e = items.iter().position(|s| s == "Entity").unwrap();
    assert!(p_em < p_e, "Entity<Mob> must rank above bare Entity: {items:?}");
    assert_eq!(p_em, 0, "Entity<Mob> must be the top suggestion: {items:?}");
}

#[test]
fn entity_synth_inserts_the_generic_type() {
    let src = "struct Mob { health: int }\nco fn mob_attack_co(e: ) -> Coroutine<int> {\n  yield 1\n}\n";
    let col = "co fn mob_attack_co(e: ".len() as u32;
    let it = item(src, 1, col, "Entity<Mob>");
    let inserted = it.insert_text.as_deref().unwrap_or("Entity<Mob>");
    assert_eq!(inserted, "Entity<Mob>", "{it:?}");
}

#[test]
fn param_entity_hint_without_payload_offers_generic_snippet() {
    // fn prefix `render` -> no `Render` struct -> no concrete payload, so the
    // suggestion is the `Entity<$0>` snippet (cursor in the generic slot), top.
    let src = "co fn render_thing(e: ) -> Coroutine<int> {\n  yield 1\n}\n";
    let col = "co fn render_thing(e: ".len() as u32;
    let it = item(src, 0, col, "Entity<>");
    assert_eq!(it.insert_text.as_deref(), Some("Entity<$0>"), "{it:?}");
    assert_eq!(it.insert_text_format, Some(lsp_types::InsertTextFormat::SNIPPET), "{it:?}");
    let items: Vec<String> = labels(src, 0, col);
    assert!(!items.iter().any(|s| s == "Entity<Mob>"), "no concrete payload: {items:?}");
}

#[test]
fn entity_hint_works_in_a_plain_fn_at_the_colon_and_on_the_space() {
    // Not just `co fn`: a plain `fn clear_attack(e:` -> `Entity<>` snippet,
    // whether the cursor sits right after the `:` or after the space(s).
    let src = "fn clear_attack(e: ) {\n}\n";
    for col in ["fn clear_attack(e:".len(), "fn clear_attack(e: ".len()] {
        let it = item(src, 0, col as u32, "Entity<>");
        assert_eq!(it.insert_text.as_deref(), Some("Entity<$0>"), "at col {col}: {it:?}");
    }
}

#[test]
fn non_entity_param_name_does_not_synthesize() {
    let src = "struct Mob { health: int }\nco fn mob_attack_co(count: ) -> Coroutine<int> {\n  yield 1\n}\n";
    let col = "co fn mob_attack_co(count: ".len() as u32;
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    let items: Vec<String> = bone.completion(PATH, Position::new(1, col)).into_iter().map(|c| c.label).collect();
    assert!(!items.iter().any(|s| s == "Entity<Mob>"), "no synth for a non-entity param name: {items:?}");
}

#[test]
fn entity_payload_from_a_non_leading_fn_segment() {
    // `<verb>_<type>` naming (`spawn_mob`) is as common as `<type>_<verb>`:
    // the payload struct is the trailing noun, not the leading segment. Each
    // `_`-segment is tried, leading-first, so `spawn_mob(e:` -> `Entity<Mob>`.
    let src = "struct Mob { health: int }\nco fn spawn_mob(e: ) -> Coroutine<int> {\n  yield 1\n}\n";
    let col = "co fn spawn_mob(e: ".len() as u32;
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    let items: Vec<String> = bone.completion(PATH, Position::new(1, col)).into_iter().map(|c| c.label).collect();
    assert!(items.contains(&"Entity<Mob>".to_string()), "missing Entity<Mob>: {items:?}");
    assert_eq!(items.iter().position(|s| s == "Entity<Mob>"), Some(0), "Entity<Mob> must rank first: {items:?}");
}

#[test]
fn leading_fn_segment_wins_over_a_later_match() {
    // When more than one segment names a struct, the leading one (the UFCS
    // `<type>_<verb>` convention) wins: `mob_to_tile` -> `Entity<Mob>`.
    let src = "struct Mob { health: int }\nstruct Tile { x: int }\nco fn mob_to_tile(e: ) -> Coroutine<int> {\n  yield 1\n}\n";
    let col = "co fn mob_to_tile(e: ".len() as u32;
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    let items: Vec<String> = bone.completion(PATH, Position::new(2, col)).into_iter().map(|c| c.label).collect();
    assert!(items.contains(&"Entity<Mob>".to_string()), "missing Entity<Mob>: {items:?}");
    assert!(!items.contains(&"Entity<Tile>".to_string()), "leading segment must win: {items:?}");
}

#[test]
fn let_binding_named_e_is_not_treated_as_a_param() {
    // `let e: |` is a local binding, not a param -- must NOT synthesize.
    let src = "struct Mob { health: int }\nfn mob_attack_co() {\n  let e: \n}\n";
    let col = "  let e: ".len() as u32;
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    let items: Vec<String> = bone.completion(PATH, Position::new(2, col)).into_iter().map(|c| c.label).collect();
    assert!(!items.iter().any(|s| s == "Entity<Mob>"), "let binding must not trigger param heuristic: {items:?}");
}

#[test]
fn let_binding_is_not_in_scope_within_its_own_initializer() {
    // `let x = <init>` -- the binding `x` is not yet in scope in its own RHS
    // (non-recursive let), so completing inside the initializer must not offer
    // `x`. A non-empty init is required for `x` to be lowered at all; the cursor
    // sits between `=` and the value. Earlier bindings still complete.
    let src = "fn r() {\n  let speed = 5\n  let x = speed\n}\n";
    let col = "  let x = ".len() as u32;
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    let items: Vec<String> = bone.completion(PATH, Position::new(2, col)).into_iter().map(|c| c.label).collect();
    assert!(!items.iter().any(|s| s == "x"), "the just-declared `x` must not be in scope in its own init: {items:?}");
    assert!(items.iter().any(|s| s == "speed"), "an earlier local must still complete: {items:?}");
}

#[test]
fn let_binding_is_in_scope_after_its_statement() {
    // Once the let statement is complete, the binding completes normally on a
    // following line.
    let src = "fn r() {\n  let myvar = 5\n  let y = my\n}\n";
    let col = "  let y = my".len() as u32;
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    let items: Vec<String> = bone.completion(PATH, Position::new(2, col)).into_iter().map(|c| c.label).collect();
    assert!(items.iter().any(|s| s == "myvar"), "a completed local must be in scope on a later line: {items:?}");
}

// -- lifecycle-hook keywords complete as a block ---------------------------
// `on_killed` / `on_resume` / `on_hot_reload` are contextual lifecycle-hook
// keywords with a statement BLOCK form (`on_killed { .. }`), parsed specially.
// Completing them must insert `name { $0 }`, not the fn-call `name($0)` -- even
// though they're also regular stdlib fns.

#[test]
fn lifecycle_hooks_complete_as_a_block() {
    let src = "co fn f() -> Coroutine<int> {\n  on_kil\n  yield 1\n}\n";
    let it = item(src, 1, "  on_kil".len() as u32, "on_killed");
    assert_eq!(it.insert_text.as_deref(), Some("on_killed { $0 }"), "{it:?}");
    assert_eq!(it.insert_text_format, Some(lsp_types::InsertTextFormat::SNIPPET), "{it:?}");

    let src2 = "co fn f() -> Coroutine<int> {\n  on_hot\n  yield 1\n}\n";
    let it2 = item(src2, 1, "  on_hot".len() as u32, "on_hot_reload");
    assert_eq!(it2.insert_text.as_deref(), Some("on_hot_reload { $0 }"), "{it2:?}");
}

// -- Entity receiver: get() first + get().unwrap() chain --------------------
// `Entity<T>` is almost always dereferenced via `.get()` (-> T?) then
// `.unwrap()`. On `e.` offer `get()` first; on `e.getu` offer the whole
// `get().unwrap()` chain.

#[test]
fn entity_member_get_ranks_first() {
    let src = "struct Mob { health: int }\nfn f(e: Entity<Mob>) {\n  e.\n}\n";
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    let mut items = bone.completion(PATH, Position::new(2, 4));
    items.sort_by(|a, b| a.sort_text.cmp(&b.sort_text));
    let labels: Vec<&str> = items.iter().map(|c| c.label.as_str()).collect();
    assert_eq!(labels.first(), Some(&"get"), "get must rank first on an Entity receiver: {labels:?}");
}

#[test]
fn entity_getu_prefix_offers_get_unwrap_chain() {
    let src = "struct Mob { health: int }\nfn f(e: Entity<Mob>) {\n  e.getu\n}\n";
    let it = item(src, 2, "  e.getu".len() as u32, "get().unwrap()");
    assert_eq!(it.insert_text.as_deref(), Some("get().unwrap()"), "{it:?}");
}

#[test]
fn entity_payload_offered_at_colon_and_through_trailing_spaces() {
    // The concrete `Entity<Mob>` is returned right after the `:` AND after one or
    // two trailing spaces -- the detection skips whitespace back to the param
    // colon. (Auto-popping ON a space is a client trigger-char concern, not this.)
    let src = "struct Mob { health: int }\nco fn mob_attack_co(e:   ) -> Coroutine<int> {\n  yield 1\n}\n";
    for col in [
        "co fn mob_attack_co(e:".len(),
        "co fn mob_attack_co(e: ".len(),
        "co fn mob_attack_co(e:  ".len(),
    ] {
        let it = item(src, 1, col as u32, "Entity<Mob>");
        assert_eq!(it.insert_text.as_deref(), Some("Entity<Mob>"), "at col {col}: {it:?}");
    }
}

// -- `_`-prefixed fields are soft-private in member completion --------------
// A field named `_foo` is an implementation detail: member completion hides it
// unless the typed prefix itself starts with `_`. All fields stay public (no
// type-check change) -- this is purely a completion-display convention, general
// to every struct.

#[test]
fn member_completion_hides_underscore_fields_by_default() {
    let src = "struct Widget { value: int, _internal: int }\nfn f(w: Widget) {\n  w.\n}\n";
    let l = labels(src, 2, 4);
    assert!(l.iter().any(|s| s.starts_with("value")), "public field shown: {l:?}");
    assert!(!l.iter().any(|s| s.starts_with("_internal")), "underscore field hidden: {l:?}");
}

#[test]
fn member_completion_shows_underscore_fields_when_typing_underscore() {
    let src = "struct Widget { value: int, _internal: int }\nfn f(w: Widget) {\n  w._\n}\n";
    let l = labels(src, 2, 5);
    assert!(l.iter().any(|s| s.starts_with("_internal")), "underscore field shown when typing `_`: {l:?}");
}

// -- UFCS methods on an inferred-type receiver ------------------------------
// A local with no declared type (inferred from a call / method chain) must
// still surface its UFCS methods, exactly as it already surfaces its fields.
// Regression: `struct_name_for_path` (methods) lacked the inference-first
// branch that `struct_fields_for_path` (fields) has, so `m = make_mob()` showed
// fields but no methods (mag `mobs.rock` `m = e.get().unwrap(); m.resolve_...`).

#[test]
fn ufcs_methods_resolve_on_an_inferred_local() {
    let src = "struct Mob { hp: int }\n\
               fn make_mob() -> Mob { Mob { hp: 1 } }\n\
               fn resolve_strike(mob: Mob) {}\n\
               fn r() {\n  let m = make_mob()\n  m.\n}\n";
    let l = labels(src, 5, 4);
    assert!(l.contains(&"resolve_strike".to_string()), "UFCS method on inferred local: {l:?}");
    assert!(l.iter().any(|s| s.starts_with("hp")), "fields still present: {l:?}");
}

#[test]
fn ufcs_methods_resolve_on_a_chained_inferred_local() {
    // Mirrors mag exactly: `m = e.get().unwrap()` where get()->T?, unwrap()->T.
    let src = "struct Mob { hp: int }\n\
               fn resolve_strike(mob: Mob) {}\n\
               fn r(e: Entity<Mob>) {\n  let m = e.get().unwrap()\n  m.resol\n}\n";
    let l = labels(src, 4, "  m.resol".len() as u32);
    assert!(l.contains(&"resolve_strike".to_string()), "UFCS method on chained inferred local: {l:?}");
}

// -- shorter prefix match ranks first --------------------------------------
// `mob.attack` -> `attacking` must rank above `attack_timer`: both are equal
// prefix matches of `attack`, so the tiebreak should prefer the SHORTER name
// (tighter match). Without a length tiebreak the alphabetical fallback puts
// `attack_timer` first (`_` < `i`).

#[test]
fn member_field_completion_ranks_shorter_prefix_match_first() {
    let src = "struct Mob { attack_timer: float, attacking: bool }\nfn f(mob: Mob) {\n  if mob.attack\n}\n";
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    let mut items = bone.completion(PATH, Position::new(2, "  if mob.attack".len() as u32));
    items.sort_by(|a, b| a.sort_text.cmp(&b.sort_text));
    let labels: Vec<&str> = items.iter().map(|c| c.label.as_str()).collect();
    let p_ing = labels.iter().position(|s| s.starts_with("attacking"));
    let p_timer = labels.iter().position(|s| s.starts_with("attack_timer"));
    assert!(p_ing.is_some() && p_timer.is_some(), "both fields present: {labels:?}");
    assert!(p_ing < p_timer, "attacking (shorter) must rank above attack_timer: {labels:?}");
}

// -- bool-context soft boost: investigation battery -------------------------
// In `if`/`while <chain>` (the chain is the whole condition), `bool` fields get
// a SOFT boost: they win a score tie, but never override a better textual match.
// These tests probe the boundaries to confirm it helps without hurting.

fn member_labels(src: &str, line: u32, ch: u32) -> Vec<String> {
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    let mut items = bone.completion(PATH, Position::new(line, ch));
    items.sort_by(|a, b| a.sort_text.cmp(&b.sort_text));
    items.into_iter().map(|c| c.label).collect()
}

const DOOR: &str = "struct Door { ajar: float, accessible: bool }\n";

#[test]
fn bool_ctx_boosts_bool_field_over_a_shorter_nonbool() {
    // bool field is LONGER -- the boost must beat the length tiebreak in an if.
    let l = member_labels(&format!("{DOOR}fn f(door: Door) {{\n  if door.a\n}}\n"), 2, "  if door.a".len() as u32);
    let pa = l.iter().position(|s| s.starts_with("accessible"));
    let pj = l.iter().position(|s| s.starts_with("ajar"));
    assert!(pa < pj, "in `if door.a`, bool `accessible` should beat float `ajar`: {l:?}");
}

#[test]
fn non_bool_ctx_keeps_shorter_first() {
    // NOT a condition -> no boost; the shorter `ajar` wins (length tiebreak).
    let l = member_labels(&format!("{DOOR}fn f(door: Door) {{\n  let x = door.a\n}}\n"), 2, "  let x = door.a".len() as u32);
    let pa = l.iter().position(|s| s.starts_with("accessible"));
    let pj = l.iter().position(|s| s.starts_with("ajar"));
    assert!(pj < pa, "outside a condition, shorter `ajar` wins: {l:?}");
}

#[test]
fn operator_before_chain_is_not_a_bool_ctx() {
    // `if x && door.a` -- an operator precedes the chain -> unknown context.
    let src = format!("{DOOR}fn f(door: Door, x: bool) {{\n  if x && door.a\n}}\n");
    let l = member_labels(&src, 2, "  if x && door.a".len() as u32);
    let pa = l.iter().position(|s| s.starts_with("accessible"));
    let pj = l.iter().position(|s| s.starts_with("ajar"));
    assert!(pj < pa, "after `&&` the bool boost must NOT apply: {l:?}");
}

#[test]
fn better_textual_match_beats_the_bool_boost() {
    // `if door.aj` -- `ajar` is a far better match for `aj` than `accessible`
    // (which has no `aj`), so score dominates and the boost doesn't override it.
    let l = member_labels(&format!("{DOOR}fn f(door: Door) {{\n  if door.aj\n}}\n"), 2, "  if door.aj".len() as u32);
    assert_eq!(l.first().map(|s| s.as_str()), Some("ajar: float"), "score beats the soft boost: {l:?}");
}

#[test]
fn while_condition_is_a_bool_ctx() {
    let l = member_labels(&format!("{DOOR}fn f(door: Door) {{\n  while door.a\n}}\n"), 2, "  while door.a".len() as u32);
    let pa = l.iter().position(|s| s.starts_with("accessible"));
    let pj = l.iter().position(|s| s.starts_with("ajar"));
    assert!(pa < pj, "`while door.a` is a bool context: {l:?}");
}

#[test]
fn entity_internal_fields_are_hidden_on_member_access() {
    // Entity's internal fields (`_arena`/`_index`/`_generation`) are `_`-prefixed
    // -> hidden on `e.` by the soft-private rule. The method API (get, remove, ...)
    // still shows. Guards the stdlib field rename that closed the abstraction leak.
    let src = "struct Mob { hp: int }\nfn f(e: Entity<Mob>) {\n  e.\n}\n";
    let l = labels(src, 2, 4);
    assert!(
        !l.iter().any(|s| s.starts_with("arena:") || s.starts_with("index:") || s.starts_with("generation:")),
        "Entity internal fields must not leak on `e.`: {l:?}"
    );
    assert!(l.contains(&"get".to_string()), "methods still shown: {l:?}");
}

#[test]
fn empty_prefix_orders_alphabetically_not_by_length() {
    // At an EMPTY prefix there's no match to be "tighter" about, so a longer
    // name must NOT be pushed below a shorter one by the length tiebreak. Probed
    // on a small member-field set (no result cap to confound the order); the
    // workspace-level effect (short stdlib names burying longer user symbols
    // past the cap) is guarded end-to-end by `partial_compilation`'s
    // `completion_after_valid_then_broken`.
    let l = member_labels("struct S { aaa_long: int, z: int }\nfn f(s: S) {\n  s.\n}\n", 2, 4);
    let pa = l.iter().position(|s| s.starts_with("aaa_long"));
    let pz = l.iter().position(|s| s.starts_with("z:"));
    assert!(pa < pz, "empty-prefix member order is alphabetical, not length: {l:?}");
}

// -- arg-position type-directed boost for direct member fields --------------
// Inside `callee(.., recv.<prefix>)` with a known expected param type, a direct
// field whose type MATCHES that param ranks first -- like the global completion
// path and reach-paths already do. Without it, a field is ranked on bare prefix
// score, so `mob.at` in a Coroutine slot put `attack_timer` (float) above
// `attack_co` (Coroutine) just because its fuzzy score was 2 points higher.

#[test]
fn arg_position_boosts_member_field_matching_expected_type() {
    // (`cor`, not `co` -- `co` is the coroutine keyword and won't parse as a param.)
    let src = "struct Mob {\n  attack_timer: float,\n  attack_co: Coroutine<int>?,\n}\n\
               fn poll_co(arena: Arena<Mob>, e: Entity<Mob>, cor: Coroutine<int>?) {}\n\
               fn r(mob: Mob, arena: Arena<Mob>, e: Entity<Mob>) {\n  poll_co(arena, e, mob.at)\n}\n";
    let col = "  poll_co(arena, e, mob.at".len() as u32;
    let l = member_labels(src, 6, col);
    let pco = l.iter().position(|s| s.starts_with("attack_co"));
    let pt = l.iter().position(|s| s.starts_with("attack_timer"));
    assert!(pco.is_some() && pt.is_some(), "both fields present: {l:?}");
    assert!(pco < pt, "type-matching `attack_co` must rank first in the Coroutine arg slot: {l:?}");
}

// -- field-reference key paths (`FieldOf<Struct, V>`) ------------------------
// A leading `.field` in a call-arg position whose expected param is
// `FieldOf<Struct, V>` completes to Struct's fields whose declared type is
// EXACTLY V -- the invariance the typechecker enforces. Presence AND absence
// matter (a `contains` .rock.lsp can't assert the exclusion).

#[test]
fn field_ref_arg_completes_only_fields_matching_v() {
    let src = "struct Mob { health: int, attack_co: int?, name: string }\n\
               fn poll_slot(f: FieldOf<Mob, int?>) {}\n\
               fn main() {\n  poll_slot(.)\n}\n";
    // cursor right after the `.` in `poll_slot(.)`
    let col = "  poll_slot(.".len() as u32;
    let l = labels(src, 3, col);
    // Labels carry the leading `.` (the field ref IS `.attack_co`).
    assert!(l.contains(&".attack_co: int?".to_string()), "int? field offered: {l:?}");
    assert!(!l.iter().any(|s| s.starts_with(".health")), "int field excluded (invariant): {l:?}");
    assert!(!l.iter().any(|s| s.starts_with(".name")), "string field excluded: {l:?}");
    assert!(!l.iter().any(|s| s == "Arena" || s == "poll_slot"), "no global dump: {l:?}");
}

#[test]
fn field_ref_arg_v_int_offers_the_int_field() {
    let src = "struct Mob { health: int, attack_co: int?, name: string }\n\
               fn want_int(f: FieldOf<Mob, int>) {}\n\
               fn main() {\n  want_int(.)\n}\n";
    let col = "  want_int(.".len() as u32;
    let l = labels(src, 3, col);
    assert!(l.contains(&".health: int".to_string()), "int field offered: {l:?}");
    assert!(!l.iter().any(|s| s.starts_with(".attack_co")), "int? field excluded: {l:?}");
}

// generic T resolved from a sibling arg: poll_slot<T>(e: Entity<T>, f:
// FieldOf<T, int?>) at `poll_slot(e, .co)` -- T is bound to Mob from `e`.
#[test]
fn field_ref_generic_t_resolved_from_sibling_arg() {
    let src = "struct Mob { hp: int, co: int? }\n\
               struct Entity<T> { i: int }\n\
               fn poll_slot<T>(e: Entity<T>, f: FieldOf<T, int?>) {}\n\
               fn r() {\n  let e: Entity<Mob> = Entity { i: 0 }\n  poll_slot(e, .co)\n}\n";
    let col = "  poll_slot(e, .co".len() as u32;
    let l = labels(src, 5, col);
    assert!(l.contains(&".co: int?".to_string()), "T=Mob resolved, co offered: {l:?}");
    assert!(!l.iter().any(|s| s.starts_with(".hp")), "hp (int) excluded by invariance: {l:?}");
}

// empty-arg slot (no `.` typed yet): the FieldOf fields are offered proactively,
// and the insert text adds the leading dot.
#[test]
fn field_ref_empty_arg_offers_fields_and_inserts_dot() {
    let src = "struct Mob { hp: int, co: int? }\n\
               fn poll_slot(f: FieldOf<Mob, int?>) {}\n\
               fn r() {\n  poll_slot()\n}\n";
    // cursor right inside `poll_slot(|)`
    let col = "  poll_slot(".len() as u32;
    let it = item(src, 3, col, ".co: int?");
    assert_eq!(it.insert_text.as_deref(), Some(".co"), "empty-arg insert adds the dot: {it:?}");
    let l = labels(src, 3, col);
    assert!(!l.iter().any(|s| s.starts_with(".hp")), "hp (int) excluded: {l:?}");
    assert!(!l.iter().any(|s| s == "poll_slot"), "no global dump in a FieldOf slot: {l:?}");
}

// -- locality outranks case: a lowercase query still surfaces globals ---------
// Typing lowercase `jlog` fuzzy-matches the all-caps global
// `JIT_TRACE_LOG_ENABLED` and the lowercase fn `journal_log` in the same band.
// Locality wins (`kind` above `case`): a global outranks a fn even though the
// lowercase query case-matches the fn -- case is only a late boost. This is the
// inverse of the old "lowercase demotes SCREAMING_CONST" rule (case used to sit
// above kind); the uppercase-query test below still pins smart-case as a boost.

#[test]
fn lowercase_query_still_ranks_global_above_fn_despite_case() {
    let src = "global JIT_TRACE_LOG_ENABLED: bool = false\n\
               fn journal_log() {}\n\
               fn r() {\n  jlog\n}\n";
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    let items = bone.completion(PATH, Position::new(3, 6));
    let st = |label: &str| -> String {
        items
            .iter()
            .find(|c| c.label == label)
            .unwrap_or_else(|| {
                panic!(
                    "no completion `{label}`: {:?}",
                    items.iter().map(|c| c.label.as_str()).collect::<Vec<_>>()
                )
            })
            .sort_text
            .clone()
            .expect("sort_text")
    };
    let fn_st = st("journal_log");
    let const_st = st("JIT_TRACE_LOG_ENABLED");
    assert!(
        const_st < fn_st,
        "global JIT_TRACE_LOG_ENABLED ({const_st}) should outrank fn journal_log ({fn_st}) even for a lowercase query -- locality above case"
    );
}

#[test]
fn uppercase_query_ranks_screaming_name_above_lowercase_same_kind() {
    // Case is still a live boost -- among SAME-kind candidates (two globals, so
    // `kind` ties), typing `JLOG` (uppercase) case-matches the all-caps name, so
    // `JIT_TRACE_LOG_ENABLED` outranks the lowercase `journal_log_flag`. This
    // isolates the case rung: it decides only once locality has tied.
    let src = "global JIT_TRACE_LOG_ENABLED: bool = false\n\
               global journal_log_flag: bool = false\n\
               fn r() {\n  JLOG\n}\n";
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    let items = bone.completion(PATH, Position::new(3, 6));
    let st = |label: &str| -> String {
        items
            .iter()
            .find(|c| c.label == label)
            .unwrap_or_else(|| {
                panic!(
                    "no completion `{label}`: {:?}",
                    items.iter().map(|c| c.label.as_str()).collect::<Vec<_>>()
                )
            })
            .sort_text
            .clone()
            .expect("sort_text")
    };
    let const_st = st("JIT_TRACE_LOG_ENABLED");
    let fn_st = st("journal_log_flag");
    assert!(
        const_st < fn_st,
        "JIT_TRACE_LOG_ENABLED ({const_st}) should rank before journal_log_flag ({fn_st}) for an uppercase query -- case boost among same-kind"
    );
}

// ---------------------------------------------------------------------------
// The context-triggered paths must filter with `clean_match` (fuzzy, boundary-
// aware) like every other completion filter -- a hand-rolled starts_with here
// is the divergence class the rock-lsp StartsWithCi invariant existed to
// prevent: `atco` finds `attack_co` globally, so it must after `recv.` too.
// ---------------------------------------------------------------------------

#[test]
fn member_field_filter_is_fuzzy() {
    // `atco`: a@0 + t run + c/o at the `_` boundary -> `attack_co`.
    let src = "struct Mob { attack_co: int, health: int }\nfn r(m: Mob) {\n  m.atco\n}\n";
    let l = labels(src, 2, "  m.atco".len() as u32);
    assert!(l.contains(&"attack_co: int".to_string()), "fuzzy member field: {l:?}");
}

#[test]
fn member_method_filter_is_fuzzy() {
    // `tiv`: t@0 + i at the `_` boundary + v run -> `to_ivec`.
    let src = "struct Movable { g: int }\nfn to_ivec(m: Movable) -> int { m.g }\nfn r(mv: Movable) {\n  mv.tiv\n}\n";
    let l = labels(src, 3, "  mv.tiv".len() as u32);
    assert!(l.iter().any(|s| s == "to_ivec"), "fuzzy UFCS method: {l:?}");
}

#[test]
fn enum_qualified_variant_filter_is_fuzzy() {
    // `nogr`: n@0 + o run + g at the camel boundary + r run -> `NotGround`.
    let src = "enum TargetError { NotGround, OutOfRange }\nfn r() {\n  let e = TargetError::nogr\n}\n";
    let l = labels(src, 2, "  let e = TargetError::nogr".len() as u32);
    assert!(l.iter().any(|s| s == "NotGround"), "fuzzy variant: {l:?}");
    assert!(!l.iter().any(|s| s == "OutOfRange"), "non-matching variant filtered: {l:?}");
}

#[test]
fn struct_literal_field_filter_is_fuzzy() {
    // `mosp`: m@0 + o run + s at the `_` boundary + p run -> `move_speed`.
    let src = "struct Cfg { move_speed: int, other: int }\nfn r() {\n  let c = Cfg { mosp }\n}\n";
    let l = labels(src, 2, "  let c = Cfg { mosp".len() as u32);
    assert!(l.contains(&"move_speed: int".to_string()), "fuzzy struct-literal field: {l:?}");
}

#[test]
fn enum_record_field_filter_is_fuzzy() {
    // `alb`: a@0 + l run + b at the `_` boundary -> `alpha_beta`.
    let src = "enum E { V { alpha_beta: int, other: int } }\nfn r() {\n  let v = E::V { alb }\n}\n";
    let l = labels(src, 2, "  let v = E::V { alb".len() as u32);
    assert!(l.contains(&"alpha_beta: int".to_string()), "fuzzy enum-record field: {l:?}");
}

#[test]
fn qualifier_inside_a_comment_gets_global_completion() {
    // The byte scanner is comment-blind: `// SpellState::i|` classified as
    // enum-qualified and served ONLY the variants. The AST classifier sees no
    // qualified expr at the cursor, so scope completion runs. (Comment
    // suppression as such is a separate question; this pins the dispatch.)
    let src = "enum SpellState { Idle, Loaded(int) }\nfn idle_check() {}\nfn r() {\n  // SpellState::i\n}\n";
    let l = labels(src, 3, 18);
    assert!(
        l.contains(&"idle_check".to_string()),
        "global fn must be offered inside a comment: {l:?}"
    );
}

#[test]
fn struct_literal_head_inside_a_string_gets_no_field_completion() {
    // The byte scanner is string-blind: `"FixVec2 { |` served FixVec2's fields as
    // if the cursor were in a real literal. The AST classifier sees a string.
    let src = "struct FixVec2 { x: float, y: float }\nfn render() {}\nfn r() {\n  let s = \"FixVec2 { \n}\n";
    let l = labels(src, 3, 18);
    assert!(
        !l.contains(&"x: float".to_string()),
        "no struct-field completion inside a string literal: {l:?}"
    );
}

#[test]
fn decl_field_default_value_slot_type_directs() {
    // `kind: SpellKind = |` in a struct DECLARATION: the default is a value
    // expr typed against the field's declared type -- the enum's variants
    // must surface (parity with the byte scanner, which cannot tell a decl
    // from a literal and usefully fires here).
    let src = "enum SpellKind { Fire, Harm }\nstruct SpellDef {\n  kind: SpellKind = har\n}\n";
    let l = labels(src, 2, 21);
    assert!(
        l.iter().any(|l| l.contains("Harm")),
        "SpellKind::Harm must be offered in the decl default slot: {l:?}"
    );
}

#[test]
fn member_path_inside_a_comment_gets_global_completion() {
    // The byte scanner is comment-blind: `// cfg.vol|` classified as member
    // access and served Config's fields inside a comment. The AST classifier
    // sees no field access there, so scope completion runs.
    let src = "struct Config { volume: float }\nglobal cfg: Config = Config { volume: 1.0 }\nfn volume_reset() {}\nfn r() {\n  // cfg.vol\n}\n";
    let l = labels(src, 4, 11);
    assert!(
        l.contains(&"volume_reset".to_string()),
        "global fn must be offered inside a comment (not member fields): {l:?}"
    );
}

/// Response labels in RANK order (unsorted) -- for ordering assertions.
fn ordered_labels(src: &str, line: u32, ch: u32) -> Vec<String> {
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, src.to_string());
    let items = bone.completion(PATH, Position::new(line, ch));
    check_invariants(src, line, ch, &items);
    items.into_iter().map(|c| c.label).collect()
}

fn rank_of(labels: &[String], needle: &str) -> usize {
    labels
        .iter()
        .position(|l| l == needle)
        .unwrap_or_else(|| panic!("no completion `{needle}` in {labels:?}"))
}

// -- PatternPos: a match-arm pattern HEAD is a slot for the scrutinee's enum;
//    its variants rank first, and patterns bind (no payload template). -------

#[test]
fn match_arm_head_ranks_scrutinee_variants_first() {
    // Fresh arm, empty prefix: the scrutinee enum's variants outrank every
    // value-universe candidate (the scrutinee local included).
    let src = "enum AiState { Idle, Combat, Flee }\n\
               global count: int = 0\n\
               fn tick(s: AiState) {\n  match s {\n    \n  }\n}\n";
    let l = ordered_labels(src, 4, 4);
    let mut head: Vec<&str> = l.iter().take(3).map(|s| s.as_str()).collect();
    head.sort_unstable();
    assert_eq!(
        head,
        ["AiState::Combat", "AiState::Flee", "AiState::Idle"],
        "the scrutinee enum's variants must BE the top of the arm-head list: {:?}...",
        &l[..l.len().min(8)]
    );
}

#[test]
fn match_arm_head_variant_beats_prefix_matching_global() {
    // Typed prefix `co`: the scrutinee variant `Combat` must outrank the
    // case-exact-prefix global `count` -- type relevance dominates band,
    // exactly as in a typed call-arg slot.
    let src = "enum AiState { Idle, Combat, Flee }\n\
               global count: int = 0\n\
               fn tick(s: AiState) {\n  match s {\n    co\n  }\n}\n";
    let l = ordered_labels(src, 4, 6);
    assert!(
        rank_of(&l, "AiState::Combat") < rank_of(&l, "count"),
        "scrutinee variant must outrank a prefix-matching global in an arm head: {l:?}"
    );
}

#[test]
fn match_arm_head_variant_binds_not_constructs() {
    // Patterns bind, they don't construct: a tuple variant completed in an
    // arm head inserts the bare qualified name, never the `($0)` payload
    // template (parity with the `Enum::|` qualified path's pattern rule).
    let src = "enum Sig { Num(int), Flag }\n\
               fn f(s: Sig) {\n  match s {\n    \n  }\n}\n";
    let it = item(src, 3, 4, "Sig::Num");
    let insert = it.insert_text.as_deref().unwrap_or("Sig::Num");
    assert!(
        !insert.contains('('),
        "arm-head tuple variant must insert the bare name (bind, not construct): {insert:?}"
    );
}

#[test]
fn match_arm_head_promotes_with_unclosed_braces_at_eof() {
    // The mid-typing worst case: fresh match, nothing closed yet. The arm-
    // completing munge (`co__rock_completion__ => {}` + balancing) must still
    // resolve the scrutinee and promote its variants.
    let src = "enum AiState { Idle, Combat, Flee }\n\
               global count: int = 0\n\
               fn tick(s: AiState) {\n  match s {\n    co";
    let l = ordered_labels(src, 4, 6);
    assert!(
        rank_of(&l, "AiState::Combat") < rank_of(&l, "count"),
        "scrutinee variant must outrank the global with unclosed braces at EOF: {l:?}"
    );
}

// -- ForIn: the `for x in |` slot ranks iterable-typed candidates first. -----

#[test]
fn for_in_slot_ranks_iterable_global_first() {
    // `MOBS: [Mob]` is iterable; the int global and the fn share the prefix
    // but aren't -- type relevance dominates band (mob_spawn is a case-exact
    // prefix match, MOBS only ci).
    let src = "struct Mob { hp: int }\n\
               global MOBS: [Mob] = []\n\
               global MOB_COUNT: int = 0\n\
               fn mob_spawn() {}\n\
               fn r() {\n  for m in mo\n}\n";
    let l = ordered_labels(src, 5, 13);
    assert!(
        rank_of(&l, "MOBS") < rank_of(&l, "MOB_COUNT"),
        "iterable global must outrank the int global in a for-in slot: {l:?}"
    );
    assert!(
        rank_of(&l, "MOBS") < rank_of(&l, "mob_spawn"),
        "iterable global must outrank the case-exact-prefix fn in a for-in slot: {l:?}"
    );
}

#[test]
fn for_in_tuple_pattern_ranks_map_local_first() {
    // Tuple binding (`for (k, v) in`) classifies too; a Map local is the
    // iterable, the int local isn't. The int local sorts alphabetically
    // FIRST (`apples` < `scores`), so only the iterable rung can flip it.
    let src = "fn r() {\n  let scores: Map<string, int> = map_new()\n  let apples = 0\n  for (k, v) in \n}\n";
    let l = ordered_labels(src, 3, 16);
    assert!(
        rank_of(&l, "scores") < rank_of(&l, "apples"),
        "Map local must outrank the int local in a for-in slot: {l:?}"
    );
}

#[test]
fn for_in_slot_ranks_iterable_returning_fn_on_the_fn_rung() {
    // A fn RETURNING an iterable rides rung 1: above wrong-type values
    // (rung 2), below direct iterable values (rung 0).
    let src = "struct Mob { hp: int }\n\
               global MOBS: [Mob] = []\n\
               fn all_mobs() -> [Mob] { return MOBS }\n\
               fn r(alpha: int) {\n  for m in al\n}\n";
    let l = ordered_labels(src, 4, 13);
    assert!(
        rank_of(&l, "all_mobs") < rank_of(&l, "alpha"),
        "iterable-returning fn must outrank the wrong-type param in a for-in slot: {l:?}"
    );
}

#[test]
fn value_slot_control_no_iterable_promotion() {
    // Control: in a plain value slot the normal ladder holds -- the
    // case-exact-prefix fn beats the ci-prefix iterable global.
    let src = "struct Mob { hp: int }\n\
               global MOBS: [Mob] = []\n\
               fn mob_spawn() {}\n\
               fn r() {\n  let x = mo\n}\n";
    let l = ordered_labels(src, 4, 12);
    assert!(
        rank_of(&l, "mob_spawn") < rank_of(&l, "MOBS"),
        "no iterable promotion outside a for-in slot: {l:?}"
    );
}

#[test]
fn match_arm_body_is_a_value_slot_no_variant_promotion() {
    // Control: AFTER the `=>` the slot is a value expression -- the normal
    // ladder holds (prefix-matching global above the scattered variant).
    let src = "enum AiState { Idle, Combat, Flee }\n\
               global count: int = 0\n\
               fn tick(s: AiState) {\n  match s {\n    AiState::Idle => co\n  }\n}\n";
    let l = ordered_labels(src, 4, 23);
    assert!(
        rank_of(&l, "count") < rank_of(&l, "AiState::Combat"),
        "no variant promotion in an arm BODY (value slot): {l:?}"
    );
}

#[test]
fn chained_member_on_inferred_local_offers_vec_methods() {
    // `c.items.le|` where `c` is an INFERRED local (`let c = mk()`, no
    // annotation, call initializer). The single-segment path recovers the
    // head type from inference, but the multi-segment name-walk resolved the
    // head only structurally (declared_ty / struct-literal init / for-var /
    // match), so an inferred local's chained member came up empty. `.items`
    // is a `[Item]` (Vec), so `len` must be offered.
    let src = "struct Item { kind: int }\n\
               struct Corpse { items: [Item], name: string }\n\
               fn mk() -> Corpse { return Corpse { items: [], name: \"\" } }\n\
               fn r() {\n  let c = mk()\n  let n = c.items.le\n}\n";
    let l = labels(src, 5, 18);
    assert!(l.contains(&"len".to_string()), "Vec methods on chained inferred-local member: {l:?}");
}

#[test]
fn method_completion_omits_parens_when_they_already_follow() {
    // `c.items.le|()` -- accepting `len` must NOT re-append `()` (would yield
    // `len()()`). When a `(` already follows the cursor, insert the bare name.
    let src = "struct Item { kind: int }\n\
               struct Corpse { items: [Item], name: string }\n\
               fn r(c: Corpse) {\n  let n = c.items.le()\n}\n";
    let it = item(src, 3, 20, "len");
    assert_eq!(it.insert_text.as_deref(), Some("len"), "no doubled parens: {it:?}");

    // Control: no trailing paren -> the method DOES bring its own `()`.
    let src2 = "struct Item { kind: int }\n\
                struct Corpse { items: [Item], name: string }\n\
                fn r(c: Corpse) {\n  let n = c.items.le\n}\n";
    let it2 = item(src2, 3, 20, "len");
    assert_eq!(it2.insert_text.as_deref(), Some("len()"), "brings its own parens: {it2:?}");
}

#[test]
fn chained_member_through_generic_struct_field() {
    // `p.first.na|` where `first: A` is generic-substituted to `Corpse`. The
    // old structural name-walk resolved field hops by declared type and lost
    // the generic arg (`A` isn't a struct), so it came up empty. Resolving the
    // receiver `p.first` via inference substitutes A=Corpse. Works for a param
    // AND an inferred local.
    let src = "struct Corpse { items: int, name: string }\n\
               struct Pair<A, B> { first: A, second: B }\n\
               fn r(p: Pair<Corpse, int>) {\n  let n = p.first.na\n}\n";
    let l = labels(src, 3, 18);
    assert!(l.contains(&"name: string".to_string()), "generic field chain (param): {l:?}");

    let src2 = "struct Corpse { items: int, name: string }\n\
                struct Pair<A, B> { first: A, second: B }\n\
                fn mk() -> Pair<Corpse, int> { return Pair { first: Corpse { items: 0, name: \"\" }, second: 1 } }\n\
                fn r() {\n  let p = mk()\n  let n = p.first.na\n}\n";
    let l2 = labels(src2, 5, 18);
    assert!(l2.contains(&"name: string".to_string()), "generic field chain (inferred local): {l2:?}");
}

#[test]
fn chained_member_through_newtype_field_in_fallback() {
    // `foo(w.0.na|` with `foo` undefined -- inference can't type the arg of an
    // unknown call, so this hits the structural name-walk fallback. It must peel
    // the newtype `.0` to its wrapped `Corpse` and offer its fields. (Direct
    // `let n = w.0.na` already resolves via `type_at`; this pins the fallback.)
    let src = "struct Corpse { items: int, name: string }\n\
               newtype Wrap(Corpse)\n\
               fn r(w: Wrap) {\n  foo(w.0.na\n}\n";
    let l = labels(src, 3, 12);
    assert!(l.contains(&"name: string".to_string()), "newtype .0 in fallback: {l:?}");
}

#[test]
fn chained_member_through_tuple_projection() {
    // `t.0.na|` -- the receiver `t.0` is a tuple projection, which the old
    // structural name-walk couldn't encode as a name segment (it bailed to
    // empty). Inference types `t.0` as `Corpse`.
    let src = "struct Corpse { items: int, name: string }\n\
               fn r(t: (Corpse, int)) {\n  let n = t.0.na\n}\n";
    let l = labels(src, 2, 16);
    assert!(l.contains(&"name: string".to_string()), "tuple projection chain: {l:?}");
}

#[test]
fn chained_member_through_tuple_projection_in_fallback() {
    // `foo(t.0.na|` with `foo` undefined -- inference can't type the arg, so it
    // hits the structural name-walk fallback. A tuple isn't nominal, so the walk
    // has no head struct name; it must resolve `.0` on the declared tuple element
    // type (`Corpse`) and offer its fields.
    let src = "struct Corpse { items: int, name: string }\n\
               fn r(t: (Corpse, int)) {\n  foo(t.0.na\n}\n";
    let l = labels(src, 2, 12);
    assert!(l.contains(&"name: string".to_string()), "tuple projection in fallback: {l:?}");
}

#[test]
fn chained_member_through_inferred_local_tuple_head_in_fallback() {
    // `foo(t.0.na|` where `t` is an INFERRED local (`let t = mk()`, no
    // annotation) of tuple type, in an unknown call. The head has no declared
    // type, so the fallback must recover the tuple type from inference
    // (`binding_types`) before resolving `.0`.
    let src = "struct Corpse { items: int, name: string }\n\
               fn mk() -> (Corpse, int) { return (Corpse { items: 0, name: \"\" }, 1) }\n\
               fn r() {\n  let t = mk()\n  foo(t.0.na\n}\n";
    let l = labels(src, 4, 12);
    assert!(l.contains(&"name: string".to_string()), "inferred-local tuple head in fallback: {l:?}");
}

#[test]
fn chained_member_through_inferred_local_newtype_head_in_fallback() {
    // `foo(w.0.na|` where `w` is an INFERRED-local newtype in an unknown call.
    // The inferred head-name recovery must accept a newtype (not just a struct)
    // so `.0` can then peel it.
    let src = "struct Corpse { items: int, name: string }\n\
               newtype Wrap(Corpse)\n\
               fn mk() -> Wrap { return Wrap(Corpse { items: 0, name: \"\" }) }\n\
               fn r() {\n  let w = mk()\n  foo(w.0.na\n}\n";
    let l = labels(src, 5, 12);
    assert!(l.contains(&"name: string".to_string()), "inferred-local newtype head in fallback: {l:?}");
}

#[test]
fn call_receiver_suppresses_globals_when_no_member_matches() {
    // `getitems().ce|` -- the receiver resolves to `[Item]` (a Vec), and `ce`
    // matches no Vec member. General completion must be SUPPRESSED: leaking the
    // global `centroid([FixVec2])` / `ceil` would be an invalid UFCS suggestion
    // (`centroid([Item])` doesn't type-check). A call receiver used to fall
    // through to general on an empty member result; a name path never did.
    let src = "struct FixVec2 { x: float, y: float }\n\
               struct Item { kind: int }\n\
               fn centroid(v: [FixVec2]) -> FixVec2 { return v[0] }\n\
               fn getitems() -> [Item] { return [] }\n\
               fn r() {\n  let n = getitems().ce\n}\n";
    let leak = labels(src, 5, 23);
    assert!(!leak.contains(&"centroid".to_string()), "leaked [FixVec2] global as UFCS: {leak:?}");
    assert!(!leak.contains(&"ceil".to_string()), "leaked a global after member `.`: {leak:?}");

    // Non-vacuous: the same receiver DOES offer its Vec members for a real
    // prefix, so the suppression above isn't just an unresolved receiver.
    let hit = "struct Item { kind: int }\n\
               fn getitems() -> [Item] { return [] }\n\
               fn r() {\n  let n = getitems().cl\n}\n";
    let l = labels(hit, 3, 23);
    assert!(l.contains(&"clear".to_string()), "Vec members must resolve on a call receiver: {l:?}");
}

#[test]
fn unspaced_less_than_comparison_is_expression_position() {
    // `hp<Loa|` -- the byte scanner reads `ident<` as a generic type slot
    // and offers the bare enum instead of qualified variants. It's a
    // comparison: an expression slot; the qualified form must be offered.
    let src = "enum SpellState { Idle, Loaded(int) }\nfn r(hp: int) {\n  let ok = hp<Loa\n}\n";
    let l = labels(src, 2, 17);
    assert!(
        l.contains(&"SpellState::Loaded".to_string()),
        "expression position: the qualified variant must be offered after an unspaced `<` comparison: {l:?}"
    );
}

#[test]
fn member_completion_recovers_a_missing_comma_in_a_struct_literal() {
    // `z: st.<cursor>` above the next field with no trailing comma used to return
    // NOTHING -- the unterminated value ran into `vel:` and collapsed the literal
    // parse. Struct-field comma recovery (skull-parser) keeps the literal, so
    // `st.` still offers its members. (mag blood.rock:111:19.)
    let src = "struct St { hp: float, mp: float }\n\
               struct Drop { kind: int, z: float, vel: float }\n\
               fn f(st: St) {\n\
               \x20   let d = Drop {\n\
               \x20       kind: 0,\n\
               \x20       z: st.\n\
               \x20       vel: 1.0,\n\
               \x20   }\n\
               }\n";
    // line 5 (0-indexed) = `        z: st.`, cursor right after the `.` (char 14).
    let l = labels(src, 5, 14);
    assert!(l.iter().any(|x| x.starts_with("hp")), "St.hp must be offered: {l:?}");
    assert!(l.iter().any(|x| x.starts_with("mp")), "St.mp must be offered: {l:?}");
}

#[test]
fn reach_insert_does_not_duplicate_an_existing_member_tail() {
    // `push(0, PANE|.rect(),` -- a reach candidate `PANEL.rect()` inserted over
    // the existing `.rect()` tail must not double it into `PANEL.rect().rect()`.
    // The insert drops the shared trailing `.rect()`, leaving `PANEL` before the
    // buffer's `.rect()`. (mag shop.rock capture 1784164491.)
    let src = "struct UiRect {}\n\
               struct Panel {}\n\
               fn rect(self: Panel) -> UiRect { UiRect {} }\n\
               global PANEL: Panel = Panel {}\n\
               fn push(a: int, r: UiRect) {}\n\
               fn caller() {\n\
               \x20   push(0, PANE.rect(),\n\
               \x20       1)\n\
               }\n";
    // line 6 (0-indexed) = `    push(0, PANE.rect(),`, cursor after `PANE` (char 16).
    let it = item(src, 6, 16, "PANEL.rect() -> UiRect");
    assert_eq!(
        it.insert_text.as_deref(),
        Some("PANEL"),
        "reach insert must strip the shared `.rect()` tail, not re-emit it: {:?}",
        it.insert_text
    );
}
