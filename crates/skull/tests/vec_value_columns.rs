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
//!     { ["@vec"] = { repr, elem, cap }, <leaf0> = {..}, .., len = n }
//! Columns are 1-indexed to match today's `_data`.

use crate::common::{emit_module, function_body};

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

const VEC_SPREAD_LITERAL: &str = r#"
value V2 { x: float, y: float }

fn make() -> [V2] {
    return [...(
        V2 { x: 1.0, y: 2.0 },
        V2 { x: 3.0, y: 4.0 },
    )]
}

fn main() { let _ = make() }
"#;

#[test]
fn tuple_spread_literal_never_boxes_column_elements() {
    let lua = emit_module(VEC_SPREAD_LITERAL);
    let body = function_body(&lua, "make");
    assert!(
        !body.contains("V2_ct("),
        "tuple spread built temporary V2 elements before storing their leaves:\n{body}"
    );
    assert!(
        body.contains(r#"["@vec"] = {repr = "columns", elem = "V2""#),
        "tuple spread did not produce the marked column representation:\n{body}"
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
// A lambda body, a nested `fn`, a `co fn`'s state machine and a global
// initializer are each their own emitted callable, and the inliner scans only
// TOP-LEVEL CFG blocks (a cell used in a lambda body is an upvalue, so splicing
// there piles onto LuaJIT's 60-upvalue cap). So a `for` over a column-stored Vec
// inside one reaches the SHARED generic `VecIter<T>.next`, which reads the
// `_data` that column storage does not have.
//
// All four now work, by two different routes. A lambda and a nested fn are
// SPLICED: `convert_closures` splits them into standalone functions with their
// own top-level CFG, at which point the upvalue reason expires and the inliner
// runs over them. A `co fn`'s `__step` and a global initializer never run an
// inliner at all, so they CALL a standalone specialized copy of the callee
// (`next_..._spec_a_n2_V2_0_z`) emitted once for the program.
//
// The ordinary-call guard (`in_unspliced_ctx` -> keep the shared boxed ABI) is
// still not the fix: for a boxed value the shared body is CORRECT and the guard
// merely forgoes an optimization, whereas for columns the shared body is WRONG.
// `unspliceable_iteration_of_boxed_elements_still_emits` guards that from the
// other side -- specialization is forced for column-eligible elements ONLY, so
// an ABI-only instantiation still rides the shared body.

const VEC_IN_LAMBDA: &str = r#"
value V2 { x: float, y: float }

fn go() -> float {
    let xs: [V2] = [V2 { x: 1.0, y: 2.0 }, V2 { x: 3.0, y: 4.0 }]
    let f = () => { let a = 0.0; for v in xs { a = a + v.x }; return a }
    return f()
}

fn main() { print("{go()}") }
"#;

const VEC_IN_NESTED_FN: &str = r#"
value V2 { x: float, y: float }

fn go() -> float {
    let xs: [V2] = [V2 { x: 1.0, y: 0.0 }, V2 { x: 3.0, y: 0.0 }]
    fn inner(ys: [V2]) -> float { let a = 0.0; for v in ys { a = a + v.x }; return a }
    return inner(xs)
}

fn main() { print("{go()}") }
"#;

/// Iterating a column-stored Vec inside a LAMBDA. Real engine code hits it
/// (`draw_glow_lights`, a `for` over `GLOW_LIGHTS` inside a
/// `with_blend_mode(..., () => ..)` callback).
///
/// Scoped to the LAMBDA's own body: the stdlib always emits `_data`, so a
/// module-wide assertion could never go green even after a correct fix.
#[test]
fn column_vec_iterated_inside_a_lambda() {
    let lua = crate::common::try_emit_module(VEC_IN_LAMBDA)
        .expect("column [V2] iterated in a lambda must emit");
    let body = crate::common::body_of_synthetic(&lua, "__lambda");
    assert!(
        !body.contains("_data"),
        "lambda body read the single `_data` backing of a column-stored [V2]:\n{body}"
    );
    // Columns were actually CHOSEN here: the column path bounds-checks once with
    // `__vec_checked_index` then reads each leaf array raw, where the boxed path
    // fetches the whole element with `__vec_checked_get`. Without this the test
    // survives column storage being switched off entirely -- the `_data` absence
    // above is equally true of a boxed element read that lives in the stdlib.
    assert!(
        body.contains("__vec_checked_index") && !body.contains("__vec_checked_get"),
        "lambda body must iterate COLUMN storage (one index check + per-leaf reads):\n{body}"
    );
    assert_eq!(
        crate::common::run_luajit(&format!("{lua}\nmain()\n"), "col_lambda"),
        "4",
        "lambda over column storage computed the wrong sum"
    );
}

/// The same shape in a NESTED FN, which becomes its own top-level function whose
/// loop calls the UNSPECIALIZED `next_VecIter_T_..`.
///
/// Scoped to the nested fn's own body: the SHARED generic `next` stays emitted
/// module-wide even when this call site correctly splices a specialized one.
#[test]
fn column_vec_iterated_inside_a_nested_fn() {
    let lua = crate::common::try_emit_module(VEC_IN_NESTED_FN)
        .expect("column [V2] iterated in a nested fn must emit");
    let body = crate::common::body_of_synthetic(&lua, "_inner_");
    assert!(
        !body.contains("next_VecIter_T_ret_tuple2_bool_T("),
        "nested fn called the UNSPECIALIZED VecIter `next` over column storage \
         (the specialized copy is `..._spec_a_n2_V2_0_z`):\n{body}"
    );
    // As in the lambda case: pin that columns were CHOSEN, not merely that the
    // unspecialized `next` is absent -- the loop inlines to a FORL with no `next`
    // call at all, so the negative above holds under boxed storage too.
    assert!(
        body.contains("__vec_checked_index") && !body.contains("__vec_checked_get"),
        "nested fn must iterate COLUMN storage (one index check + per-leaf reads):\n{body}"
    );
    assert_eq!(
        crate::common::run_luajit(&format!("{lua}\nmain()\n"), "col_nested"),
        "4",
        "nested fn over column storage computed the wrong sum"
    );
}

const VEC_IN_CO_FN: &str = r#"
value V2 { x: float, y: float }

co fn walk(xs: [V2]) -> Coroutine<float> {
    for v in xs {
        yield v.x
    }
}

fn go() -> float {
    let xs: [V2] = [V2 { x: 1.0, y: 0.0 }, V2 { x: 3.0, y: 0.0 }]
    let c = walk(xs)
    let a = c.resume() ?? 0.0
    let b = c.resume() ?? 0.0
    return a + b
}

fn main() { print("{go()}") }
"#;

/// The same shape in a `co fn` -- the state-machine `__step` is its own emitted
/// top-level function, so it is the nested-fn case again.
///
/// A coroutine is why this context must reject at COMPILE time and cannot be
/// left to fail loudly at runtime: `__resume` wraps `__step` in a `pcall` and, on
/// error, parks the error in `s.__error`, sets `__state = -2` and returns nil. An
/// unspecialized `next` therefore raises `attempt to index field '_data' (a nil
/// value)` that never reaches stderr -- the caller reads an exhausted coroutine
/// and computes a wrong answer (`go()` yielded 0, not 4).
///
/// Scoped to `__step`'s own body for the same reason as the nested-fn case.
#[test]
fn column_vec_iterated_inside_a_co_fn() {
    let lua = crate::common::try_emit_module(VEC_IN_CO_FN)
        .expect("column [V2] iterated in a co fn must emit");
    let body = crate::common::body_of_synthetic(&lua, "__step");
    assert!(
        !body.contains("next_VecIter_T_ret_tuple2_bool_T("),
        "co fn `__step` called the UNSPECIALIZED VecIter `next` over column \
         storage (the raised error is swallowed by `__resume`'s pcall):\n{body}"
    );
    assert_eq!(
        crate::common::run_luajit(&format!("{lua}\nmain()\n"), "col_cofn"),
        "4",
        "co fn over column storage computed the wrong sum"
    );
}

const VEC_IN_GLOBAL_INIT: &str = r#"
value V2 { x: float, y: float }

global TOTAL: float = {
    let xs: [V2] = [V2 { x: 1.0, y: 0.0 }, V2 { x: 3.0, y: 0.0 }]
    let a = 0.0
    for v in xs { a = a + v.x }
    a
}

fn main() { print("{TOTAL}") }
"#;

/// A global initializer is the fourth separately-emitted body: `__skull_ginit_*`
/// is lowered and emitted by its own unit, which the inliner never enters.
#[test]
fn column_vec_iterated_in_a_global_initializer() {
    let lua = crate::common::try_emit_module(VEC_IN_GLOBAL_INIT)
        .expect("column [V2] iterated in a global initializer must emit");
    let body = crate::common::body_of_synthetic(&lua, "ginit_TOTAL");
    assert!(
        !body.contains("next_VecIter_T_ret_tuple2_bool_T("),
        "global initializer called the UNSPECIALIZED VecIter `next` over column \
         storage:\n{body}"
    );
    assert_eq!(crate::common::run_luajit(&format!("{lua}\nmain()\n"), "col_ginit"), "4");
}

const VEC_STDLIB_SURFACE: &str = r#"
value V2 { x: float, y: float }

fn main() {
    let xs: [V2] = [V2 { x: 1.0, y: 10.0 }, V2 { x: 2.0, y: 20.0 }, V2 { x: 3.0, y: 30.0 }]
    xs.set(0, V2 { x: 9.0, y: 90.0 })
    print("set:{xs[0].x},{xs[0].y}")
    let g = xs.get(1)
    if g != null { print("get:{g.x},{g.y}") }
    let p = xs.pop()
    if p != null { print("pop:{p.x} len{xs.len}") }
    let r = xs.swap_remove(0)
    if r != null { print("swap:{r.x} now{xs[0].x} len{xs.len}") }
    xs.clear()
    print("clear len{xs.len}")
    xs.push(V2 { x: 7.0, y: 70.0 })
    print("repush:{xs[0].x},{xs[0].y} len{xs.len}")
}
"#;

/// The stdlib Vec surface over column storage, end to end. These are PLAIN
/// generics -- no `@inline`, no `@pure` -- so nothing used to specialize them and
/// every one reached the shared `_data` body: `xs.set(..)` failed at top level,
/// and `Vec.clear` is what stopped bitgun running.
///
/// They work now because a column-eligible instantiation forces specialization
/// whether or not the callee opted in, and because the specialized copy is
/// emitted standalone rather than needing a splice. Inside it,
/// `__vec_data_get/_set/_unset` see the concrete element through the `SpecSubst`
/// and lower to column operations.
///
/// Asserted on VALUES, not shape. `swap_remove` moving the last element into the
/// hole and `pop` returning the right one are exactly the properties a
/// per-column decomposition can get subtly wrong while still emitting
/// column-shaped code.
#[test]
fn stdlib_vec_surface_works_over_column_storage() {
    let lua = crate::common::try_emit_module(VEC_STDLIB_SURFACE).expect("stdlib Vec surface must emit");
    // The values below are correct under BOXED storage too, so pin the mechanism
    // this test is named for: each plain-generic entry point reaches a standalone
    // The encoded `a_n2_V2_0_z` key is the parameterless nominal `V2`.
    for callee in [
        "vec_set_arr_T_int_T_ret_unit_spec_a_n2_V2_0_z(",
        "pop_arr_T_ret_T_opt_spec_a_n2_V2_0_z(",
        "clear_arr_T_ret_unit_spec_a_n2_V2_0_z(",
    ] {
        assert!(
            lua.contains(callee),
            "the stdlib Vec surface must call the specialized `{callee}` over column storage"
        );
    }
    assert_eq!(
        crate::common::run_luajit(&format!("{lua}\nmain()\n"), "col_surface"),
        "set:9,90\nget:2,20\npop:3 len2\nswap:9 now2 len1\nclear len0\nrepush:7,70 len1"
    );
}

const VEC_BOXED_ELEMS_UNSPLICED: &str = r#"
value WithRef { name: string, k: int }
value Wide { a: int, b: int, c: int, d: int, e: int }

co fn walk_ref(xs: [WithRef]) -> Coroutine<int> {
    for v in xs { yield v.k }
}

fn nested_wide(xs: [Wide]) -> int {
    fn inner(ys: [Wide]) -> int { let a = 0; for v in ys { a = a + v.a }; return a }
    return inner(xs)
}

fn main() {
    let c = walk_ref([WithRef { name: "a", k: 1 }])
    let one = c.resume() ?? 0
    print("{one + nested_wide([Wide { a: 3, b: 0, c: 0, d: 0, e: 0 }])}")
}
"#;

/// The other side of the rejection: it must fire ONLY where the representation
/// differs. `WithRef` has a reference leaf and `Wide` exceeds
/// `MAX_COLUMN_LEAVES`, so both keep the boxed `_data` -- for them the shared
/// generic `next` is CORRECT and an unspliceable context simply forgoes the
/// scalar ABI, exactly as an ordinary call does. Rejecting here would bounce
/// working code, which is what the first cut of the validator did.
#[test]
fn unspliceable_iteration_of_boxed_elements_still_emits() {
    let lua = crate::common::try_emit_module(VEC_BOXED_ELEMS_UNSPLICED)
        .expect("boxed element types must still emit in unspliceable contexts");
    assert_eq!(crate::common::run_luajit(&format!("{lua}\nmain()\n"), "boxed_unspliced"), "4");
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

const VEC_NEW_COLUMN: &str = r#"
value V2 { x: float, y: float }

fn main() {
    let a: [V2] = vec_new()
    a.push(V2 { x: 1.0, y: 2.0 })
    print("{a.len},{a[0].x},{a[0].y}")
}
"#;

/// `vec_new()` is the fourth representation-boundary primitive and the odd one
/// out: its element type comes from the CALL's own type, not from a receiver, so
/// the container plan cannot read it off an argument. The concrete boundary
/// plan must therefore construct the columns before the next column write.
#[test]
fn vec_new_at_a_column_element_builds_empty_columns() {
    let lua = crate::common::try_emit_module(VEC_NEW_COLUMN).expect("vec_new at [V2] must emit");
    let body = crate::common::function_body(&lua, "main");
    assert!(
        !body.contains("vec_new()"),
        "vec_new at a column element must not call the boxed stdlib body:\n{body}"
    );
    assert!(
        body.contains("[\"x\"] = {}") && body.contains("[\"y\"] = {}"),
        "vec_new at [V2] must build one empty array per leaf:\n{body}"
    );
    assert_eq!(
        crate::common::run_luajit(&format!("{lua}\nmain()\n"), "col_vecnew"),
        "1,1,2",
        "push onto a vec_new-ed column Vec must land in the columns"
    );
}

const VEC_COMBINATOR_CHAIN: &str = r#"
value V2 { x: float, y: float }

fn main() {
    let a: [V2] = [V2 { x: 1.0, y: 2.0 }, V2 { x: 3.0, y: 4.0 }]
    print("{a.map(_.x).sum_f()}")
}
"#;

#[test]
fn combinator_chain_preserves_column_element_type() {
    let lua = crate::common::try_emit_module(VEC_COMBINATOR_CHAIN).expect("chain must emit");
    let body = crate::common::function_body(&lua, "main");
    assert!(
        !body.contains("__vec_checked_get(") && body.contains("__vec_checked_index("),
        "the wrapped VecIter<V2> must retain V2 and read its columns:\n{body}"
    );
    assert_eq!(
        crate::common::run_luajit(&format!("{lua}\nmain()\n"), "col_combinator"),
        "4"
    );
}

const NOBOX_COLUMN_SORT: &str = r#"
nobox value NP { x: float, y: float }

fn main() {
    let a: [NP] = [NP { x: 3.0, y: 30.0 }, NP { x: 1.0, y: 10.0 }, NP { x: 2.0, y: 20.0 }]
    a.sort(v => v.x)
    print("{a[0].x},{a[0].y},{a[1].x},{a[1].y},{a[2].x},{a[2].y}")
}
"#;

/// Sorting a column Vec needs the key applied to each ELEMENT, which the boxed
/// helper supplies by materializing one. Under `nobox` that box is exactly what
/// the mark forbids -- but the key fn is reached through a fn value, whose ABI
/// is derived from the fn type, so at a `nobox` element it already takes LEAVES.
/// The leaves are the columns, so the sort hands them over directly and no
/// element is ever built.
#[test]
fn nobox_column_sort_never_materializes_an_element() {
    let lua = crate::common::try_emit_module(NOBOX_COLUMN_SORT).expect("[NP].sort must emit");
    let body = crate::common::function_body(&lua, "main");
    assert!(
        !body.contains("NP_ct(") && !body.contains("NP_lct("),
        "[NP].sort built an element; the key fn takes leaves:\n{body}"
    );
    assert_eq!(
        crate::common::run_luajit(&format!("{lua}\nmain()\n"), "col_nobox_sort"),
        "1,10,2,20,3,30",
        "every column must permute in lockstep with the key's column"
    );
}

#[test]
fn nobox_reference_elements_use_columns() {
    let lua = emit_module(r#"
nobox value ReferenceRow { payload: [int]?, position: int }
fn fill_reference_rows(payload: [int]) -> int {
    let rows: [ReferenceRow] = []
    rows.push(ReferenceRow { payload, position: 7 })
    return rows[0].position
}
fn main() { print(fill_reference_rows([3])) }
"#);
    let body = function_body(&lua, "fill_reference_rows");
    assert!(body.contains(r#"["payload"] = {}"#), "{body}");
    assert!(body.contains(r#"["position"] = {}"#), "{body}");
    assert!(!body.contains("ReferenceRow_ct("), "{body}");
}
