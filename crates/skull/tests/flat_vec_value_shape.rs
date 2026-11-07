//! FlatVec<T> lowering-shape oracle. The stdlib FlatVec
//! ops are `@inline` and specialize per element type at their call sites, so a
//! push/read loop over `FlatVec<value>` must scalarize field-wise: NO per-push
//! `<T>_ct(..)` box and NO boxed `__nb_checked_set` whole-value store. The
//! bounds-checked slot accessor `__nb_checked_get` stays (it fetches the flat slot
//! that a scalarized store then writes in place). Runtime correctness is
//! `tests/values/flat_vec_value.rock`; this pins the emitted shape.
use crate::common::{emit_module, function_body};

const FLAT_VEC_LOOP: &str = r#"
value V2 { x: float, y: float }

fn fill(n: int) -> float {
    let fv: FlatVec<V2> = flat_vec_new(4)
    for i in 0..n {
        fv.push(V2 { x: 1.0, y: 2.0 })
    }
    let acc = 0.0
    for i in 0..fv.len {
        acc = acc + fv.get(i).x
    }
    return acc
}

fn main() { let _ = fill(8) }
"#;

#[test]
fn flat_vec_push_read_loop_is_zero_box() {
    let lua = emit_module(FLAT_VEC_LOOP);
    let body = function_body(&lua, "fill");

    // The value struct never boxes anywhere in the loop -- the element threads as
    // scalar leaves into the flat slot (the P3 zero-`_ct` oracle). (A growth copy
    // `grown[i] = old[i]` legitimately uses `__nb_checked_set` with a cdata REF,
    // not a box, so it is `_ct`-free too.)
    assert!(
        !body.contains("V2_ct("),
        "FlatVec push must not box the element (`V2_ct(..)`); got:\n{body}"
    );
    // The push writes the element's LEAVES through typed views -- no slot
    // reference is fetched at all, so there is no `slot.x = ..` to look for.
    // The bounds check moves to the index (`__nb_checked_index`).
    assert!(
        !body.contains("__nb_checked_get("),
        "FlatVec push should take no slot reference; got:\n{body}"
    );
    assert!(
        body.contains("__nb_checked_index("),
        "FlatVec push must still bounds-check the element access; got:\n{body}"
    );
    // The ffi array is the concrete element ctype (the specialized alloc intrinsic).
    assert!(
        body.contains(r#"ffi.new("V2[?]","#),
        "FlatVec storage must be a `ffi.new(\"V2[?]\", ..)` array; got:\n{body}"
    );
    // No dangling call to a shared generic op survives the specialized splices.
    assert!(
        !body.contains("flat_vec_new") && !body.contains(" push("),
        "FlatVec ops must be spliced, not left as calls; got:\n{body}"
    );
}
