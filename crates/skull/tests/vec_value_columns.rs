//! Lowering-shape oracle for flat `Vec<T>` element storage, T an all-scalar
//! `value` struct (`docs/vec-value-columns-plan.md`). A `[V2]` stores its
//! elements FLAT -- one ordinary Lua array per leaf -- so no per-element table
//! exists at any point: no `V2_ct(..)` on push, no whole-element
//! `__vec_checked_get` on read.
//!
//! Every assertion is on EMITTED SHAPE. A runtime-value assertion cannot
//! distinguish "flat" from "boxed and unboxed again", which is the vacuity
//! class that kept the `embed` tests green while the headline property was
//! false. Behavioral coverage belongs beside this in a `.rock` test.
//!
//! Layout under test (K <= 4 leaves, the only tier v1 builds):
//!     { <leaf0> = {..}, <leaf1> = {..}, len = n }
//! so `V2 { x, y }` is `{ x = {..}, y = {..}, len = n }`, columns 1-indexed to
//! match today's `_data`.

mod common;
use common::{emit_module, function_body};

const VEC_VALUE_LOOP: &str = r#"
value V2 { x: float, y: float }

fn fill(n: int) -> float {
    let xs: [V2] = []
    for i in 0..n {
        xs.push(V2 { x: 1.0, y: 2.0 })
    }
    let acc = 0.0
    for i in 0..xs.len {
        acc = acc + xs[i].x
    }
    return acc
}

fn main() { let _ = fill(8) }
"#;

/// The headline property: nothing in the push/read loop constructs an element.
/// This is the single assertion the whole feature exists to make true.
#[test]
fn vec_of_value_struct_never_boxes_an_element() {
    let lua = emit_module(VEC_VALUE_LOOP);
    let body = function_body(&lua, "fill");
    assert!(
        !body.contains("V2_ct("),
        "[V2] push boxed the element (`V2_ct(..)`); flat storage means the \
         element table never exists:\n{body}"
    );
}

/// `push` today splices the `vec_push` inline(lua) body, which writes a whole
/// element into one backing (`vec._data[vec.len] = item`). Under columns it must
/// write each leaf into its own column instead.
#[test]
fn push_writes_one_slot_per_leaf() {
    let lua = emit_module(VEC_VALUE_LOOP);
    let body = function_body(&lua, "fill");
    assert!(
        !body.contains("_data"),
        "[V2] push still wrote the single `_data` backing; columns replace it:\n{body}"
    );
    // The literal builds one array per leaf. Regalloc splits the store itself
    // into `rN = vec.x` / `rN[i] = ..`, so assert the CONSTRUCTION shape (which
    // is stable) rather than a fused `vec.x[i]` text that never appears.
    assert!(
        body.contains(r#"["x"] = {}"#) && body.contains(r#"["y"] = {}"#),
        "[V2] literal did not build per-leaf column arrays:\n{body}"
    );
    assert!(
        body.contains(".x") && body.contains(".y"),
        "[V2] push did not reach the leaf columns:\n{body}"
    );
}

/// A projecting read must touch exactly the column it needs. Today this is
/// `__vec_checked_get(xs, i).x` -- fetch the whole element table, then project.
#[test]
fn projecting_read_indexes_one_column() {
    let lua = emit_module(VEC_VALUE_LOOP);
    let body = function_body(&lua, "fill");
    assert!(
        !body.contains("__vec_checked_get("),
        "`xs[i].x` still fetched the whole element; a column read indexes the \
         `x` column directly:\n{body}"
    );
}

const VEC_ELEM_BIND: &str = r#"
value V2 { x: float, y: float }

fn sum(xs: [V2]) -> float {
    let acc = 0.0
    for i in 0..xs.len {
        let v = xs[i]
        acc = acc + v.x + v.y
    }
    return acc
}

fn main() { let _: [V2] = []; let _ = sum([]) }
"#;

/// Binding a whole element (`let v = xs[i]`) is legal and must stay flat: each
/// leaf loads from its column and SROA peels the binding.
///
/// NOTE the assertion is on the FETCH, not on `V2_ct(`. A no-ctor assertion is
/// vacuously green today -- `__vec_checked_get` hands back the element table
/// that storage already boxed, so no *new* ctor is needed and the test would
/// pass while the property is false. Asserting the whole-element fetch is gone
/// is what actually distinguishes flat from boxed.
#[test]
fn binding_a_whole_element_loads_leaves_not_an_element() {
    let lua = emit_module(VEC_ELEM_BIND);
    let body = function_body(&lua, "sum");
    assert!(
        !body.contains("__vec_checked_get("),
        "`let v = xs[i]` fetched a whole element table; under columns the bind \
         loads one slot per leaf:\n{body}"
    );
    assert!(
        !body.contains("V2_ct("),
        "`let v = xs[i]` rematerialized the element:\n{body}"
    );
}

// --- separately-emitted callable bodies ------------------------------------
//
// A lambda body / nested fn is its own emitted callable, and the inliner scans
// only TOP-LEVEL CFG blocks (a cell used in a lambda body is an upvalue, so
// splicing there piles onto LuaJIT's 60-upvalue cap). So a `for` over a
// column-stored Vec inside one reaches the SHARED generic `VecIter<T>.next`,
// which reads the `_data` that column storage does not have.
//
// These are RED and stay red until specialization works in every emitted
// callable context. The ordinary-call guard (`in_unspliced_ctx` -> keep the
// shared boxed ABI, `lower.rs:3965`) is NOT the fix here: for a boxed value the
// shared body is CORRECT and the guard merely forgoes an optimization, whereas
// for columns the shared body is WRONG -- taking that branch would turn a loud
// rejection into a miscompile.

const VEC_IN_LAMBDA: &str = r#"
value V2 { x: float, y: float }

fn go() -> float {
    let xs: [V2] = [V2 { x: 1.0, y: 2.0 }]
    let f = () => { let a = 0.0; for v in xs { a = a + v.x }; return a }
    return f()
}

fn main() { let _ = go() }
"#;

const VEC_IN_NESTED_FN: &str = r#"
value V2 { x: float, y: float }

fn go() -> float {
    let xs: [V2] = [V2 { x: 1.0, y: 0.0 }]
    fn inner(ys: [V2]) -> float { let a = 0.0; for v in ys { a = a + v.x }; return a }
    return inner(xs)
}

fn main() { let _ = go() }
"#;

/// Iterating a column-stored Vec inside a LAMBDA. Today this is a hard emit
/// rejection ("a value-specialized generic call was not spliced") -- the
/// residual-specialization validator catching it, which is the safety net
/// working. Real engine code hits it (`draw_glow_lights`, a `for` over
/// `GLOW_LIGHTS` inside a `with_blend_mode(..., () => ..)` callback).
#[test]
fn column_vec_iterated_inside_a_lambda() {
    let lua = common::try_emit_module(VEC_IN_LAMBDA)
        .expect("column [V2] iterated in a lambda must emit");
    assert!(
        !lua.contains("_data"),
        "lambda body iterated a column-stored [V2] through the shared generic \
         `next`, which reads `_data`:\n{lua}"
    );
}

/// The same shape in a NESTED FN is worse than the lambda case: it emits
/// silently. The nested fn becomes its own top-level function whose loop calls
/// the UNSPECIALIZED `next_VecIter_T_..`, and the residual-specialization
/// validator does not fire -- so there is no compile-time signal at all and it
/// dies at runtime on `_data` being nil.
#[test]
fn column_vec_iterated_inside_a_nested_fn() {
    let lua = common::try_emit_module(VEC_IN_NESTED_FN)
        .expect("column [V2] iterated in a nested fn must emit");
    assert!(
        !lua.contains("next_VecIter_T"),
        "nested fn called the UNSPECIALIZED VecIter next over column storage \
         (silent miscompile, no validator error):\n{lua}"
    );
}

const VEC_CONTROLS: &str = r#"
value WithRef { name: string, k: int }
struct Plain { a: int }

fn ints() -> int {
    let xs: [int] = []
    xs.push(3)
    return xs[0]
}

fn refs() -> int {
    let xs: [WithRef] = []
    xs.push(WithRef { name: "a", k: 1 })
    return xs[0].k
}

fn plains() -> int {
    let xs: [Plain] = []
    xs.push(Plain { a: 1 })
    return xs[0].a
}

fn main() { let _ = ints() + refs() + plains() }
"#;

/// Controls: the representation swap is NARROW. A primitive element is already
/// one flat array, a value struct with a REFERENCE leaf is out of scope, and a
/// non-value struct keeps identity. All three keep `_data`.
#[test]
fn ineligible_element_types_keep_todays_representation() {
    let lua = emit_module(VEC_CONTROLS);
    for f in ["ints", "refs", "plains"] {
        let body = function_body(&lua, f);
        assert!(
            body.contains("_data"),
            "{f}: ineligible element type lost the `_data` backing -- the \
             column swap must not reach it:\n{body}"
        );
    }
}
