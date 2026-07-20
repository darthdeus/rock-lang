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

use std::process::Command;

use skull_mir::lower::ScalarCtx;
use skull_mir::mir::FuncRef;

mod common;
use common::ws_with;

fn node_available() -> bool {
    Command::new("node").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

fn emit_closure(ws: &bone_semantic::inference::Workspace, root: &str) -> Result<String, String> {
    let scx = ScalarCtx::new(ws);
    let mid = *ws.module_ids.get(&ustr::Ustr::from("<test>")).expect("test module");
    let mut r = None;
    for (fid, f) in ws.module(mid).functions.iter() {
        if f.name.as_str() == root {
            r = Some(FuncRef { module: mid, func: fid });
        }
    }
    skull_codegen_js::emit::emit_with_closure(ws, &scx, &[(r.unwrap(), root.to_string())])
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
    let out = Command::new("node").arg("-e").arg(&program).output().expect("spawn node");
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

// The nil-slot trap: `Sig` (closure payload -> non-cdata) flattened into a struct
// field, read whole (`h.sig`), reboxes union-wide via `MakeEnumDyn` -- its inactive
// second tuple slot is nil. A directly-constructed `Sig::Num(5)` has only slot 0.
// The two must compare `==`; without nil-slot dropping the reboxed box owns an
// extra undefined-valued key and `__op_eq`'s key-count walk returns false.
#[test]
fn nil_slot_rebox_eq() {
    // The direct `Sig::Num(5)` is INLINE (a `let` binding would flatten to the same
    // union-wide rep, hiding the asymmetry): it stays a narrow `{0:5}` box while the
    // flattened-field read reboxes to `{0:5, 1:nil}`.
    let src = "enum Sig { Idle, Num(int), Pair(int, int), Act(() -> ()) }\n\
               struct Holder { sig: Sig }\n\
               fn root() -> int {\n\
                   let h = Holder { sig: Sig::Num(5) }\n\
                   if h.sig == Sig::Num(5) { 1 } else { 0 }\n\
               }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("nil-slot rebox emits");
    assert!(js.contains("__rock_make_enum("), "reboxes via the ctor:\n{js}");
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
