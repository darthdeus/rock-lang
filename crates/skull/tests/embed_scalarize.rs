//! Codegen-shape contract for `embed`: an embedded field has NO wrapper box.
//! Its leaves are physical keys on the parent under a DOTTED path
//! (`t["p.x"]`), so there is no nested table and no inner `__ty`.
//!
//! Dotted rather than an identifier-namespace scheme because `.` cannot appear
//! in a Rock identifier: the encoding can never collide with a user field or a
//! control key, and it is injective over paths (`a__b` is not -- `embed a: X`
//! with field `b` and a plain field `a_b` both encode to it).
//!
//! Behavioral coverage is `tests/embed_projection.rock`; this file pins the
//! LAYOUT that makes the feature real, mirroring `tuple_scalarize.rs`.

use crate::common::{emit_module, function_body};

const EMBED_LEAVES: &str = r#"
struct Pt { x: int, y: int }
struct Holder { embed p: Pt, tag: int }

fn mk(a: int, b: int) -> Holder {
    return Holder { p: Pt { x: a, y: b }, tag: 3 }
}

fn read_leaf(h: Holder) -> int { return h.p.x + h.p.y + h.tag }

fn write_leaf(h: Holder) { h.p.x = 9 }

fn main() {
    let h = mk(1, 2)
    write_leaf(h)
    let _ = read_leaf(h)
}
"#;

#[test]
fn embed_ctor_splices_dotted_keys() {
    let lua = emit_module(EMBED_LEAVES);
    let mk = function_body(&lua, "mk");
    assert!(
        mk.contains(r#"["p.x"]"#) && mk.contains(r#"["p.y"]"#),
        "embedded ctor did not write dotted parent keys in mk():\n{mk}"
    );
    assert!(
        !mk.contains(r#""Pt""#),
        "embedded child kept an inner `__ty` (its own box) in mk():\n{mk}"
    );
    assert!(
        mk.contains("tag = "),
        "non-embed sibling field lost its plain key in mk():\n{mk}"
    );
}

#[test]
fn embed_leaf_read_indexes_the_parent() {
    let lua = emit_module(EMBED_LEAVES);
    let body = function_body(&lua, "read_leaf");
    assert!(
        body.contains(r#"["p.x"]"#) && body.contains(r#"["p.y"]"#),
        "embedded leaf read did not index the parent's dotted key:\n{body}"
    );
    assert!(
        !body.contains(".p"),
        "an intermediate `.p` projection survived -- the child box is still being read:\n{body}"
    );
}

#[test]
fn embed_leaf_write_stores_into_the_parent() {
    let lua = emit_module(EMBED_LEAVES);
    let body = function_body(&lua, "write_leaf");
    assert!(
        body.contains(r#"["p.x"] = "#),
        "embedded leaf write did not store into the parent's dotted key:\n{body}"
    );
    assert!(
        !body.contains(".p"),
        "an intermediate `.p` projection survived on the write path:\n{body}"
    );
}

const EMBED_NOBOX_VALUE: &str = r#"
nobox value Pt { x: int, y: int }
struct Holder { embed p: Pt?, tag: int }

fn mk(a: int, b: int) -> Holder {
    return Holder { p: Pt { x: a, y: b }, tag: 3 }
}

fn read_leaf(h: Holder) -> int { return (h.p?.x ?? 0) + (h.p?.y ?? 0) + h.tag }
fn main() { let _ = read_leaf(mk(1, 2)) }
"#;

#[test]
fn embed_nobox_value_uses_the_owned_representation_fact() {
    let lua = emit_module(EMBED_NOBOX_VALUE);
    let body = function_body(&lua, "mk");
    assert!(body.contains(r#"["p.x"]"#) && body.contains(r#"["p.y"]"#));
    assert!(!body.contains(r#""Pt""#), "the embedded nobox value acquired a box:\n{body}");
}

// Recursive: an embed chain composes the whole path, so the deepest leaf is a
// single key on the outermost parent -- no intermediate box at any level.
const EMBED_NESTED: &str = r#"
struct Inner { c: int }
struct Mid { embed b: Inner, m: int }
struct Outer { embed a: Mid, o: int }

fn mk_outer(x: int) -> Outer {
    return Outer { a: Mid { b: Inner { c: x }, m: 2 }, o: 3 }
}

fn read_outer(t: Outer) -> int { return t.a.b.c }

fn main() { let _ = read_outer(mk_outer(1)) }
"#;

#[test]
fn nested_embed_composes_the_whole_path() {
    let lua = emit_module(EMBED_NESTED);
    let ctor = function_body(&lua, "mk_outer");
    assert!(
        ctor.contains(r#"["a.b.c"]"#),
        "nested embed did not compose a whole-path key in mk_outer():\n{ctor}"
    );
    assert!(
        ctor.contains(r#"["a.m"]"#),
        "the mid level's own field did not flatten in mk_outer():\n{ctor}"
    );
    assert!(
        !ctor.contains(r#""Mid""#) && !ctor.contains(r#""Inner""#),
        "an intermediate embed level kept its own box in mk_outer():\n{ctor}"
    );

    let read = function_body(&lua, "read_outer");
    assert!(
        read.contains(r#"["a.b.c"]"#),
        "nested leaf read did not compose the whole path:\n{read}"
    );
}

// The load-bearing one: SROA must read an embed field-store as LEAF-WISE, not
// as an escape. If the store escaped, the parent would stop scalarizing and the
// whole feature would be inert -- a flattened layout that still allocates.
const EMBED_HOT_LOOP: &str = r#"
struct Pt { x: int, y: int }
struct Holder { embed p: Pt, tag: int }

@inline fn mk(s: int) -> Holder { return Holder { p: Pt { x: s, y: s * 2 }, tag: s } }

fn hot(n: int) -> int {
    let acc = 0
    for i in 0..n {
        let h = mk(i)
        h.p.x = h.p.x + 1
        acc = acc + h.p.x + h.p.y + h.tag
    }
    return acc
}

fn main() { let _ = hot(4) }
"#;

#[test]
fn embed_parent_still_scalarizes() {
    let lua = emit_module(EMBED_HOT_LOOP);
    let hot = function_body(&lua, "hot");
    assert!(
        !hot.contains("{"),
        "the embed parent survived as a table ctor in hot() -- an embed field-store \
         is being read as an escape, which makes the flattening inert:\n{hot}"
    );
    assert!(
        !hot.contains(r#"["p."#),
        "a physical key survived in hot(): the parent scalarized but its leaves did \
         not become plain locals:\n{hot}"
    );
}

// CONTROL: a `value` parent is already contiguous cdata, so `embed` there is a
// no-op GUARANTEE -- it asserts the layout and must NOT introduce dotted keys.
const EMBED_VALUE_PARENT: &str = r#"
value V2 { x: float, y: float }
value ValueParent { embed v: V2, w: float }

fn mkv(a: float) -> ValueParent {
    return ValueParent { v: V2 { x: a, y: a }, w: 1.0 }
}

fn main() { let _ = mkv(1.0) }
"#;

#[test]
fn value_parent_embed_introduces_no_dotted_keys() {
    let lua = emit_module(EMBED_VALUE_PARENT);
    let body = function_body(&lua, "mkv");
    assert!(
        !body.contains(r#"["v."#),
        "embed on a value (cdata) parent emitted a dotted table key; it is a layout \
         assertion, not a reencoding:\n{body}"
    );
}

const NOBOX_REFLECTION_FIELD: &str = r#"
nobox value Pt { x: int, y: int }
struct Holder { p: Pt, tag: int }

fn main() {
    let _ = Holder { p: Pt { x: 1, y: 2 }, tag: 3 }
}
"#;

#[test]
fn type_table_uses_the_shared_plan_for_a_nobox_field() {
    let lua = emit_module(NOBOX_REFLECTION_FIELD);
    assert!(
        lua.contains(r#"name = "p", type = "Pt", nullable = false, embed = true"#),
        "the nobox field was not reflected from its inline plan:\n{lua}"
    );
}
