//! Boxed-enum emission (T8e). A non-cdata enum (a recursive or GC-carrying
//! payload makes it non-cdata, so it does NOT scalarize to a value struct) boxes
//! as `{__variant, __ty, 0:.., 1:.., <field>:..}` via the nil-dropping ctor
//! (`JS_ENUM_RUNTIME`) -- native 0-based positional keys. Unit variants of a mixed
//! enum are interned singletons (`__rock_enum_<E>_<V>`); a payload-free enum keeps
//! its int repr with a `__rock_enum_<E>_names` Map for `{x}` interpolation. The
//! node-executed checks are the correctness oracle; the nil-slot eq case
//! (`nil_slot_rebox_eq`) pins the union-wide-rebox trap: a `MakeEnumDyn` rebox with
//! an inactive (nil) slot must own the SAME keys as a direct `MakeEnum`, or
//! `__op_eq`'s key-count walk calls the equal values unequal.

use skull_mir::mir::FuncRef;
use crate::common::ws_with;

fn node_available() -> bool {
    crate::common::node().arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

fn emit_closure(ws: &bone_semantic::inference::Workspace, root: &str) -> Result<String, String> {
    let mid = *ws.module_ids.get(&ustr::Ustr::from("<test>")).expect("test module");
    let mut r = None;
    for (fid, f) in ws.module(mid).functions.iter() {
        if f.name.as_str() == root {
            r = Some(FuncRef { module: mid, func: fid });
        }
    }
    skull_codegen_js::emit::emit_with_closure(ws, &[(r.unwrap(), root.to_string())])
        .map_err(|e| e.message)
}

fn preamble() -> String {
    format!(
        "{}{}{}{}{}",
        skull_codegen_js::JS_NB_CHECK_SHIMS,
        skull_codegen_js::JS_VEC_CHECK_SHIMS,
        skull_codegen_js::JS_TABLE_PRIM,
        skull_codegen_js::JS_STRUCTURAL_EQ,
        skull_codegen_js::JS_ENUM_RUNTIME,
    ) + skull_codegen_js::JS_INSPECT_RUNTIME
}

fn eval(src: &str, root: &str, tail: &str) -> String {
    if !node_available() {
        eprintln!("SKIP: node not found");
        return String::new();
    }
    let ws = ws_with(src);
    let js = emit_closure(&ws, root).unwrap_or_else(|e| panic!("emit `{root}`: {e}"));
    let program = format!("{}{}\n{js}\n{tail}", preamble(), skull_codegen_js::JS_STRING_RUNTIME);
    let out = crate::common::node().arg("-e").arg(&program).output().expect("spawn node");
    assert!(
        out.status.success(),
        "node failed:\n--- program ---\n{program}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn eval_int(src: &str, root: &str, expect: i64) {
    if !node_available() {
        return;
    }
    let got = eval(src, root, &format!("console.log({root}());"));
    let n: i64 = got.parse().unwrap_or_else(|_| panic!("not an int: {got:?}"));
    assert_eq!(n, expect, "wrong result for `{root}`");
}

// A recursive (non-cdata) enum: nested tuple-variant boxes, matched + summed.
#[test]
fn mixed_enum_tuple_variant() {
    let src = "enum Node { Nil, Leaf(int), Pair(int, Node) }\n\
               fn node_sum(n: Node) -> int {\n\
                   match n { Nil => 0, Leaf(x) => x, Pair(x, rest) => x + node_sum(rest) }\n\
               }\n\
               fn root() -> int { node_sum(Node::Pair(1, Node::Pair(2, Node::Leaf(3)))) }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("mixed enum emits");
    assert!(js.contains("__rock_make_enum("), "boxes via the nil-dropping ctor:\n{js}");
    eval_int(src, "root", 6);
}

// A unit variant of a mixed enum is its interned singleton global -- it survives
// whole as a nested payload (`Pair(9, Node::Nil)`), where scalarization at a call
// boundary would otherwise flatten it to bare tag/slot args.
#[test]
fn unit_variant_singleton() {
    let src = "enum Node { Nil, Leaf(int), Pair(int, Node) }\n\
               fn node_sum(n: Node) -> int {\n\
                   match n { Nil => 0, Leaf(x) => x, Pair(x, rest) => x + node_sum(rest) }\n\
               }\n\
               fn root() -> int { node_sum(Node::Pair(9, Node::Nil)) }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("unit singleton emits");
    assert!(
        js.contains("var __rock_enum_Node_Nil = { __variant: \"Nil\", __ty: \"Node\" };"),
        "emits the singleton def:\n{js}"
    );
    eval_int(src, "root", 9);
}

// A record variant fills the `named` arm (`<field>: v` keys on the box).
#[test]
fn record_variant() {
    let src = "enum Msg { Ping, Say(string), At { x: int, y: int } }\n\
               fn code(m: Msg) -> int { match m { Ping => 1, Say(_) => 2, At { x, y } => x + y } }\n\
               fn root() -> int { code(Msg::At { x: 10, y: 20 }) }";
    eval_int(src, "root", 30);
}

// Two distinct constructions of the same boxed variant compare EQUAL (structural),
// distinct contents UNEQUAL -- matching luajit byte-value eq.
#[test]
fn boxed_enum_eq() {
    let src = "enum Node { Nil, Leaf(int), Pair(int, Node) }\n\
               fn root() -> int {\n\
                   let a = Node::Leaf(5)\n\
                   let b = Node::Leaf(5)\n\
                   let c = Node::Leaf(6)\n\
                   if a == b && a != c { 1 } else { 0 }\n\
               }";
    eval_int(src, "root", 1);
}

// The nil-slot trap: `Sig` has a union-wide inactive second tuple slot when
// flattened into a struct field, while a direct `Sig::Num(5)` does not.
#[test]
fn nil_slot_rebox_eq() {
    let src = "enum Sig { Idle, Num(int), Pair(int, int), Act(() -> ()) }\n\
               struct Holder { sig: Sig }\n\
               fn root() -> int {\n\
                   let h = Holder { sig: Sig::Num(5) }\n\
                   if h.sig == Sig::Num(5) { 1 } else { 0 }\n\
               }";
    eval_int(src, "root", 1);
}

// `inspect` of a boxed enum byte-matches luajit: positional payloads render with
// luajit's 1-based `[N]` display over the native 0-based runtime keys; record
// variants render `field = v`; a nested singleton renders `E::V`.
#[test]
fn inspect_byte_matches_luajit() {
    let src = "enum Node { Nil, Leaf(int), Pair(int, Node) }\n\
               enum Msg { Ping, Say(string), At { x: int, y: int } }\n\
               fn leaf() -> string { inspect(Node::Leaf(5)) }\n\
               fn pair() -> string { inspect(Node::Pair(1, Node::Nil)) }\n\
               fn rec() -> string { inspect(Msg::At { x: 1, y: 2 }) }\n\
               fn unit() -> string { inspect(Msg::Ping) }";
    assert_eq!(eval(src, "leaf", "console.log(leaf());"), "Node::Leaf { [1] = 5 }");
    assert_eq!(eval(src, "pair", "console.log(pair());"), "Node::Pair { [1] = 1, [2] = Node::Nil }");
    assert_eq!(eval(src, "rec", "console.log(rec());"), "Msg::At { x = 1, y = 2 }");
    assert_eq!(eval(src, "unit", "console.log(unit());"), "Msg::Ping");
}

// A flat enum widens a tuple payload's `nobox value` leaves into its positional
// slots. Inspect restores those names when the active payload layout is exact.
// This enum has no unit variant, so the metadata must not depend on a singleton
// global being emitted for it.
#[test]
fn inspect_flat_enum_nobox_record_payload_names() {
    let src = "nobox value Pos { x: int, y: int }\n\
               enum Event { At(Pos), Text(string) }\n\
               struct Holder { event: Event }\n\
               fn root() -> string { inspect(Holder { event: Event::At(Pos { x: 4, y: 9 }) }.event) }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("flat enum inspection emits");
    assert_eq!(
        eval(src, "root", "console.log(root());"),
        "Event::At { x = 4, y = 9 }"
    );
    assert!(
        js.contains("__rock_enum_inspect_payloads[\"Event\"]")
            && !js.contains("__rock_enum_Event_"),
        "emits per-enum inspect metadata independently of enum singleton globals:\n{js}"
    );
}

// The same leaf names appear twice, so no positional slot can be labeled
// unambiguously. Lua inspect falls back for the entire payload walk.
#[test]
fn inspect_flat_enum_duplicate_payload_names_fall_back_to_positions() {
    let src = "nobox value Pos { x: int, y: int }\n\
               enum Event { Both(Pos, Pos), Text(string) }\n\
               struct Holder { event: Event }\n\
               fn root() -> string { inspect(Holder { event: Event::Both(Pos { x: 1, y: 2 }, Pos { x: 3, y: 4 }) }.event) }";
    assert_eq!(
        eval(src, "root", "console.log(root());"),
        "Event::Both {\n  [1] = 1,\n  [2] = 2,\n  [3] = 3,\n  [4] = 4\n}"
    );
}

// A nested nominal payload does not have a flat record-name walk. Keep the
// positional fallback even though the outer value is itself a `nobox` record.
#[test]
fn inspect_flat_enum_nested_nominal_payload_falls_back_to_positions() {
    let src = "nobox value Inner { x: int, y: int }\n\
               nobox value Pos { p: Inner, z: int }\n\
               enum Event { At(Pos), Text(string) }\n\
               struct Holder { event: Event }\n\
               fn root() -> string { inspect(Holder { event: Event::At(Pos { p: Inner { x: 1, y: 2 }, z: 3 }) }.event) }";
    assert_eq!(
        eval(src, "root", "console.log(root());"),
        "Event::At {\n  [1] = 1,\n  [2] = 2,\n  [3] = 3\n}"
    );
}

// The declared leaf type still controls the Lua fallback: a direct enum field
// must not be renamed merely because its flat enum slot path has no dot.
#[test]
fn inspect_flat_enum_nominal_field_falls_back_to_positions() {
    let src = "enum Mode { A, B }\n\
               nobox value Pos { mode: Mode, y: int }\n\
               enum Event { At(Pos), Text(string) }\n\
               struct Holder { event: Event }\n\
               fn root() -> string { inspect(Holder { event: Event::At(Pos { mode: Mode::A, y: 9 }) }.event) }";
    assert_eq!(
        eval(src, "root", "console.log(root());"),
        "Event::At { [1] = 0, [2] = 9 }"
    );
}

// A stray slot, a colliding named key, or an inactive trailing descriptor makes
// the active walk ambiguous; keep all labels positional rather than partially
// renaming it.
#[test]
fn inspect_flat_enum_payload_names_require_exact_leaf_count() {
    let src = "nobox value Pos { x: int, y: int }\n\
               enum Event { At(Pos), Maybe(Pos, int?), Named { x: int }, Text(string) }\n\
               fn root() -> int { 0 }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("flat enum metadata emits");
    let program = format!(
        "{}{}\n{js}\nconsole.log(inspect({{__ty: \"Event\", __variant: \"At\", 0: 1, 1: 2, 2: 3}}));\n\
         console.log(inspect({{__ty: \"Event\", __variant: \"Maybe\", 0: 1, 1: 2}}));\n\
         console.log(inspect({{__ty: \"Event\", __variant: \"At\", 0: 1, 1: 2, x: 99}}));",
        preamble(),
        skull_codegen_js::JS_STRING_RUNTIME,
    );
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
    let out = crate::common::node().arg("-e").arg(&program).output().expect("spawn node");
    assert!(
        out.status.success(),
        "node failed:\n--- program ---\n{program}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "Event::At {\n  [1] = 1,\n  [2] = 2,\n  [3] = 3\n}\nEvent::Maybe { [1] = 1, [2] = 2 }\nEvent::At {\n  [1] = 1,\n  [2] = 2,\n  x = 99\n}"
    );
}

// A payload-free enum keeps its int repr; `{x}` interpolation reads the
// `__rock_enum_<E>_names` Map by ordinal, yielding the `<E>::<V>` display string.
#[test]
fn payload_free_names_table() {
    let src = "enum Color { Red, Green, Blue }\n\
               fn root() -> string { let g = Color::Green\n \"{g}\" }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("payload-free enum emits");
    assert!(
        js.contains("var __rock_enum_Color_names = new Map([[0, \"Color::Red\"], [1, \"Color::Green\"], [2, \"Color::Blue\"]]);"),
        "emits the names Map:\n{js}"
    );
    assert_eq!(eval(src, "root", "console.log(root());"), "Color::Green");
}
