//! Completion contexts: what each cursor context offers, how accepting an
//! item edits the buffer, and how a context orders its candidates.

use crate::completion::Insert::{Label, Plain, Snippet, Text};
use crate::completion::*;
use bone_query::BoneWorkspace;
use lsp_types::CompletionItemKind;

#[test]
fn member_access_offers() {
    check_labels(&[
        (
            "fields and UFCS methods",
            "struct FixVec2 { x: float, y: float }\nfn dist(a: FixVec2) -> float { a.x }\n\
             fn r() {\n  let v = FixVec2 { x: 1.0, y: 2.0 }\n  v.<|>\n}\n",
            &["x: float", "dist"],
        ),
        // The invariants reject the duplicate label two overloads would add.
        (
            "UFCS overloads dedup to one item",
            "struct FixVec2 { x: float, y: float }\n\
             fn rotate(v: FixVec2, angle: float) -> FixVec2 { v }\n\
             fn rotate(v: FixVec2, rhs: FixVec2) -> FixVec2 { v }\n\
             fn r(p: FixVec2) {\n  p.<|>\n}\n",
            &["rotate"],
        ),
        // `[T]` resolves as the stdlib `Vec` struct. `_data` is soft-private, and
        // UFCS is element-aware: a `[int]` fn matches, a `[Foo]` fn does not.
        (
            "vec receiver",
            "struct Foo { a: int }\nfn takes_foos(fs: [Foo]) {}\nfn takes_ints(v: [int]) {}\n\
             fn r(xs: [int]) {\n  xs.<|>\n}\n",
            &["len: int", "push", "takes_ints", "!_data*", "!takes_foos"],
        ),
        (
            "plain call receiver",
            "struct SpellDef { cast_time: float, mana: int }\n\
             fn find_spell() -> SpellDef { SpellDef { cast_time: 1.0, mana: 2 } }\n\
             fn r() {\n  find_spell().<|>\n}\n",
            &["cast_time: float", "mana: int"],
        ),
        (
            "UFCS call receiver",
            "struct SpellDef { cast_time: float, mana: int }\n\
             fn find_spell(kind: int) -> SpellDef { SpellDef { cast_time: 1.0, mana: 2 } }\n\
             fn r(spell: int) {\n  spell.find_spell().<|>\n}\n",
            &["cast_time: float", "mana: int"],
        ),
        // `Iterator`-bounded combinators are UFCS-callable on any iterator: a
        // generic first param, not a nominal one.
        (
            "iterator combinators on a stdlib iterator",
            "fn r(xs: [int]) {\n  let it = xs.iter()\n  it.<|>\n  let _n = 0\n}\n",
            &["enumerate", "map", "filter", "collect", "next"],
        ),
        (
            "iterator combinators on a user iterator",
            "struct Counter { n: int }\n\
             fn Counter.next(c) -> (bool, int) { return (c.n <= 0, c.n) }\n\
             fn r() {\n  let c = Counter { n: 3 }\n  c.<|>\n  let _x = 0\n}\n",
            &["next", "enumerate", "map"],
        ),
        // An iterable reaches a combinator through the implicit `iter(recv)`.
        (
            "iterator combinators on a vec",
            "fn r(names: [int]) {\n  names.<|>\n  let _n = 0\n}\n",
            &["map", "filter", "enumerate"],
        ),
        (
            "iterator combinators on an arena",
            "struct Mob { hp: int }\nfn r(mobs: Arena<Mob>) {\n  mobs.<|>\n  let _n = 0\n}\n",
            &["map", "filter", "enumerate"],
        ),
        (
            "no combinators when next is not (bool, E)",
            "struct Fake { n: int }\nfn Fake.next(f) -> int { return f.n }\n\
             fn r() {\n  let f = Fake { n: 3 }\n  f.<|>\n  let _x = 0\n}\n",
            &["next", "!enumerate", "!map"],
        ),
        (
            "no combinators without next",
            "struct Plain { a: int }\nfn r() {\n  let p = Plain { a: 1 }\n  p.<|>\n  let _n = 0\n}\n",
            &["a: int", "!enumerate", "!map", "!collect"],
        ),
        // The `-> Result {` body brace is not a `Result { }` literal.
        (
            "member in a fn returning a struct",
            "struct Result { value: int, ok: bool }\nfn c() -> Result {\n  \
             let res = Result { value: 1, ok: true }\n  return res.<|>\n}\n",
            &["value: int", "ok: bool"],
        ),
        (
            "underscore fields hidden",
            "struct Widget { value: int, _internal: int }\nfn f(w: Widget) {\n  w.<|>\n}\n",
            &["value*", "!_internal*"],
        ),
        (
            "underscore fields shown when typing `_`",
            "struct Widget { value: int, _internal: int }\nfn f(w: Widget) {\n  w._<|>\n}\n",
            &["_internal*"],
        ),
        (
            "Entity internals hidden, method API shown",
            "struct Mob { hp: int }\nfn f(e: Entity<Mob>) {\n  e.<|>\n}\n",
            &["get", "!arena:*", "!index:*", "!generation:*", "!_*"],
        ),
        (
            "UFCS methods on an inferred local",
            "struct Mob { hp: int }\nfn make_mob() -> Mob { Mob { hp: 1 } }\n\
             fn resolve_strike(mob: Mob) {}\nfn r() {\n  let m = make_mob()\n  m.<|>\n}\n",
            &["resolve_strike", "hp*"],
        ),
        (
            "UFCS methods on a chained inferred local",
            "struct Mob { hp: int }\nfn resolve_strike(mob: Mob) {}\n\
             fn r(e: Entity<Mob>) {\n  let m = e.get().unwrap()\n  m.resol<|>\n}\n",
            &["resolve_strike"],
        ),
        (
            "fuzzy field filter",
            "struct Mob { attack_co: int, health: int }\nfn r(m: Mob) {\n  m.atco<|>\n}\n",
            &["attack_co: int"],
        ),
        (
            "fuzzy method filter",
            "struct Movable { g: int }\nfn to_ivec(m: Movable) -> int { m.g }\n\
             fn r(mv: Movable) {\n  mv.tiv<|>\n}\n",
            &["to_ivec"],
        ),
        (
            "generic substituted inside a field type",
            "struct Mob { hp: int }\nstruct Box<T> { items: [T], best: T? }\n\
             fn r(b: Box<Mob>) {\n  b.<|>\n  let _n = 0\n}\n",
            &["items: [Mob]", "best: Mob?"],
        ),
        (
            "vec methods through an inferred local's field",
            "struct Item { kind: int }\nstruct Corpse { items: [Item], name: string }\n\
             fn mk() -> Corpse { return Corpse { items: [], name: \"\" } }\n\
             fn r() {\n  let c = mk()\n  let n = c.items.<|>le\n}\n",
            &["len"],
        ),
        (
            "chain through a generic field of a param",
            "struct Corpse { items: int, name: string }\n\
             struct Pair<A, B> { first: A, second: B }\n\
             fn r(p: Pair<Corpse, int>) {\n  let n = p.first.<|>na\n}\n",
            &["name: string"],
        ),
        (
            "chain through a generic field of an inferred local",
            "struct Corpse { items: int, name: string }\n\
             struct Pair<A, B> { first: A, second: B }\n\
             fn mk() -> Pair<Corpse, int> { return Pair { first: Corpse { items: 0, name: \"\" }, second: 1 } }\n\
             fn r() {\n  let p = mk()\n  let n = p.first.<|>na\n}\n",
            &["name: string"],
        ),
        (
            "chain through a tuple projection",
            "struct Corpse { items: int, name: string }\n\
             fn r(t: (Corpse, int)) {\n  let n = t.0.na<|>\n}\n",
            &["name: string"],
        ),
        // `foo` is undefined, so these receivers go through the structural
        // name-walk fallback instead of inference.
        (
            "fallback peels a newtype field",
            "struct Corpse { items: int, name: string }\nnewtype Wrap(Corpse)\n\
             fn r(w: Wrap) {\n  foo(w.0.na<|>\n}\n",
            &["name: string"],
        ),
        (
            "fallback resolves a tuple projection",
            "struct Corpse { items: int, name: string }\n\
             fn r(t: (Corpse, int)) {\n  foo(t.0.na<|>\n}\n",
            &["name: string"],
        ),
        (
            "fallback recovers an inferred tuple head",
            "struct Corpse { items: int, name: string }\n\
             fn mk() -> (Corpse, int) { return (Corpse { items: 0, name: \"\" }, 1) }\n\
             fn r() {\n  let t = mk()\n  foo(t.0.na<|>\n}\n",
            &["name: string"],
        ),
        (
            "fallback recovers an inferred newtype head",
            "struct Corpse { items: int, name: string }\nnewtype Wrap(Corpse)\n\
             fn mk() -> Wrap { return Wrap(Corpse { items: 0, name: \"\" }) }\n\
             fn r() {\n  let w = mk()\n  foo(w.0.na<|>\n}\n",
            &["name: string"],
        ),
        // A call receiver with no matching member offers nothing rather than
        // leaking globals as invalid UFCS (`centroid([Item])`).
        (
            "call receiver suppresses globals",
            "struct FixVec2 { x: float, y: float }\nstruct Item { kind: int }\n\
             fn centroid(v: [FixVec2]) -> FixVec2 { return v[0] }\n\
             fn getitems() -> [Item] { return [] }\n\
             fn r() {\n  let n = getitems().ce<|>\n}\n",
            &["!centroid", "!ceil"],
        ),
        (
            "call receiver offers its vec members",
            "struct Item { kind: int }\nfn getitems() -> [Item] { return [] }\n\
             fn r() {\n  let n = getitems().cl<|>\n}\n",
            &["clear"],
        ),
        // Struct-field comma recovery keeps the literal when `z: st.` runs into
        // the next field.
        (
            "missing comma in a struct literal",
            "struct St { hp: float, mp: float }\n\
             struct Drop { kind: int, z: float, vel: float }\n\
             fn f(st: St) {\n    let d = Drop {\n        kind: 0,\n        z: st.<|>\n        \
             vel: 1.0,\n    }\n}\n",
            &["hp*", "mp*"],
        ),
    ]);
}

#[test]
fn member_receiver_resolves_in_broken_statements() {
    // Each receiver sits where inference cannot type it: an argument of an
    // unknown call, or a binding whose own initializer fails.
    let decls = "struct Corpse { items: int, name: string }\n\
                 enum Slot { Full(Corpse), Empty }\n\
                 global LAST: Corpse = Corpse { items: 0, name: \"\" }\n";
    let cases: &[(&str, &str)] = &[
        ("param", "fn r(c: Corpse) {\n  foo(c.na<|>\n}\n"),
        (
            "struct-literal let",
            "fn r() {\n  let c = Corpse { items: 1, name: \"\" }\n  foo(c.na<|>\n}\n",
        ),
        (
            "for var",
            "fn r(cs: [Corpse]) {\n  for c in cs {\n    foo(c.na<|>\n  }\n}\n",
        ),
        (
            "match binding",
            "fn r(s: Slot) {\n  match s {\n    Slot::Full(c) => { foo(c.na<|> }\n    _ => {}\n  }\n}\n",
        ),
        (
            "annotated let, failed init",
            "fn r() {\n  let c: Corpse = nope()\n  foo(c.na<|>\n}\n",
        ),
        (
            "annotated let, failed init, plain statement",
            "fn r() {\n  let c: Corpse = nope()\n  let n = c.na<|>\n}\n",
        ),
        ("global", "fn r() {\n  foo(LAST.na<|>\n}\n"),
    ];
    for (what, body) in cases {
        assert_labels(what, &labels(&format!("{decls}{body}")), &["name: string"]);
    }
}

#[test]
fn member_completion_resolves_cross_file_types() {
    // The completion reparse includes the whole project, not just the edited
    // file, and Rock has one global namespace.
    let dir = project(&[
        ("a.rock", "struct IVec2 { x: int, y: int }\n"),
        ("b.rock", "fn f(g: IVec2) {\n  g.\n}\n"),
        (
            "types.rock",
            "struct Movable { grid: int }\nstruct Player { movable: Movable }\nglobal player: Player\n",
        ),
        ("use.rock", "fn r() {\n  let z = player\n}\n"),
    ]);
    let b = project_path(&dir, "b.rock");
    let l = labels_of(&complete_in(&b, "fn f(g: IVec2) {\n  g.<|>\n}\n"));
    assert_offers(&l, &["x: int", "y: int"]);
    let used = project_path(&dir, "use.rock");
    let l = labels_of(&complete_in(&used, "fn r() {\n  let z = player.<|>\n}\n"));
    assert_offers(&l, &["movable*"]);
}

#[test]
fn literal_and_block_contexts_offer() {
    check_labels(&[
        (
            "struct literal offers its fields",
            "struct FixVec2 { x: float, y: float }\nfn r() {\n  let v = FixVec2 {<|> \n}\n",
            &["x: float", "y: float", "!Arena"],
        ),
        (
            "struct literal skips written fields",
            "struct FixVec2 { x: float, y: float }\nfn r() {\n  let v = FixVec2 { x: 1.0,<|> \n}\n",
            &["y: float", "!x: float"],
        ),
        (
            "fuzzy struct literal field filter",
            "struct Cfg { move_speed: int, other: int }\nfn r() {\n  let c = Cfg { mosp<|> }\n}\n",
            &["move_speed: int"],
        ),
        (
            "enum record literal offers variant fields",
            "enum Effect { None, Bandage { duration: float, potency: int } }\n\
             fn r() {\n  let e = Effect::Bandage {\n  <|>}\n}\n",
            &["duration: float", "potency: int", "!Arena"],
        ),
        (
            "fuzzy enum record field filter",
            "enum E { V { alpha_beta: int, other: int } }\nfn r() {\n  let v = E::V { alb<|> }\n}\n",
            &["alpha_beta: int"],
        ),
        (
            "struct literal head inside a string",
            "struct FixVec2 { x: float, y: float }\nfn render() {}\n\
             fn r() {\n  let s = \"FixVec2<|> { \n}\n",
            &["!x: float"],
        ),
        (
            "match block is not a struct literal",
            "enum E { A, B }\nfn r(e: E) {\n  match e {\n    <|>\n  }\n}\n",
            &["E::A"],
        ),
        // A unit variant has no record fields; reading the `if` brace as a
        // record literal emptied all completion inside the block.
        (
            "block after a unit-variant condition",
            "enum SpellKind { Fire, Dispel }\nfn spell_clear() {}\n\
             fn fire(kind: SpellKind) {\n  if kind == SpellKind::Dispel {\n    spell_c<|>\n  }\n}\n",
            &["spell_clear"],
        ),
        (
            "block after a record-variant condition",
            "enum Effect { None, Bandage { duration: float } }\nfn spell_clear() {}\n\
             fn fire(e: Effect) {\n  if e == Effect::Bandage {\n    spell_c<|>\n  }\n}\n",
            &["spell_clear"],
        ),
        (
            "struct declaration field default is a typed value slot",
            "enum SpellKind { Fire, Harm }\nstruct SpellDef {\n  kind: SpellKind = h<|>ar\n}\n",
            &["*Harm*"],
        ),
        // Candidates come from `deco` declarations, not the fn namespace.
        (
            "decorators",
            "deco init late_draw {}\ndeco init late_update {}\ndeco init update {}\n\
             @late_dra<|>\nfn my_ui() {}\n",
            &["late_draw", "!update", "!my_ui"],
        ),
    ]);
}

#[test]
fn type_slots_offer_types_only() {
    check_labels(&[
        ("uint", "fn r(x: u<|>) {}\n", &["uint"]),
        // The full classifier matrix lives in the `backend::position` unit tests.
        (
            "bare enum, not its variants",
            "enum SpellState { Idle, Loaded(int) }\nstruct Holder { field: SpellState<|> }\n",
            &["SpellState", "!SpellState::*"],
        ),
        (
            "Coroutine builtin as its generic snippet",
            "co fn f() -> Co<|> {\n}\n",
            &["Coroutine<>", "!Coroutine"],
        ),
        ("Map builtin", "fn r() {\n  let m: Ma<|>\n}\n", &["Map"]),
        // `string` does not collide with a stdlib fn name (`int`/`float` do).
        ("string builtin", "fn f(x: stri<|>) {\n}\n", &["string"]),
        (
            "no macros",
            "macro twice(x: expr) {\n    x + x\n}\n\nfn r(a: tw<|>) {}\n",
            &["!twice"],
        ),
        (
            "param type slot",
            "struct Weapon { dmg: float }\nglobal wcount: int = 3\nfn wield(x: float) {}\n\
             fn use_weapon(w: W<|>) {}\n",
            &["Weapon", "!wcount", "!w", "!wield"],
        ),
        (
            "let annotation slot",
            "struct Weapon { dmg: float }\nglobal wcount: int = 3\nfn wield(x: float) {}\n\
             fn r() {\n  let x: w<|> = 0\n}\n",
            &["Weapon", "!wcount", "!wield"],
        ),
    ]);
}

#[test]
fn bool_is_offered_as_a_type_in_a_type_slot() {
    let kind = item("fn f(x: boo<|>) {\n}\n", "bool").kind;
    assert!(
        matches!(
            kind,
            Some(
                CompletionItemKind::CLASS
                    | CompletionItemKind::KEYWORD
                    | CompletionItemKind::STRUCT
            )
        ),
        "{kind:?}"
    );
}

#[test]
fn value_slots_offer() {
    check_labels(&[
        // Structs share the function bucket, so the item cap keeps them.
        (
            "struct types at an empty prefix",
            "struct Point { x: float, y: float }\n\
             fn create_point() -> Point { Point { x: 1.0, y: 2.0 } }\nfn r() {\n  <|>\n}\n",
            &["Point"],
        ),
        (
            "qualified variants, not the bare enum",
            "enum SpellState { Idle, Loaded(int) }\nfn r() {\n  let s = Spe<|>\n}\n",
            &["SpellState::Idle", "SpellState::Loaded", "!SpellState"],
        ),
        (
            "qualified variant matched by its variant name",
            "enum SpellState { Idle, Loaded(int) }\nfn r() {\n  let s = idl<|>\n}\n",
            &["SpellState::Idle"],
        ),
        (
            "unspaced `<` is a comparison, not a generic slot",
            "enum SpellState { Idle, Loaded(int) }\nfn r(hp: int) {\n  let ok = hp<Loa<|>\n}\n",
            &["SpellState::Loaded"],
        ),
        (
            "no Coroutine type name",
            "fn r() {\n  let x = Co<|>\n}\n",
            &["!Coroutine"],
        ),
        (
            "fuzzy qualified variant filter",
            "enum TargetError { NotGround, OutOfRange }\n\
             fn r() {\n  let e = TargetError::nogr<|>\n}\n",
            &["NotGround", "!OutOfRange"],
        ),
        // `pg` is a word-boundary acronym of `player.movable.grid` but only
        // scatters mid-word through `prop.image_dims()`, which is dropped.
        (
            "clean reach path offered, mid-word scatter filtered",
            "struct IVec2 { x: int }\nstruct Movable { grid: IVec2 }\n\
             struct Player { movable: Movable }\nstruct Prop { w: int }\n\
             fn image_dims(p: Prop) -> IVec2 { IVec2 { x: 0 } }\nglobal prop: Prop\n\
             fn sfx(g: IVec2) {}\nfn r(player: Player) {\n  sfx(pg<|>)\n}\n",
            &["player.movable.grid: IVec2", "!prop.image_dims*"],
        ),
        // A query absorbed by a fn name is fn-name browsing: no per-arg
        // call-through flood.
        (
            "no call-through when the query is the fn name",
            "fn spawn_thing(n: int) {}\nglobal N: int\nfn r() {\n  spawn<|>\n}\n",
            &["spawn_thing", "!spawn_thing(*"],
        ),
        (
            "fuzzy fn across `_` without call-through noise",
            "global PIXEL_EDITOR_TOOL_CURRENT_X: int\nfn tile_center(s: int) {}\n\
             fn table_new_with_size(n: int) {}\nfn r() {\n  tilecen<|>\n}\n",
            &["tile_center", "!*(*"],
        ),
        (
            "clean query finds a snake_case fn",
            "fn event_log() {}\nfn r() {\n  evlog<|>\n}\n",
            &["event_log"],
        ),
        (
            "let binding not in scope in its own initializer",
            "fn r() {\n  let speed = 5\n  let x = <|>speed\n}\n",
            &["speed", "!x"],
        ),
        (
            "let binding in scope after its statement",
            "fn r() {\n  let myvar = 5\n  let y = my<|>\n}\n",
            &["myvar"],
        ),
        (
            "non-FieldRef UFCS arg falls through",
            "struct Player { hp: int }\nfn take(o: Player, n: int) {}\n\
             fn helper_val() -> int { 0 }\n\
             fn r() {\n  let p: Player = Player { hp: 0 }\n  p.take(hel<|>)\n}\n",
            &["helper_val"],
        ),
        // Macros are expanded before lowering; completion reads the parse-time
        // capture, and stdlib macros live in the warm modules.
        (
            "macro",
            "macro twice(x: expr) {\n    x + x\n}\n\nfn r() {\n    tw<|>\n}\n",
            &["twice"],
        ),
        (
            "macro and fn under a shared prefix",
            "macro log_once(msg: expr) {\n    log(msg)\n}\nfn log_info(msg: string) {}\n\
             fn r() {\n    log_<|>\n}\n",
            &["log_once", "log_info"],
        ),
        ("stdlib macro", "fn r() {\n    log_o<|>\n}\n", &["log_once"]),
        (
            "identifier where a number could go",
            "fn remap_to_01(value: float, min1: float, max1: float) -> float {\n  value\n}\n\
             global DRAG_COMMIT: float = remap<|>\n",
            &["remap_to_01"],
        ),
    ]);
}

#[test]
fn code_around_comments_and_strings_completes() {
    const HELPER: &str = "fn helper_thing() {}\nfn main() {\n";
    const MOB: &str = "struct Mob { health: int }\nfn helper_thing() {}\n\
                       fn main() {\n    let mob = Mob { health: 1 }\n";
    let cases: &[(&str, &str, &str)] = &[
        (
            HELPER,
            "    let u = \"http://x\"\n    hel<|>\n}\n",
            "helper_thing",
        ),
        (HELPER, "    hel<|>// note\n}\n", "helper_thing"),
        // `#if` lexes as a directive, not a `#` comment.
        (
            HELPER,
            "    #if debug\n    hel<|>\n    #end\n}\n",
            "helper_thing",
        ),
        (HELPER, "    /* note */ hel<|>\n}\n", "helper_thing"),
        (MOB, "    print(\"x={hel<|>}\")\n}\n", "helper_thing"),
        (MOB, "    print(\"hp={mob.hea<|>}\")\n}\n", "health: int"),
        (
            MOB,
            "    print(\"a {mob.health} b {hel<|>}\")\n}\n",
            "helper_thing",
        ),
        (
            MOB,
            "    let t = \"\"\"t {hel<|>} u\"\"\"\n}\n",
            "helper_thing",
        ),
        (MOB, "    print(\"x={hel<|>\n}\n", "helper_thing"),
        (MOB, "    let s = \"txt\"\n    hel<|>\n}\n", "helper_thing"),
    ];
    for (head, body, label) in cases {
        assert_labels(body, &labels(&format!("{head}{body}")), &[label]);
    }
}

#[test]
fn comments_strings_and_numbers_offer_nothing() {
    const HELPER: &str = "fn helper_thing() {}\nfn main() {\n";
    const MOB: &str = "struct Mob { health: int }\nfn helper_thing() {}\n\
                       fn main() {\n    let mob = Mob { health: 1 }\n";
    let bodies: &[(&str, &str)] = &[
        (HELPER, "    // plain hel<|>\n}\n"),
        (HELPER, "    /// docs hel<|>\n}\n"),
        (HELPER, "    let x = 1 // hel<|>\n}\n"),
        (HELPER, "    # hash hel<|>\n}\n"),
        (HELPER, "    /* block hel<|> */\n}\n"),
        (HELPER, "    /* one\n       hel<|>\n     */\n}\n"),
        // LSP columns are UTF-16 units; comment scanning is byte offsets.
        (HELPER, "    // caf\u{e9} hel<|>\n}\n"),
        (MOB, "    print(\"some hel<|>\")\n}\n"),
        (MOB, "    print(\"{mob.health} hel<|>\")\n}\n"),
        (MOB, "    print(\"{mob.health} hel<|> {mob.health}\")\n}\n"),
        // `{{` is a literal `{`, not a hole.
        (MOB, "    print(\"{{hel<|>}}\")\n}\n"),
        (MOB, "    print(r\"raw hel<|>\")\n}\n"),
        (MOB, "    print(r\"raw {hel<|>}\")\n}\n"),
        (MOB, "    let t = \"\"\"triple hel<|>\"\"\"\n}\n"),
        // The closing quote is not typed yet.
        (MOB, "    print(\"some hel<|>\n}\n"),
        // A literal nested in a hole is still text.
        (MOB, "    print(\"{join(v, \"hel<|>\")}\")\n}\n"),
    ];
    for (head, body) in bodies {
        assert_eq!(
            labels(&format!("{head}{body}")),
            Vec::<String>::new(),
            "{body}"
        );
    }
    check_empty(&[
        // The byte scanner was comment-blind: these classified as an enum
        // qualifier, a member access and a yield operand.
        (
            "qualifier in a comment",
            "enum SpellState { Idle, Loaded(int) }\nfn idle_check() {}\n\
             fn r() {\n  // SpellState::i<|>\n}\n",
        ),
        (
            "member path in a comment",
            "struct Config { volume: float }\nglobal cfg: Config = Config { volume: 1.0 }\n\
             fn volume_reset() {}\nfn r() {\n  // cfg.vo<|>l\n}\n",
        ),
        (
            "yield in a comment",
            "co fn frame_co() -> Coroutine<int> {\n  yield 1\n}\nfn frame() {}\n\
             fn t() {\n  // yield fr<|>\n}\n",
        ),
        // Identifiers cannot start with a digit; `0` once matched `remap_to_01`.
        (
            "numeric literal",
            "fn remap_to_01(value: float, min1: float, max1: float) -> float {\n  value\n}\n\
             global DRAG_COMMIT: float = 0<|>\n",
        ),
    ]);
}

#[test]
fn magic_comment_completes_directives_only() {
    let cases: &[(&str, &str, &[&str])] = &[
        (
            "directive verb",
            "fn main() {\n    // rock: <|>\n    let unused_thing = 5\n}\n",
            &["allow"],
        ),
        (
            "lint names",
            "fn main() {\n    // rock: allow(<|>\n    let unused_thing = 5\n}\n",
            &["unused_var", "unused_fn"],
        ),
        (
            "lint names filtered by prefix",
            "fn main() {\n    // rock: allow(unused_v<|>\n    let unused_thing = 5\n}\n",
            &["unused_var", "!unused_fn"],
        ),
        (
            "no code symbols",
            "fn unused_helper() {}\nfn main() {\n    // rock: allow(un<|>\n}\n",
            &["!unused_helper", "unused_var"],
        ),
    ];
    for (case, src, patterns) in cases {
        assert_labels(case, &labels(src), patterns);
    }
}

#[test]
fn space_trigger_on_an_indentation_only_line_is_suppressed() {
    // Space triggers completion (`let x = |`), but `<CR>` autoindent inserts
    // spaces too, and a menu there makes `<CR><CR>` accept an item.
    let cases = [
        ("    let x = <|>", true, true),
        ("    <|>", true, false),
        ("    <|>", false, true),
    ];
    for (line, space, offers) in cases {
        let (src, pos) = cursor(&format!("fn f() {{\n    let count = 1\n{line}\n}}\n"));
        let mut bone = BoneWorkspace::new();
        bone.update(PATH, src);
        let n = bone.completion_with_trigger(PATH, pos, space).len();
        assert_eq!(n > 0, offers, "{line:?} space={space}: {n} items");
    }
}

#[test]
fn field_refs_offer_exactly_the_fields_of_their_value_type() {
    check_labels(&[
        // FieldRef<S, V> is invariant in V.
        (
            "V = int?",
            "struct Mob { health: int, attack_co: int?, name: string }\n\
             fn poll_slot(f: FieldRef<Mob, int?>) {}\nfn main() {\n  poll_slot(.<|>)\n}\n",
            &[
                ".attack_co: int?",
                "!.health*",
                "!.name*",
                "!Arena",
                "!poll_slot",
            ],
        ),
        (
            "V = int",
            "struct Mob { health: int, attack_co: int?, name: string }\n\
             fn want_int(f: FieldRef<Mob, int>) {}\nfn main() {\n  want_int(.<|>)\n}\n",
            &[".health: int", "!.attack_co*"],
        ),
        (
            "T bound by a sibling arg",
            "struct Mob { hp: int, co: int? }\nstruct Entity<T> { i: int }\n\
             fn poll_slot<T>(e: Entity<T>, f: FieldRef<T, int?>) {}\n\
             fn r() {\n  let e: Entity<Mob> = Entity { i: 0 }\n  poll_slot(e, .co<|>)\n}\n",
            &[".co: int?", "!.hp*"],
        ),
        (
            "empty arg slot",
            "struct Mob { hp: int, co: int? }\nfn poll_slot(f: FieldRef<Mob, int?>) {}\n\
             fn r() {\n  poll_slot(<|>)\n}\n",
            &["!.hp*", "!poll_slot"],
        ),
        (
            "T bound by a UFCS receiver",
            "struct Player { hp: int, cast_co: int? }\nfn stop<T>(o: T, f: FieldRef<T, int?>) {}\n\
             fn r() {\n  let p: Player = Player { hp: 0, cast_co: null }\n  p.stop(.<|>)\n}\n",
            &[".cast_co: int?", "!.hp*", "!stop", "!Player"],
        ),
        // Every overload is consulted for the FieldRef slot.
        (
            "second overload supplies the slot",
            "struct Player { hp: int, cast_co: int? }\nfn stop(t: int) {}\n\
             fn stop<T>(o: T, f: FieldRef<T, int?>) {}\n\
             fn r() {\n  let p: Player = Player { hp: 0, cast_co: null }\n  p.stop(.<|>)\n}\n",
            &[".cast_co: int?", "!stop"],
        ),
        (
            "T bound through an Entity receiver",
            "struct Glyph { life: int, co: int? }\nstruct Entity<T> { i: int }\n\
             fn run_into<T>(o: T, f: FieldRef<T, int?>, co: int) {}\n\
             fn run_into<T>(e: Entity<T>, f: FieldRef<T, int?>, co: int) {}\n\
             fn r() {\n  let e: Entity<Glyph> = Entity { i: 0 }\n  e.run_into(.<|>)\n}\n",
            &[".co: int?", "!.life*", "!run_into"],
        ),
        // A generic-receiver FieldRef method is callable only with a field to pass.
        (
            "FieldRef method absent without a matching field",
            "struct Mob { health: int, name: string }\n\
             fn kill_co<T>(o: T, f: FieldRef<T, int?>) {}\nfn r(mob: Mob) {\n  mob.kil<|>\n}\n",
            &["!kill_co"],
        ),
    ]);
}

#[test]
fn coroutine_contexts_offer() {
    check_labels(&[
        (
            "yield in a co fn",
            "co fn f() -> Coroutine<int> {\n  yi<|>\n}\n",
            &["yield"],
        ),
        (
            "await in a co fn",
            "co fn f() -> Coroutine<int> {\n  aw<|>\n}\n",
            &["await"],
        ),
        (
            "no yield or await in a plain fn",
            "fn f() -> int {\n  yi<|>\n}\n",
            &["!yield", "!await"],
        ),
        // The `Wait` ergonomics need a `Wait` type.
        (
            "no Wait without the type",
            "co fn f() -> Coroutine<<|>> {\n  yield 0\n}\n",
            &["!Wait"],
        ),
        (
            "only the generic snippet without Wait",
            "co fn f() -> Cor<|> {\n  yield 0\n}\n",
            &["Coroutine<>", "!Coroutine<Wait>", "!Coroutine"],
        ),
        (
            "Coroutine snippets replace the bare name",
            "struct Wait { wait_frame: bool }\nco fn f() -> Cor<|> {\n  yield 0\n}\n",
            &["!Coroutine"],
        ),
        (
            "no return type snippet on a plain fn",
            "struct Wait { wait_frame: bool }\nfn spawn() <|>{\n}\n",
            &["!-> Coroutine*"],
        ),
    ]);
}

#[test]
fn entity_param_hints_offer() {
    // A param named `e` suggests `Entity`; a `_`-segment of the fn name that
    // names a struct picks the payload, leading segment first.
    check_labels(&[
        (
            "no payload struct",
            "co fn render_thing(e: <|>) -> Coroutine<int> {\n  yield 1\n}\n",
            &["!Entity<Mob>"],
        ),
        (
            "not an entity param name",
            "struct Mob { health: int }\n\
             co fn mob_attack_co(count: <|>) -> Coroutine<int> {\n  yield 1\n}\n",
            &["!Entity<Mob>"],
        ),
        (
            "leading segment wins",
            "struct Mob { health: int }\nstruct Tile { x: int }\n\
             co fn mob_to_tile(e: <|>) -> Coroutine<int> {\n  yield 1\n}\n",
            &["Entity<Mob>", "!Entity<Tile>"],
        ),
        (
            "a let binding is not a param",
            "struct Mob { health: int }\nfn mob_attack_co() {\n  let e: <|>\n}\n",
            &["!Entity<Mob>"],
        ),
    ]);
}

#[test]
fn accepting_a_member_inserts() {
    check_inserts(&[
        (
            "field inserts only its name",
            "struct Movable { grid: int }\nstruct Player { movable: Movable }\n\
             fn r(player: Player) {\n  let z = player.<|>\n}\n",
            "movable: Movable",
            Text("movable"),
        ),
        // The reachability index walks fields and UFCS methods through the
        // receiver in an arg slot of a known type.
        (
            "transitive path",
            "struct IVec2 { x: int }\nstruct Movable { grid: IVec2 }\n\
             struct Player { movable: Movable }\nfn tile_center(g: IVec2) {}\n\
             fn r(player: Player) {\n  tile_center(player.<|>)\n}\n",
            "movable.grid: IVec2",
            Text("movable.grid"),
        ),
        (
            "field-then-method path",
            "struct IVec2 { x: int }\nstruct Movable { grid: int }\n\
             struct Player { movable: Movable }\n\
             fn to_ivec(m: Movable) -> IVec2 { IVec2 { x: 0 } }\n\
             fn tile_center(g: IVec2) {}\nfn r(player: Player) {\n  tile_center(player.<|>)\n}\n",
            "movable.to_ivec() -> IVec2",
            Text("movable.to_ivec()"),
        ),
        (
            "no-arg UFCS method parks the cursor after `()`",
            "struct Counter { value: int }\nfn reset(c: Counter) -> int { 0 }\n\
             fn add(c: Counter, n: int) -> int { 0 }\nfn run(c: Counter) {\n  c.<|>\n}\n",
            "reset",
            Plain("reset()"),
        ),
        (
            "UFCS method with an arg left snippets into it",
            "struct Counter { value: int }\nfn reset(c: Counter) -> int { 0 }\n\
             fn add(c: Counter, n: int) -> int { 0 }\nfn run(c: Counter) {\n  c.<|>\n}\n",
            "add",
            Snippet("add($0)"),
        ),
        (
            "UFCS method reuses a following `(`",
            "struct Mob { hp: int }\nfn poke(m: Mob, n: int) -> int { n }\n\
             fn r(mob: Mob) {\n  mob.pok<|>(3)\n}\n",
            "poke",
            Plain("poke"),
        ),
        (
            "vec method reuses a following `(`",
            "struct Item { kind: int }\nstruct Corpse { items: [Item], name: string }\n\
             fn r(c: Corpse) {\n  let n = c.items.le<|>()\n}\n",
            "len",
            Text("len"),
        ),
        (
            "vec method brings its own `()`",
            "struct Item { kind: int }\nstruct Corpse { items: [Item], name: string }\n\
             fn r(c: Corpse) {\n  let n = c.items.le<|>\n}\n",
            "len",
            Text("len()"),
        ),
        (
            "enum call receiver takes UFCS methods",
            "enum Dir { N, S }\nnewtype Meters(float)\nfn flip(d: Dir) -> Dir { return d }\n\
             fn halve(m: Meters) -> Meters { return m }\nfn dir() -> Dir { return Dir::N }\n\
             fn dist() -> Meters { return Meters(1.0) }\n\
             fn r() {\n  let a = dir().fl<|>\n  let b = dist().ha\n}\n",
            "flip",
            Text("flip()"),
        ),
        (
            "newtype call receiver takes UFCS methods",
            "enum Dir { N, S }\nnewtype Meters(float)\nfn flip(d: Dir) -> Dir { return d }\n\
             fn halve(m: Meters) -> Meters { return m }\nfn dir() -> Dir { return Dir::N }\n\
             fn dist() -> Meters { return Meters(1.0) }\n\
             fn r() {\n  let a = dir().fl\n  let b = dist().ha<|>\n}\n",
            "halve",
            Text("halve()"),
        ),
        (
            "Entity get().unwrap() chain",
            "struct Mob { health: int }\nfn f(e: Entity<Mob>) {\n  e.getu<|>\n}\n",
            "get().unwrap()",
            Text("get().unwrap()"),
        ),
        // A FieldRef method fills its field arg at method selection. Behind a
        // bare generic receiver it is enumerated separately; behind a nominal
        // `Entity<T>` the prefill rewrites the one `ufcs_methods` item.
        (
            "FieldRef method, one matching field",
            "struct Mob { health: int, attack_co: int?, name: string }\n\
             fn kill_co<T>(o: T, f: FieldRef<T, int?>) {}\nfn r(mob: Mob) {\n  mob.kil<|>\n}\n",
            "kill_co",
            Snippet("kill_co(.attack_co)"),
        ),
        (
            "FieldRef method, a choice of fields in declaration order",
            "struct Mob { health: int, attack_co: int?, cast_co: int?, name: string }\n\
             fn kill_co<T>(o: T, f: FieldRef<T, int?>) {}\nfn r(mob: Mob) {\n  mob.kil<|>\n}\n",
            "kill_co",
            Snippet("kill_co(${1|.attack_co,.cast_co|})"),
        ),
        (
            "FieldRef method reuses a following `(`",
            "struct Mob { health: int, attack_co: int?, name: string }\n\
             fn kill_co<T>(o: T, f: FieldRef<T, int?>) {}\nfn r(mob: Mob) {\n  mob.kil<|>()\n}\n",
            "kill_co",
            Plain("kill_co"),
        ),
        (
            "FieldRef method, FieldRef not the trailing param",
            "struct Mob { health: int, attack_co: int?, name: string }\n\
             fn set_co<T>(o: T, f: FieldRef<T, int?>, next: int?) {}\n\
             fn r(mob: Mob) {\n  mob.se<|>\n}\n",
            "set_co",
            Snippet("set_co(.attack_co, ${1:next})"),
        ),
        (
            "FieldRef method on an Entity receiver",
            "struct Mob { health: int, attack_co: int?, name: string }\n\
             fn run_into<T>(e: Entity<T>, f: FieldRef<T, int?>, co: int) {}\n\
             fn r(e: Entity<Mob>) {\n  e.run<|>\n}\n",
            "run_into",
            Snippet("run_into(.attack_co, ${1:co})"),
        ),
        (
            "FieldRef method on an Entity receiver, a choice of fields",
            "struct Mob { health: int, attack_co: int?, cast_co: int?, name: string }\n\
             fn run_into<T>(e: Entity<T>, f: FieldRef<T, int?>, co: int) {}\n\
             fn r(e: Entity<Mob>) {\n  e.run<|>\n}\n",
            "run_into",
            Snippet("run_into(${1|.attack_co,.cast_co|}, ${2:co})"),
        ),
        (
            "FieldRef arg in an empty slot adds the dot",
            "struct Mob { hp: int, co: int? }\nfn poll_slot(f: FieldRef<Mob, int?>) {}\n\
             fn r() {\n  poll_slot(<|>)\n}\n",
            ".co: int?",
            Text(".co"),
        ),
        (
            "decorator inserts only its name",
            "deco init late_draw {}\n@late_dra<|>\nfn my_ui() {}\n",
            "late_draw",
            Text("late_draw"),
        ),
    ]);
}

#[test]
fn accepting_a_value_inserts() {
    check_inserts(&[
        (
            "callee reuses a following `(`",
            "fn world_to_design(g: int) -> int { g }\n\
             fn r() {\n  world_to_des<|>(tile_center)\n}\n",
            "world_to_design",
            Plain("world_to_design"),
        ),
        (
            "no-param fn parks the cursor after `()`",
            "fn tick() -> int { 0 }\nfn run() {\n  tic<|>\n}\n",
            "tick",
            Plain("tick()"),
        ),
        // Reach paths to the expected arg type; the label carries the type.
        (
            "field path",
            "struct SpellDef { kind: SpellKind }\nenum SpellKind { Fire, Ice }\n\
             fn sfx_spell(k: SpellKind) {}\n\
             fn r() {\n  let spell: SpellDef = SpellDef { kind: SpellKind::Fire }\n  sfx_spell(speki<|>)\n}\n",
            "spell.kind: SpellKind",
            Text("spell.kind"),
        ),
        (
            "recursive field path",
            "struct IVec2 { x: int }\nstruct Movable { grid: IVec2 }\n\
             struct Player { movable: Movable }\nfn sfx(g: IVec2) {}\n\
             fn r(player: Player) {\n  sfx(plgr<|>)\n}\n",
            "player.movable.grid: IVec2",
            Text("player.movable.grid"),
        ),
        (
            "UFCS method path",
            "struct SpellDef { kind: int }\nfn spell_dps(s: SpellDef) -> float { 0.0 }\n\
             fn need_dps(d: float) {}\n\
             fn r() {\n  let spell: SpellDef = SpellDef { kind: 0 }\n  need_dps(spell_d<|>)\n}\n",
            "spell.spell_dps() -> float",
            Text("spell.spell_dps()"),
        ),
        (
            "field-then-method path",
            "struct IVec2 { x: int }\nstruct Movable { grid: int }\n\
             struct Player { movable: Movable }\n\
             fn to_ivec(m: Movable) -> IVec2 { IVec2 { x: 0 } }\nfn sfx(g: IVec2) {}\n\
             fn r(player: Player) {\n  sfx(plmoiv<|>)\n}\n",
            "player.movable.to_ivec() -> IVec2",
            Text("player.movable.to_ivec()"),
        ),
        // A reach path typed over an existing `.rect()` tail drops that tail.
        (
            "reach path does not duplicate a member tail",
            "struct UiRect {}\nstruct Panel {}\nfn rect(self: Panel) -> UiRect { UiRect {} }\n\
             global PANEL: Panel = Panel {}\nfn push(a: int, r: UiRect) {}\n\
             fn caller() {\n    push(0, PANE<|>.rect(),\n        1)\n}\n",
            "PANEL.rect() -> UiRect",
            Text("PANEL"),
        ),
        // Call-through: the fn name absorbs a query prefix and the remainder
        // matches the first arg.
        (
            "call through an enum variant",
            "enum TargetError { NotGround, OutOfRange }\nfn reject_target(e: TargetError) {}\n\
             fn r() {\n  rejtanog<|>\n}\n",
            "reject_target(TargetError::NotGround)",
            Text("reject_target(TargetError::NotGround)"),
        ),
        (
            "call through with more params snippets to the next",
            "enum TargetError { NotGround }\nfn reject_target(e: TargetError, reason: int) {}\n\
             fn r() {\n  rejtanog<|>\n}\n",
            "reject_target(TargetError::NotGround)",
            Snippet("reject_target(TargetError::NotGround, $0)"),
        ),
        (
            "call through an in-scope value",
            "fn apply_damage(amount: float) {}\nfn r() {\n  let dt: float = 1.0\n  apdt<|>\n}\n",
            "apply_damage(dt)",
            Text("apply_damage(dt)"),
        ),
        (
            "call through a tuple variant",
            "enum Damage { Fire(int), Cold }\nfn deal(d: Damage) {}\nfn r() {\n  dealfir<|>\n}\n",
            "deal(Damage::Fire(...))",
            Snippet("deal(Damage::Fire($0))"),
        ),
        (
            "call through a record variant",
            "enum Cfg { Opts { volume: int }, Mute }\nfn apply(c: Cfg) {}\n\
             fn r() {\n  applyopt<|>\n}\n",
            "apply(Cfg::Opts { ... })",
            Snippet("apply(Cfg::Opts { $0 })"),
        ),
        (
            "call through a tuple variant with more params",
            "enum Damage { Fire(int), Cold }\nfn deal(d: Damage, n: int) {}\n\
             fn r() {\n  dealfir<|>\n}\n",
            "deal(Damage::Fire(...))",
            Text("deal(Damage::Fire($1), $0)"),
        ),
        (
            "call through a reach path",
            "struct IVec2 { x: int }\nstruct Movable { grid: IVec2 }\n\
             struct Player { movable: Movable }\nfn tile_center(g: IVec2) {}\n\
             fn r(player: Player) {\n  tilecpmg<|>\n}\n",
            "tile_center(player.movable.grid)",
            Text("tile_center(player.movable.grid)"),
        ),
        // Lifecycle hooks are also stdlib fns, but complete as a block.
        (
            "lifecycle hook on_killed",
            "co fn f() -> Coroutine<int> {\n  on_kil<|>\n  yield 1\n}\n",
            "on_killed",
            Snippet("on_killed { $0 }"),
        ),
        (
            "lifecycle hook on_hot_reload",
            "co fn f() -> Coroutine<int> {\n  on_hot<|>\n  yield 1\n}\n",
            "on_hot_reload",
            Text("on_hot_reload { $0 }"),
        ),
    ]);
}

#[test]
fn enum_variants_template_their_payload_only_in_value_position() {
    // A value position constructs: a tuple variant inserts `V($0)`, a record
    // variant `V { $0 }`. A pattern binds, so it gets no template.
    const STATE: &str = "enum SpellState { Idle, Loaded(int) }\n";
    let cases: &[(&str, &str, &str, Insert)] = &[
        (
            "qualified tuple variant",
            "fn r() {\n  let s = Load<|>\n}\n",
            "SpellState::Loaded",
            Snippet("SpellState::Loaded($0)"),
        ),
        (
            "tuple variant",
            "fn r() {\n  let s = SpellState::<|>\n}\n",
            "Loaded",
            Snippet("Loaded($0)"),
        ),
        (
            "unit variant",
            "fn r() {\n  let s = SpellState::<|>\n}\n",
            "Idle",
            Label,
        ),
        (
            "arm pattern",
            "fn r(s: SpellState) {\n  match s {\n    SpellState::<|> => {}\n  }\n}\n",
            "Loaded",
            Label,
        ),
        // No `=>` yet: the enclosing `match` still makes it a pattern.
        (
            "fresh arm pattern",
            "fn r(s: SpellState) {\n  match s {\n    SpellState::<|>\n  }\n}\n",
            "Loaded",
            Label,
        ),
        (
            "partial fresh arm pattern",
            "fn r(s: SpellState) {\n  match s {\n    SpellState::Loa<|>\n  }\n}\n",
            "Loaded",
            Label,
        ),
        (
            "arm body",
            "fn r(s: SpellState) {\n  match s {\n    SpellState::Idle => SpellState::<|>\n  }\n}\n",
            "Loaded",
            Snippet("Loaded($0)"),
        ),
        (
            "first statement of a plain block",
            "fn r() {\n  SpellState::<|>\n}\n",
            "Loaded",
            Snippet("Loaded($0)"),
        ),
    ];
    for (case, body, label, want) in cases {
        assert_insert(case, &item(&format!("{STATE}{body}"), label), *want);
    }
    check_inserts(&[
        (
            "record variant",
            "enum Effect { None, Bandage { duration: float } }\nfn r() {\n  let e = Effect::<|>\n}\n",
            "Bandage",
            Snippet("Bandage { $0 }"),
        ),
        (
            "arm head binds",
            "enum Sig { Num(int), Flag }\nfn f(s: Sig) {\n  match s {\n    <|>\n  }\n}\n",
            "Sig::Num",
            Label,
        ),
    ]);
}

#[test]
fn accepting_a_type_inserts() {
    check_inserts(&[
        (
            "Coroutine<Wait>",
            "struct Wait { wait_frame: bool }\nco fn f() -> Cor<|> {\n  yield 0\n}\n",
            "Coroutine<Wait>",
            Text("Coroutine<Wait>"),
        ),
        (
            "Coroutine<$0>",
            "struct Wait { wait_frame: bool }\nco fn f() -> Cor<|> {\n  yield 0\n}\n",
            "Coroutine<>",
            Snippet("Coroutine<$0>"),
        ),
        (
            "co fn return type",
            "struct Wait { wait_frame: bool }\nco fn spawn() <|>{\n  yield 0\n}\n",
            "-> Coroutine<Wait>",
            Text("-> Coroutine<Wait>"),
        ),
        (
            "Entity of the fn-prefix struct",
            "struct Mob { health: int }\nco fn mob_attack_co(e: <|>) -> Coroutine<int> {\n  yield 1\n}\n",
            "Entity<Mob>",
            Text("Entity<Mob>"),
        ),
        (
            "Entity payload right after the colon",
            "struct Mob { health: int }\nco fn mob_attack_co(e:<|>   ) -> Coroutine<int> {\n  yield 1\n}\n",
            "Entity<Mob>",
            Text("Entity<Mob>"),
        ),
        (
            "Entity payload after one space",
            "struct Mob { health: int }\nco fn mob_attack_co(e: <|>  ) -> Coroutine<int> {\n  yield 1\n}\n",
            "Entity<Mob>",
            Text("Entity<Mob>"),
        ),
        (
            "Entity payload after two spaces",
            "struct Mob { health: int }\nco fn mob_attack_co(e:  <|> ) -> Coroutine<int> {\n  yield 1\n}\n",
            "Entity<Mob>",
            Text("Entity<Mob>"),
        ),
        (
            "Entity snippet without a payload struct",
            "co fn render_thing(e: <|>) -> Coroutine<int> {\n  yield 1\n}\n",
            "Entity<>",
            Snippet("Entity<$0>"),
        ),
        (
            "Entity snippet in a plain fn, at the colon",
            "fn clear_attack(e:<|> ) {\n}\n",
            "Entity<>",
            Text("Entity<$0>"),
        ),
        (
            "Entity snippet in a plain fn, after the space",
            "fn clear_attack(e: <|>) {\n}\n",
            "Entity<>",
            Text("Entity<$0>"),
        ),
    ]);
}

#[test]
fn function_completion_carries_its_signature() {
    let it = item(
        "fn separation(a: int, b: int) -> int { a }\nfn run() {\n  sep<|>\n}\n",
        "separation",
    );
    assert_eq!(
        it.detail.as_deref(),
        Some("fn separation(a: int, b: int) -> int")
    );
    let inline = it.label_details.as_ref().and_then(|d| d.detail.as_deref());
    assert_eq!(inline, Some("(a: int, b: int) -> int"));
    assert_insert("signature", &it, Snippet("separation($0)"));
}

#[test]
fn enum_qualifier_without_a_matching_variant_offers_them_all() {
    // A closed set: showing every variant beats falling back to buffer words.
    // `filter_text` is the typed prefix so the client's own filter keeps them.
    let src = "enum TextAlign { Left, Center, Right }\nfn r() {\n  let a = TextAlign::top<|>\n}\n";
    let items = complete(src);
    let mut all = labels_of(&items);
    all.sort();
    assert_eq!(all, ["Center", "Left", "Right"]);
    assert_eq!(find(&items, "Center").filter_text.as_deref(), Some("top"));

    let src = "enum TextAlign { Left, Center, Right }\nfn r() {\n  let a = TextAlign::Ce<|>\n}\n";
    let items = complete(src);
    assert_eq!(labels_of(&items), ["Center"]);
    assert_eq!(find(&items, "Center").filter_text, None);
}

#[test]
fn reachability_index_rebuilds_on_an_interface_edit() {
    // Adding a field that reaches the expected type is an interface edit; a
    // stale index would miss the new `w.v` path.
    let (before, pos) = cursor(
        "struct V { y: int }\nstruct W { a: int }\nfn need(v: V) {}\nfn r(w: W) {\n  need(wv<|>)\n}\n",
    );
    let after = before.replace("W { a: int }", "W { v: V }");
    let mut bone = BoneWorkspace::new();
    let mut has_wv = |src: String| {
        bone.update(PATH, src);
        bone.completion(PATH, pos)
            .into_iter()
            .any(|c| c.insert_text.as_deref() == Some("w.v"))
    };
    assert!(!has_wv(before), "no `w.v` path before W has a V field");
    assert!(has_wv(after), "`w.v` path after the interface edit");
}

#[test]
fn contexts_order_candidates() {
    const DOOR: &str = "struct Door { ajar: float, accessible: bool }\n";
    // `bool` fields get a soft boost when the chain is a whole `if`/`while`
    // condition: it wins a tie but never beats a better textual match.
    let door: &[(&str, &str, &str)] = &[
        (
            "if condition",
            "fn f(door: Door) {\n  if door.a<|>\n}\n",
            "accessible* < ajar*",
        ),
        (
            "while condition",
            "fn f(door: Door) {\n  while door.a<|>\n}\n",
            "accessible* < ajar*",
        ),
        (
            "not a condition",
            "fn f(door: Door) {\n  let x = door.a<|>\n}\n",
            "ajar* < accessible*",
        ),
        (
            "after `&&`",
            "fn f(door: Door, x: bool) {\n  if x && door.a<|>\n}\n",
            "ajar* < accessible*",
        ),
    ];
    for (case, body, order) in door {
        assert_ranked(case, &complete(&format!("{DOOR}{body}")), order);
    }
    check_ranked(&[
        (
            "struct literal: required fields first, then declaration order",
            "struct Cfg { scale: float = 1.0, name: string, volume: float = 0.5, id: int }\n\
             fn r() {\n  let c = Cfg { <|>\n}\n",
            "name: string < id: int < scale: float < volume: float",
        ),
        (
            "arg slot: a value of the type, then a fn returning it",
            "struct SpellDef { kind: int }\nfn spell_make() -> SpellDef { SpellDef { kind: 0 } }\n\
             fn spell_dps(s: SpellDef) -> float { 0.0 }\nfn use_spell(s: SpellDef) {}\n\
             fn r() {\n  let spell: SpellDef = SpellDef { kind: 0 }\n  use_spell(spe<|>)\n}\n",
            "spell < spell_make < spell_dps",
        ),
        // Without an expected type candidates rank by kind; enum-member variants
        // come last.
        (
            "untyped slot: variables, functions and structs, then variants",
            "enum AggressorRef { PlayerRef, NpcRef }\nenum ClickTarget { Player, Ground }\n\
             struct Player { name: string }\nglobal player: Player? = null\n\
             global player_dead: bool = false\nfn play_sound() {}\n\
             fn r() -> int {\n  play<|>\n  return 0\n}\n",
            "player < player_dead < play_sound < AggressorRef::PlayerRef; \
             player_dead < Player < AggressorRef::PlayerRef; player < ClickTarget::Player",
        ),
        // Match quality dominates path length, and case only breaks ties.
        (
            "prefix-matching reach path above a scattered one",
            "struct IVec2 { x: int }\nstruct Movable { grid: IVec2 }\n\
             struct Player { movable: Movable }\nstruct Image { dims: IVec2 }\n\
             global PIXEL_LAYER: Image\nfn sfx(g: IVec2) {}\n\
             fn r(player: Player) {\n  sfx(play<|>)\n}\n",
            "player.movable.grid: IVec2 < PIXEL_LAYER.dims: IVec2",
        ),
        // `nog` scatters mid-word through `event_log` but hits camelCase
        // boundaries in `NotGround`: both admitted, the band orders them.
        (
            "camelCase match above a mid-word scatter",
            "fn event_log() {}\nenum E { NotGround }\nfn r() {\n  nog<|>\n}\n",
            "E::NotGround < event_log",
        ),
        (
            "coroutine fn first after await",
            "co fn frame(n: int) -> Coroutine<int> {\n  yield 0\n}\n\
             fn fragment(n: int) -> int { n }\nco fn g() -> Coroutine<int> {\n  await fr<|>\n}\n",
            "frame < fragment",
        ),
        (
            "concrete Entity payload above the bare Entity",
            "struct Mob { health: int }\nco fn mob_attack_co(e: <|>) -> Coroutine<int> {\n  yield 1\n}\n",
            "Entity<Mob> < Entity",
        ),
        (
            "shorter of two prefix matches first",
            "struct Mob { attack_timer: float, attacking: bool }\n\
             fn f(mob: Mob) {\n  if mob.attack<|>\n}\n",
            "attacking* < attack_timer*",
        ),
        // No match to be tighter about, so no length tiebreak.
        (
            "empty prefix is alphabetical",
            "struct S { aaa_long: int, z: int }\nfn f(s: S) {\n  s.<|>\n}\n",
            "aaa_long* < z:*",
        ),
        (
            "member field of the expected arg type first",
            "struct Mob {\n  attack_timer: float,\n  attack_co: Coroutine<int>?,\n}\n\
             fn poll_co(arena: Arena<Mob>, e: Entity<Mob>, cor: Coroutine<int>?) {}\n\
             fn r(mob: Mob, arena: Arena<Mob>, e: Entity<Mob>) {\n  poll_co(arena, e, mob.at<|>)\n}\n",
            "attack_co* < attack_timer*",
        ),
        // Locality ranks above case: a lowercase query still puts the global
        // above the case-matching fn; case decides only between equals.
        (
            "global above fn despite a lowercase query",
            "global JIT_TRACE_LOG_ENABLED: bool = false\nfn journal_log() {}\n\
             fn r() {\n  jlog<|>\n}\n",
            "JIT_TRACE_LOG_ENABLED < journal_log",
        ),
        (
            "uppercase query picks the screaming global",
            "global JIT_TRACE_LOG_ENABLED: bool = false\nglobal journal_log_flag: bool = false\n\
             fn r() {\n  JLOG<|>\n}\n",
            "JIT_TRACE_LOG_ENABLED < journal_log_flag",
        ),
        (
            "right-type local above wrong-type local",
            "value Color { r: float }\nfn paint(c: Color) {}\n\
             fn r() {\n  let tint = Color { r: 1.0 }\n  let tin = 3\n  paint(ti<|>)\n}\n",
            "tint < tin",
        ),
        // -- slot-typed contexts: match-arm heads and `for x in` --
        (
            "arm head: scrutinee variant above a prefix-matching global",
            "enum AiState { Idle, Combat, Flee }\nglobal count: int = 0\n\
             fn tick(s: AiState) {\n  match s {\n    co<|>\n  }\n}\n",
            "AiState::Combat < count",
        ),
        // The arm-completing munge must still resolve the scrutinee.
        (
            "arm head with unclosed braces at EOF",
            "enum AiState { Idle, Combat, Flee }\nglobal count: int = 0\n\
             fn tick(s: AiState) {\n  match s {\n    co<|>",
            "AiState::Combat < count",
        ),
        (
            "arm body is a plain value slot",
            "enum AiState { Idle, Combat, Flee }\nglobal count: int = 0\n\
             fn tick(s: AiState) {\n  match s {\n    AiState::Idle => co<|>\n  }\n}\n",
            "count < AiState::Combat",
        ),
        // `apples` sorts first, so only the iterable rung can flip these.
        (
            "for-in tuple pattern: Map local first",
            "fn r() {\n  let scores: Map<string, int> = map_new()\n  let apples = 0\n  for (k, v) in <|>\n}\n",
            "scores < apples",
        ),
        (
            "for-in: FlatVec local first",
            "struct Mob { hp: int }\n\
             fn r() {\n  let scores: FlatVec<Mob> = flat_vec_new()\n  let apples = 0\n  for m in <|>\n}\n",
            "scores < apples",
        ),
        (
            "for-in: iter/next protocol local first",
            "struct Bag { n: int }\nstruct BagIter { i: int }\n\
             fn Bag.iter(b) -> BagIter { return BagIter { i: b.n } }\n\
             fn BagIter.next(it) -> (bool, int) { return (it.i <= 0, it.i) }\n\
             fn r() {\n  let scores = Bag { n: 3 }\n  let apples = 0\n  for m in <|>\n}\n",
            "scores < apples",
        ),
        (
            "for-in: iterable-returning fn above a wrong-type value",
            "struct Mob { hp: int }\nglobal MOBS: [Mob] = []\n\
             fn all_mobs() -> [Mob] { return MOBS }\nfn r(alpha: int) {\n  for m in al<|>\n}\n",
            "all_mobs < alpha",
        ),
        (
            "for-in: iterable global above int global and prefix fn",
            "struct Mob { hp: int }\nglobal MOBS: [Mob] = []\nglobal MOB_COUNT: int = 0\n\
             fn mob_spawn() {}\nfn r() {\n  for m in mo<|>\n}\n",
            "MOBS < MOB_COUNT; MOBS < mob_spawn",
        ),
        (
            "plain value slot: no iterable promotion",
            "struct Mob { hp: int }\nglobal MOBS: [Mob] = []\nfn mob_spawn() {}\n\
             fn r() {\n  let x = mo<|>\n}\n",
            "mob_spawn < MOBS",
        ),
    ]);
}

#[test]
fn contexts_lead_with() {
    check_head(&[
        (
            "Wait in an empty Coroutine generic slot",
            "struct Wait { wait_frame: bool }\nco fn f() -> Coroutine<<|>> {\n  yield 0\n}\n",
            &["Wait"],
        ),
        (
            "Coroutine snippets for the type name",
            "struct Wait { wait_frame: bool }\nco fn f() -> Cor<|> {\n  yield 0\n}\n",
            &["Coroutine<Wait>", "Coroutine<>"],
        ),
        // Hosts ship `Wait` as an enum.
        (
            "Coroutine<Wait> for an enum Wait",
            "enum Wait { Frame, Secs(float) }\nco fn f() -> Cor<|> {\n  yield 0\n}\n",
            &["Coroutine<Wait>"],
        ),
        (
            "co fn return type",
            "struct Wait { wait_frame: bool }\nco fn spawn() <|>{\n  yield 0\n}\n",
            &["-> Coroutine<Wait>"],
        ),
        (
            "Entity of the leading fn-name struct",
            "struct Mob { health: int }\nco fn mob_attack_co(e: <|>) -> Coroutine<int> {\n  yield 1\n}\n",
            &["Entity<Mob>"],
        ),
        (
            "Entity of a trailing fn-name struct",
            "struct Mob { health: int }\nco fn spawn_mob(e: <|>) -> Coroutine<int> {\n  yield 1\n}\n",
            &["Entity<Mob>"],
        ),
        (
            "get on an Entity receiver",
            "struct Mob { health: int }\nfn f(e: Entity<Mob>) {\n  e.<|>\n}\n",
            &["get"],
        ),
        (
            "better textual match beats the bool boost",
            "struct Door { ajar: float, accessible: bool }\nfn f(door: Door) {\n  if door.aj<|>\n}\n",
            &["ajar: float"],
        ),
        (
            "scrutinee variants in a fresh arm head",
            "enum AiState { Idle, Combat, Flee }\nglobal count: int = 0\n\
             fn tick(s: AiState) {\n  match s {\n    <|>\n  }\n}\n",
            &["AiState::Combat", "AiState::Flee", "AiState::Idle"],
        ),
    ]);
}

#[test]
fn right_type_local_in_top_three_of_typed_slot() {
    let l = labels(
        "value Color { r: float }\nglobal COLD_START: int = 1\nfn collect() {}\n\
         fn collide() {}\nfn colorize() {}\nfn paint(c: Color) {}\n\
         fn r() {\n  let col = Color { r: 1.0 }\n  paint(col<|>)\n}\n",
    );
    assert!(rank(&l, "col") < 3, "{l:?}");
}

#[test]
fn expected_type_values_take_the_first_type_bucket() {
    // RankKey digit 1 is the type rank in an arg slot.
    let cases = [
        (
            "enum variant",
            "enum TargetError { NotGround, OutOfRange }\nfn reject_target(e: TargetError) {}\n\
             fn r() {\n  reject_target(nog<|>)\n}\n",
            "TargetError::NotGround",
        ),
        (
            "global",
            "struct Cfg { x: int }\nglobal GAME_CFG: Cfg\nfn apply_cfg(c: Cfg) {}\n\
             fn r() {\n  apply_cfg(gam<|>)\n}\n",
            "GAME_CFG",
        ),
    ];
    for (case, src, label) in cases {
        assert_eq!(sort_digit(&complete(src), label, 1), '0', "{case}");
    }
}
