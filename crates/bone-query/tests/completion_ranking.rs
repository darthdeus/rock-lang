//! Completion ranking corpus: each case pins a scoring outcome.

use crate::completion::*;
use crate::fixture::Project;
use bone_query::BoneWorkspace;
use lsp_types::Position;

const RECEIVER_OVERLOAD: &str = "value Color { r: float, g: float, b: float, a: float }\nstruct UiRect { x: float }\n\
    struct Image { w: int }\nglobal WHITE: Color = Color { r: 1.0, g: 1.0, b: 1.0, a: 1.0 }\n\
    global WHITE_PIXEL: Image = Image { w: 1 }\nfn image_dims(i: Image) -> float { 1.0 }\n\
    fn text(r: UiRect, s: string, size: float, color: Color) -> UiRect { r }\n\
    fn text(d: Image, s: string, size: float, alpha: float) -> Image { d }\n\
    fn draw(ui: UiRect) {\n  ui.text(\"x\", 12.0, WHI<|>)\n}\n";

#[test]
fn match_quality_orders_candidates() {
    check_ranked(&[
        // Match quality leads; case only breaks near-ties.
        (
            "whole prefix of a PascalCase name above a boundary match",
            "struct SpellBindingX { key: string }\nfn spell_bank() {}\nfn spell_clear() {}\n\
             fn r() {\n  spellb<|>\n}\n",
            "SpellBindingX < spell_bank",
        ),
        (
            "whole prefix of a const above a boundary match",
            "const FONT_SMALL: float = 12.0\nfn format_new() {}\nfn r() {\n  fon<|>\n}\n",
            "FONT_SMALL < ?format_new",
        ),
        (
            "contiguous run above scattered boundary hits",
            "global LOOT_PANEL: int = 0\nglobal LATER_OUT_OF_TOWN_PERSON_ASTEROID: int = 0\n\
             fn r() {\n  let x = LOOTPA<|>\n}\n",
            "LOOT_PANEL < LATER_OUT_OF_TOWN_PERSON_ASTEROID",
        ),
        // A run must never be worth less than a one-char hop to a boundary.
        (
            "contiguous run above an interior boundary hop",
            "fn pixel_readback_new() -> int { 0 }\nfn pixel_readback_depth_new() -> int { 0 }\n\
             fn r() {\n  let x = pixelreadnew<|>\n}\n",
            "pixel_readback_new < pixel_readback_depth_new",
        ),
        // Same band: the local's kind beats the fn's extra boundary points.
        (
            "local above a boundary-richer fn",
            "struct IVec2 { x: int, y: int }\n\
             fn target_ref_grid(t: int) -> IVec2? { return IVec2 { x: 0, y: 0 } }\n\
             fn use_it() {\n  let target_grid = IVec2 { x: 1, y: 2 }\n  let tdist = targrid<|>\n}\n",
            "target_grid < target_ref_grid",
        ),
        (
            "member: prefix field above a boundary-richer field",
            "struct S { cost: int, charge_over_time: int }\nfn r(s: S) {\n  let x = s.co<|>\n}\n",
            "cost* < charge_over_time*",
        ),
        (
            "most recent binding wins a tie",
            "fn r(dt: float) {\n  let dmg = 1.0\n  d<|>\n}\n",
            "dmg < dt",
        ),
        // Alphabetically last, so only the proximity rung lifts it past the
        // cap and the stdlib.
        (
            "same-file fn above stdlib at an empty prefix",
            "fn zzz_mine() {}\nfn r() {\n  <|>\n}\n",
            "zzz_mine < abs",
        ),
    ]);
}

#[test]
fn expected_type_orders_candidates() {
    check_ranked(&[
        // A match-arm body is a return slot. The enclosing fn is not offered
        // as a confident autofill inside its own body.
        (
            "constructor above a recursive self-call in an arm body",
            "enum PxKind { Blood, Dust }\nvalue PartitionStyle { gravity: float, damp: float }\n\
             fn partition_style(k: PxKind) -> PartitionStyle {\n  match k {\n    \
             PxKind::Blood => PartSty<|>\n    PxKind::Dust => PartSty\n  }\n}\n",
            "PartitionStyle < ?partition_style(k)",
        ),
        (
            "arm body sees the return type",
            "value Sty { a: float }\nglobal sty_val: Sty = Sty { a: 0.0 }\n\
             fn sty_maker(x: int) -> int { 0 }\nfn use_it(x: int) -> Sty {\n  match x {\n    \
             _ => sty<|>\n  }\n}\n",
            "sty_val < ?sty_maker(x)",
        ),
        // UFCS makes the receiver param 0, and the receiver picks the overload.
        (
            "UFCS arg slot uses the receiver overload's param type",
            RECEIVER_OVERLOAD,
            "WHITE < ?WHITE_PIXEL",
        ),
        (
            "lambda body expects the field's fn return type",
            "struct PanelReg { is_open: fn() -> bool }\nglobal SPELL_IS_OPEN: bool = false\n\
             global SPELL_ID: int = 0\nglobal PANELS: [PanelReg] = [\n  \
             PanelReg { is_open: () => sp<|> }\n]\n",
            "SPELL_IS_OPEN < SPELL_ID",
        ),
        // The struct's own field type, not the enclosing call's param type.
        (
            "field value of a literal that is a call arg",
            "enum BlendModeX { Alpha, Replace }\nstruct PassX { blend: BlendModeX }\n\
             fn run(p: PassX) {}\nfn replace(s: string, old: string, new: string) -> string { s }\n\
             fn probe() {\n  run(PassX { blend: repl<|> })\n}\n",
            "BlendModeX::Replace < replace",
        ),
        // The shorter plain fn wins without `await`; `await` flips the pair.
        (
            "shorter plain fn first without await",
            "co fn frame_co() -> Coroutine<int> {\n  yield 1\n}\nfn frag() -> int { 0 }\n\
             co fn t() -> Coroutine<int> {\n  let x = fr<|>\n  yield 1\n}\n",
            "frag < frame_co",
        ),
        (
            "coroutine fn first after await",
            "co fn frame_co() -> Coroutine<int> {\n  yield 1\n}\nfn frag() -> int { 0 }\n\
             co fn t() -> Coroutine<int> {\n  await fr<|>\n}\n",
            "frame_co < frag",
        ),
        // Ranking runs bone's `coerce`, so call-compatible types with different
        // renders still take the expected-type bucket.
        (
            "concrete param type",
            "fn feed(x: int) {}\nfn r() {\n  let aa_decoy = \"hello\"\n  let n = 5\n  feed(<|>)\n}\n",
            "n < aa_decoy",
        ),
        (
            "generic param instantiated by a vec",
            "fn apply_all<T>(xs: [T]) {}\n\
             fn r() {\n  let aa_decoy = \"hello\"\n  let nums = [1, 2, 3]\n  apply_all(<|>)\n}\n",
            "nums < aa_decoy",
        ),
        (
            "nullable param accepts a bare value",
            "fn feed(x: int?) {}\nfn r() {\n  let aa_decoy = \"hello\"\n  let n = 5\n  feed(<|>)\n}\n",
            "n < aa_decoy",
        ),
        (
            "generic UFCS receiver binding is kept",
            "fn add_all<T>(xs: [T], ys: [T]) {}\nfn r() {\n  let aa_decoy = \"x\"\n  \
             let nums = [1, 2]\n  let more = [3]\n  nums.add_all(<|>)\n}\n",
            "more < aa_decoy",
        ),
        (
            "fresh arg between named args",
            "fn draw(p: int, size: float, name: string, color: int) {}\n\
             fn t(c: int) {\n  let sv: string = \"x\"\n  let iv = 7\n  draw(1, size: 2.0,<|> color: c)\n}\n",
            "sv < iv",
        ),
        (
            "chain receiver with a paren in a string",
            "struct A { t: int }\nstruct B { t: int }\nfn m(a: A, x: int) {}\n\
             fn m(b: B, s: string) {}\nfn mk(tag: string) -> A { return A { t: 1 } }\n\
             fn t() {\n  let aa_s: string = \"y\"\n  let zz_i = 7\n  mk(\"(x\").m(<|>)\n}\n",
            "zz_i < aa_s",
        ),
        (
            "local-seeded call-through above an unrelated global",
            "struct Mob { hp: int }\nglobal attention_span: int = 0\nfn attack(m: Mob) {}\n\
             fn r(mob: Mob) {\n  att<|>\n}\n",
            "attack(mob) < attention_span",
        ),
        (
            "exact name above the expected type",
            "fn feed(x: int) {}\nfn r() {\n  let xsize = 5\n  let xs = \"hi\"\n  feed(xs<|>)\n}\n",
            "xs < xsize",
        ),
        (
            "assignment RHS: expected variant above a prefix fn",
            "enum MobAiState { Wander, Combat }\nstruct Mob { ai_state: MobAiState }\n\
             fn combat_target_grid(mob: Mob) -> int { return 0 }\n\
             fn use_it(mob: Mob) {\n  mob.ai_state = combat<|>\n}\n",
            "MobAiState::Combat < ?combat_target_grid",
        ),
        (
            "return value: expected variant above a prefix fn",
            "enum MobAiState { Wander, Combat }\nfn combat_target_grid(x: int) -> int { return 0 }\n\
             fn pick() -> MobAiState {\n  return combat<|>\n}\n",
            "MobAiState::Combat < ?combat_target_grid",
        ),
        (
            "typed let: expected variant above a prefix fn",
            "enum MobAiState { Wander, Combat }\nfn combat_target_grid(x: int) -> int { return 0 }\n\
             fn r() {\n  let s: MobAiState = combat<|>\n}\n",
            "MobAiState::Combat < ?combat_target_grid",
        ),
        (
            "comparison RHS: expected variant above a prefix fn",
            "enum MobAiState { Wander, Combat }\nstruct Mob { ai_state: MobAiState }\n\
             fn combat_target_grid(mob: Mob) -> int { return 0 }\n\
             fn use_it(mob: Mob) {\n  if mob.ai_state == combat<|> {\n  }\n}\n",
            "MobAiState::Combat < ?combat_target_grid",
        ),
        (
            "coalesce RHS: expected variant above a prefix fn",
            "enum MobAiState { Wander, Combat }\nfn combat_target_grid(x: int) -> int { return 0 }\n\
             fn r(state: MobAiState?) {\n  let s = state ?? combat<|>\n}\n",
            "MobAiState::Combat < ?combat_target_grid",
        ),
        // Band before kind: a case-exact-prefix global beats a scattered local;
        // within one band the local wins.
        (
            "typed slot: better band beats locality",
            "global nomad_x: int = 3\nfn feed(x: int) {}\n\
             fn r() {\n  let nx_offset_mad = 5\n  feed(nomad<|>)\n}\n",
            "nomad_x < nx_offset_mad",
        ),
        (
            "typed slot: locality breaks a same-band tie",
            "global target_ref_grid_max: int = 3\nfn feed(x: int) {}\n\
             fn r() {\n  let target_grid = 5\n  feed(targrid<|>)\n}\n",
            "target_grid < target_ref_grid_max",
        ),
        (
            "direct global above a reach chain rooted at another global",
            "struct Holder { grid_size: int }\nglobal grip_count: int = 3\n\
             global grid_holder: Holder = Holder { grid_size: 1 }\nfn feed(x: int) {}\n\
             fn r() {\n  feed(gr<|>)\n}\n",
            "grip_count < grid_holder.grid_size*",
        ),
        (
            "same-band local-rooted path above a right-type global",
            "struct Holder { grid_size: int }\nglobal h_count: int = 3\nfn feed(x: int) {}\n\
             fn r(h: Holder) {\n  feed(h<|>)\n}\n",
            "h.grid_size* < h_count",
        ),
        // A method-final reach path is a computed value, not a stored one.
        (
            "direct fn above its own UFCS reach synths",
            "struct Trail { x: int }\nstruct Color { r: int }\nglobal RED: Color = Color { r: 1 }\n\
             global BLUE: Color = Color { r: 2 }\nglobal GREEN: Color = Color { r: 3 }\n\
             global DRAG: Trail? = null\n\
             fn wisp_trail_new(tint: Color) -> Trail { return Trail { x: 0 } }\n\
             fn r() {\n  if DRAG == null {\n    DRAG = wisptrailn<|>\n  }\n}\n",
            "wisp_trail_new < RED.wisp_trail_new()*",
        ),
        (
            "member: assignment LHS types the slot",
            "enum MobAiState { Wander, Combat }\nstruct Mob { ai_state: MobAiState }\n\
             struct Other { zz_state: MobAiState, aa_count: int }\n\
             fn r(mob: Mob, other: Other) {\n  mob.ai_state = other.<|>\n}\n",
            "zz_state* < aa_count*",
        ),
        (
            "member: return slot types the slot",
            "enum MobAiState { Wander, Combat }\n\
             struct Other { zz_state: MobAiState, aa_count: int }\n\
             fn pick(other: Other) -> MobAiState {\n  return other.<|>\n}\n",
            "zz_state* < aa_count*",
        ),
        (
            "member arg slot: field above a boundary-richer method",
            "struct S { target_grid: int }\nfn target_ref_grid(s: S) -> int { return 0 }\n\
             fn feed(x: int) {}\nfn r(s: S) {\n  feed(s.targrid<|>)\n}\n",
            "target_grid* < target_ref_grid*",
        ),
        (
            "member field coercing into a nullable slot",
            "struct S { zog: int, aaa: string }\nfn wants(x: int?) {}\n\
             fn r() {\n  let s = S { zog: 1, aaa: \"y\" }\n  wants(s.<|>)\n}\n",
            "zog: int < aaa: string",
        ),
        (
            "wrong-type call-through synth below the expected variant",
            "enum MobAiState { Wander, Combat }\nstruct Mob { ai_state: MobAiState }\n\
             fn compare(mob: Mob) -> int { return 0 }\n\
             fn use_it(mob: Mob) {\n  mob.ai_state = com<|>\n}\n",
            "MobAiState::Combat < ?compare(mob)",
        ),
        (
            "right-type call-through synth keeps its promotion",
            "enum MobAiState { Wander, Combat }\nstruct Mob { ai_state: MobAiState }\n\
             fn make_state(mob: Mob) -> MobAiState { return MobAiState::Wander }\n\
             fn makeshift_grid(x: int) -> int { return 0 }\n\
             fn use_it(mob: Mob) {\n  mob.ai_state = makes<|>\n}\n",
            "make_state(mob) < ?makeshift_grid",
        ),
        // A bare-ident assign target is typed `()` by inference (place position).
        (
            "assignment to a local types the RHS",
            "struct Widget { id: int }\n\
             fn f(dest: Widget, maybe_w: Widget?, sure_w: Widget, num: int) {\ndest = <|>\n}\n",
            "sure_w < maybe_w; sure_w < num",
        ),
    ]);
}

#[test]
fn ranking_contexts_offer() {
    check_labels(&[
        // No float-typed reach expansion (`WHITE_PIXEL.image_dims()`).
        (
            "UFCS arg slot of the receiver overload",
            RECEIVER_OVERLOAD,
            &["WHITE", "!*image_dims*", "!*->*", "!*.*"],
        ),
        // The trailing arg is broken, so the chain's last method return type
        // picks the overload.
        (
            "UFCS arg slot of a chain receiver",
            "value Color { r: float }\nstruct UiRect { x: float }\nstruct Image { w: int }\n\
             global WHITE: Color = Color { r: 1.0 }\nglobal WHITE_PIXEL: Image = Image { w: 1 }\n\
             fn image_dims(i: Image) -> float { 1.0 }\nfn fill(r: UiRect) -> UiRect { r }\n\
             fn text(r: UiRect, s: string, color: Color) -> UiRect { r }\n\
             fn text(d: Image, s: string, alpha: float) -> Image { d }\n\
             fn draw(ui: UiRect) {\n  ui.fill().text(\"x\", WHI<|>)\n}\n",
            &["WHITE", "!*image_dims*", "!*.*", "!*->*"],
        ),
        // A top-level `:` before the cursor is not a field-name slot.
        (
            "lambda body in a struct literal in an array",
            "struct PanelReg { is_open: fn() -> bool }\nglobal SPELL_OPEN: bool = false\n\
             global PANELS: [PanelReg] = [\n  PanelReg { is_open: () => SPE<|> }\n]\n",
            &["SPELL_OPEN"],
        ),
        (
            "struct field value is a value slot",
            "enum SpellKind { Fire, Harm }\nstruct SpellBindingX { key: string, kind: SpellKind }\n\
             global BINDS: [SpellBindingX] = [\n  SpellBindingX { key: \"k\", kind: Spell<|> }\n]\n",
            &["*SpellKind*"],
        ),
        (
            "struct field value offers the field's enum variant",
            "enum SpellKind { Fire, Harm }\nstruct SpellBindingX { key: string, kind: SpellKind }\n\
             global BINDS: [SpellBindingX] = [\n  SpellBindingX { key: \"k\", kind: har<|> }\n]\n",
            &["*Harm*"],
        ),
        // An enclosing call's unsupplied param must not leak in as `name:`.
        (
            "no named arg from the enclosing call",
            "struct Widget { x: int }\nfn run(w: Widget, replace_me: int) {}\n\
             fn probe() {\n  run(Widget { x: repl<|> })\n}\n",
            &["!replace_me*"],
        ),
        // A unique in-scope producer of the param type is an unambiguous arg,
        // offered even when the fn name covers the query.
        (
            "unique local autofills a call",
            "struct SpellX { kind: int }\nfn can_cast(s: SpellX) -> bool { true }\n\
             fn probe(s: SpellX) {\n  let a = can<|>\n}\n",
            &["can_cast", "can_cast(s)"],
        ),
        (
            "unique field path autofills a call",
            "struct SpellX { kind: int }\nfn spell_hotkey_label(kind: int) -> string { \"\" }\n\
             fn probe(s: SpellX) {\n  let b = spehol<|>\n}\n",
            &["spell_hotkey_label(s.kind)"],
        ),
        (
            "ambiguous arg does not autofill",
            "struct SpellX { kind: int }\nfn can_cast(s: SpellX) -> bool { true }\n\
             fn probe(s1: SpellX, s2: SpellX) {\n  let a = can<|>\n}\n",
            &["can_cast", "!can_cast(*"],
        ),
        // Rendering an un-annotated receiver's type needs the body inference.
        (
            "UFCS arg slot on a local receiver",
            "fn add_all<T>(xs: [T], ys: [T]) {}\nfn r() {\n  let nums = [1, 2]\n  nums.add_all(<|>)\n}\n",
            &["nums"],
        ),
        // The noise guard drops a fn absorbing under half the query, except
        // when the residual matches a local of the param type.
        (
            "local-seeded call-through on a short fn prefix",
            "struct Mob { hp: int }\nfn give(m: Mob) {}\nfn r(mob: Mob) {\n  gmob<|>\n}\n",
            &["give(mob)"],
        ),
        (
            "soft-private symbols hidden on a plain prefix",
            "fn _helper() {}\nglobal _PRIV: int = 1\nfn visible_helper() {}\n\
             fn r() {\n  let _loc = 2\n  let visible_loc = 3\n  he<|>\n}\n",
            &["visible_helper", "!_helper", "!_PRIV", "!_loc"],
        ),
        (
            "typing `_` surfaces soft-private symbols",
            "fn _helper() {}\nglobal _PRIV: int = 1\nfn r() {\n  _<|>\n}\n",
            &["_helper", "_PRIV"],
        ),
    ]);
    // The explicit prefix is the escape hatch from the test-fn demotion.
    check_head(&[(
        "typed test_ prefix",
        "fn test_apply() {}\nfn zap_thing() {}\nfn r() {\n  test_ap<|>\n}\n",
        &["test_apply"],
    )]);
}

#[test]
fn reach_path_flood_is_kept_and_ranked() {
    // Fifteen right-type dotted paths stay browsable; the response is capped,
    // never thinned.
    let l = labels(
        "fn feed(x: int) {}\nstruct S { a: int, b: int, c: int, d: int, e: int, f: int, g: int, \
         h: int, i: int, j: int, k: int, l: int, m: int, n: int, o: int }\n\
         fn r(s: S) {\n  feed(<|>)\n}\n",
    );
    let paths = l.iter().filter(|l| l.contains('.')).count();
    assert!(paths >= 15, "{paths} paths: {l:?}");
}

// RankKey digits: 1 type rank, 3 kind, 12 proximity, 13 frequency, 14 recency.
const TYPE: usize = 1;
const KIND: usize = 3;
const PROX: usize = 12;
const FREQ: usize = 13;
const REC: usize = 14;

#[test]
fn rank_key_digits() {
    let test_fn = "fn test_apply() {}\nfn r() {\n  test_ap<|>\n}\n";
    let locals = "fn r(dt: float) {\n  let alpha = 1\n  let dmg = 1.0\n  <|>\n}\n";
    let cases: &[(&str, &str, &[(&str, usize, char)])] = &[
        (
            PATH,
            "struct Mob { hp: int }\nfn give(m: Mob) {}\nfn r(mob: Mob) {\n  givemob<|>\n}\n",
            &[("give(mob)", TYPE, '0')],
        ),
        (PATH, test_fn, &[("test_apply", KIND, '4')]),
        (
            "/tmp/tests/rock_lsp_ranking_t.rock",
            test_fn,
            &[("test_apply", KIND, '2')],
        ),
        (
            PATH,
            "fn zzz_mine() {}\nfn r() {\n  <|>\n}\n",
            &[("zzz_mine", PROX, '0'), ("abs", PROX, '3')],
        ),
        (
            PATH,
            locals,
            &[("dmg", REC, '0'), ("alpha", REC, '1'), ("dt", REC, '2')],
        ),
    ];
    for (path, src, expected) in cases {
        let items = complete_in(path, src);
        for &(label, digit, want) in *expected {
            assert_eq!(
                sort_digit(&items, label, digit),
                want,
                "{path} {src:?}: {label}"
            );
        }
    }
}

#[test]
fn reference_count_orders_and_buckets_fns() {
    // Alphabetical order opposes, so only the frequency rung lifts `zz_hot`.
    let calls = |name: &str, n: usize| format!("  {name}()\n").repeat(n);
    let hot = format!(
        "fn zz_hot() {{}}\nfn aa_cold() {{}}\nfn r() {{\n{}  <|>\n}}\n",
        calls("zz_hot", 16)
    );
    assert_ranked("hot fn", &complete(&hot), "zz_hot < aa_cold");
    let buckets = format!(
        "fn zz_hot() {{}}\nfn mm_warm() {{}}\nfn aa_cold() {{}}\nfn r() {{\n{}{}  <|>\n}}\n",
        calls("zz_hot", 16),
        calls("mm_warm", 3)
    );
    let items = complete(&buckets);
    assert_eq!(sort_digit(&items, "zz_hot", FREQ), '0', "16 refs");
    assert_eq!(sort_digit(&items, "mm_warm", FREQ), '1', "3 refs");
    assert_eq!(sort_digit(&items, "aa_cold", FREQ), '2', "0 refs");
}

#[test]
fn tests_dir_symbols_demote_only_outside_test_files() {
    let main_src = "fn zzz_real() {}\nfn game() {\n  aaa_h\n}\nfn game2() {\n  zzz_r\n}\n";
    let helper_src = "fn aaa_helper() {}\nfn sim_run() {\n  aaa_h\n}\n";
    let files = [("main.rock", main_src), ("tests/helpers.rock", helper_src)];
    let project = Project::package(&files);
    let cases = [
        ("tests/ fn from main", 0, (2, 7), "aaa_helper", KIND, '4'),
        ("main fn from main", 0, (5, 7), "zzz_real", KIND, '2'),
        (
            "tests/ fn is project-near",
            0,
            (2, 7),
            "aaa_helper",
            PROX,
            '1',
        ),
        (
            "tests/ fn from its own file",
            1,
            (2, 7),
            "aaa_helper",
            KIND,
            '2',
        ),
    ];
    for (case, file, (line, col), label, idx, want) in cases {
        let (rel, src) = files[file];
        let path = project.path(rel);
        let mut bone = BoneWorkspace::new();
        bone.update(&path, src.to_string());
        let items = bone.completion(&path, Position::new(line, col));
        assert_eq!(sort_digit(&items, label, idx), want, "{case}");
    }
}

#[test]
fn project_dir_named_stdlib_keeps_project_proximity() {
    // Stdlib ownership comes from the loader's source id, not a path component.
    let main_src = "fn game() {\n  aaa_h\n  ab\n}\n";
    let project = Project::package(&[
        ("main.rock", main_src),
        ("stdlib/helpers.rock", "fn aaa_helper() {}\n"),
    ]);
    let main = project.path("main.rock");
    let mut bone = BoneWorkspace::new();
    bone.update(&main, main_src.to_string());
    let helper = bone.completion(&main, Position::new(1, 7));
    assert_eq!(sort_digit(&helper, "aaa_helper", PROX), '1');
    let stdlib = bone.completion(&main, Position::new(2, 4));
    assert_eq!(sort_digit(&stdlib, "abs", PROX), '3');
}

#[test]
fn completion_after_an_interface_edit_reads_the_new_signature() {
    let source = |params: &str| {
        format!("fn helper_fn({params}) -> int {{ 1 }}\nfn main() {{\n    helper\n}}\n")
    };
    let pos = Position::new(2, 10);
    let detail = |bone: &mut BoneWorkspace| {
        bone.completion(PATH, pos)
            .into_iter()
            .find(|item| item.label.starts_with("helper_fn"))
            .and_then(|item| item.detail)
            .expect("helper_fn is offered with its signature")
    };
    let mut bone = BoneWorkspace::new();
    bone.update(PATH, source("a: int"));
    assert!(detail(&mut bone).contains("a: int"));
    bone.update(PATH, source("renamed: string"));
    let after = detail(&mut bone);
    assert!(
        after.contains("renamed: string"),
        "stale completion detail: {after}"
    );
}
