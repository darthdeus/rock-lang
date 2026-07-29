//! Emitted-shape contract for leaf-addressed `NativeBuffer<T>` elements.
//!
//! A behavioral test cannot see a surviving ctor -- `tests/native_buffer_
//! leaf_addressing.rock` asserts VALUES, and every one of its cases is already
//! green on the boxed path. What distinguishes the two representations is the
//! emitted ACCESS SHAPE, so that is what these assert: no `<name>_ct(` on the
//! buffer path, and no struct-slot reference.
//!
//! Design + measurements: docs/native-buffer-leaf-addressing-plan.md.

mod common;
use common::{emit_module, function_body};

/// A NESTED value field is the case that forces a ctor today: `gen_leaf_stores`
/// (skull-transform) writes immediate fields into the slot but does not descend,
/// so the nested field is assigned whole as `slot.pos = V2_ct(x, y)`.
const NESTED_STORE: &str = r#"
value V2 { x: float, y: float }
value Vert { pos: V2, uv: V2, id: int, w: float }

@nomangle inline(lua) fn __nb_alloc_Vert(n: int) -> Ptr<Vert> = 'return ffi.new("Vert[?]", n)'
fn nb_vert(n: int) -> NativeBuffer<Vert> { return NativeBuffer { ptr: __nb_alloc_Vert(n), len: n } }

fn fill(buf: NativeBuffer<Vert>, n: int) {
    for i in 0..n {
        let f = i as float
        buf[i] = Vert { pos: V2 { x: f, y: f }, uv: V2 { x: f, y: f }, id: i, w: 1.0 }
    }
}

fn main() {
    let buf = nb_vert(4)
    fill(buf, 4)
}
"#;

#[test]
fn nested_value_store_materializes_no_ctor() {
    let lua = emit_module(NESTED_STORE);
    let body = function_body(&lua, "fill");
    assert!(
        !body.contains("V2_ct("),
        "a nested value field must be stored leaf-wise, not as a whole cdata:\n{body}"
    );
}

#[test]
fn nested_value_store_takes_no_slot_reference() {
    // Indexing a cdata array of struct type returns a REFERENCE cdata (24 B,
    // measured). Leaf addressing goes through a typed view instead, so no slot
    // is taken at all.
    let lua = emit_module(NESTED_STORE);
    let body = function_body(&lua, "fill");
    assert!(
        !body.contains("__nb_checked_get("),
        "leaf addressing must not take a struct-slot reference:\n{body}"
    );
}

const NESTED_READ: &str = r#"
value V2 { x: float, y: float }
value Vert { pos: V2, uv: V2, id: int, w: float }

@nomangle inline(lua) fn __nb_alloc_Vert(n: int) -> Ptr<Vert> = 'return ffi.new("Vert[?]", n)'
fn nb_vert(n: int) -> NativeBuffer<Vert> { return NativeBuffer { ptr: __nb_alloc_Vert(n), len: n } }

fn total(buf: NativeBuffer<Vert>, n: int) -> float {
    let acc = 0.0
    for i in 0..n {
        acc = acc + buf[i].pos.x + buf[i].w
    }
    return acc
}

fn main() {
    let buf = nb_vert(4)
    print("{total(buf, 4)}")
}
"#;

/// S5: the debug bounds check is per ELEMENT ACCESS, not per leaf, and it must
/// PRECEDE the leaf accesses it guards.
///
/// Two separate failures are possible and only one is a count: bounds-checking
/// each of the K leaf writes (K times the cost the boxed path paid once, on the
/// hot path), or checking AFTER writing, which reads and writes out of bounds
/// before the check that was supposed to prevent it ever runs. Matched on the
/// `checked` helper call rather than a specific name, so this survives naming
/// the new helper.
#[test]
fn a_whole_element_store_bounds_checks_exactly_once_first() {
    let lua = emit_module(NESTED_STORE);
    let body = function_body(&lua, "fill");

    let checks = body.matches("checked").count();
    assert_eq!(
        checks, 1,
        "one element store must bounds-check once, not once per leaf; found {checks}:\n{body}"
    );

    // Ordering: the check has to come before the buffer is addressed through a
    // typed view (`r6 = r3.__vf`), which is where every leaf write goes.
    let check_at = body.find("checked").expect("a bounds check");
    let first_view_use = body
        .find(".__v")
        .unwrap_or_else(|| panic!("a typed-view use, so the ordering below is checked:\n{body}"));
    assert!(
        check_at < first_view_use,
        "the bounds check must PRECEDE the leaf writes it guards:\n{body}"
    );
}

#[test]
fn nested_value_read_takes_no_slot_reference() {
    // A read costs TWO reference cdatas today: the slot, then the nested field
    // (`r1 = __nb_checked_get(..)`, `r2 = r1.pos`). Both must be gone.
    let lua = emit_module(NESTED_READ);
    let body = function_body(&lua, "total");
    assert!(
        !body.contains("__nb_checked_get("),
        "a leaf read must address the view directly, not take a slot ref:\n{body}"
    );
}

/// A read of ONE field must load one leaf. `is_dead_value` treats every
/// `Inst::Index` as effectful because a Vec/NativeBuffer index errors on OOB --
/// but a leaf index is a raw pointer read whose bounds check was already hoisted
/// into `__nb_checked_index`, so the unused leaf loads are pure and dead.
/// Without the exemption an 8-leaf element pays 8 loads per single-field read,
/// in exactly the hot loops leaf addressing exists to speed up.
const SINGLE_FIELD_READ: &str = r#"
value V2 { x: float, y: float }
value Vert { pos: V2, uv: V2, id: int, w: float }

@nomangle inline(lua) fn __nb_alloc_Vert(n: int) -> Ptr<Vert> = 'return ffi.new("Vert[?]", n)'
fn nb_vert(n: int) -> NativeBuffer<Vert> { return NativeBuffer { ptr: __nb_alloc_Vert(n), len: n } }

fn read_one(buf: NativeBuffer<Vert>, i: int) -> float {
    return buf[i].pos.x
}

fn main() {
    let b = nb_vert(2)
    print("{read_one(b, 0)}")
}
"#;

#[test]
fn a_single_field_leaf_read_loads_only_that_leaf() {
    let lua = emit_module(SINGLE_FIELD_READ);
    let body = function_body(&lua, "read_one");
    // Count the view-indexed loads. `Vert` has 6 leaves; only `pos.x` is used.
    let loads = body.matches("[(").count() + body.matches("[__").count();
    let view_reads = body.matches(".__v").count();
    assert!(
        view_reads <= 1,
        "a one-field read must bind at most one typed view, found {view_reads}:\n{body}"
    );
    assert!(
        loads <= 1,
        "a one-field read must emit one leaf load, found {loads}:\n{body}"
    );
    // The PRECONDITION for dropping the other loads: the element's bounds check
    // is hoisted, so each leaf read carries no error of its own. If this ever
    // stops being emitted, the DCE exemption is deleting a real check --
    // `leaf_read_still_bounds_checks` in the .rock suite is the runtime half.
    assert!(
        body.contains("__nb_checked_index("),
        "the hoisted bounds check must survive; it is what makes the leaf loads pure:\n{body}"
    );
}
