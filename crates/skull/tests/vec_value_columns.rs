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
// A lambda body, a nested `fn`, a `co fn`'s state machine and a global
// initializer are each their own emitted callable, and the inliner scans only
// TOP-LEVEL CFG blocks (a cell used in a lambda body is an upvalue, so splicing
// there piles onto LuaJIT's 60-upvalue cap). So a `for` over a column-stored Vec
// inside one reaches the SHARED generic `VecIter<T>.next`, which reads the
// `_data` that column storage does not have.
//
// The lambda and nested-fn cases WORK: `convert_closures` splits those bodies
// into standalone functions with their own top-level CFG, at which point the
// upvalue reason is gone and the inliner runs over them. The `co fn` and
// global-init cases are still RED -- neither runs the inliner at all -- and
// `column_iteration_rejects_where_it_cannot_specialize` pins that they REJECT
// meanwhile, so a regression to a silent miscompile fails a test rather than
// merely leaving them red for a new reason.
//
// The ordinary-call guard (`in_unspliced_ctx` -> keep the shared boxed ABI,
// `lower.rs:3965`) is NOT the fix here: for a boxed value the shared body is
// CORRECT and the guard merely forgoes an optimization, whereas for columns the
// shared body is WRONG -- taking that branch would turn a loud rejection into a
// miscompile. That distinction is what
// `unspliceable_iteration_of_boxed_elements_still_emits` guards from the other
// side: the rejection must reach column-eligible elements ONLY.

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

/// The body of the single emitted function whose name contains `needle`.
/// Synthetic callables carry a generated suffix (`<mangled>__lambda0`) or a
/// hash (`__local_go_<hash>_inner_0`), so they are found by pattern rather
/// than spelled out. Panics if it is not unique -- an ambiguous match would
/// silently assert against the wrong body.
fn body_of_synthetic<'a>(lua: &'a str, needle: &str) -> &'a str {
    let names: Vec<&str> = lua
        .match_indices("\nfunction ")
        .map(|(i, m)| {
            let rest = &lua[i + m.len()..];
            &rest[..rest.find('(').expect("fn header has `(`")]
        })
        .filter(|n| n.contains(needle))
        .collect();
    assert_eq!(names.len(), 1, "expected exactly one fn containing `{needle}`, got {names:?}");
    common::function_body(lua, names[0])
}

/// Iterating a column-stored Vec inside a LAMBDA. Real engine code hits it
/// (`draw_glow_lights`, a `for` over `GLOW_LIGHTS` inside a
/// `with_blend_mode(..., () => ..)` callback).
///
/// Scoped to the LAMBDA's own body: the stdlib always emits `_data`, so a
/// module-wide assertion could never go green even after a correct fix.
#[test]
fn column_vec_iterated_inside_a_lambda() {
    let lua = common::try_emit_module(VEC_IN_LAMBDA)
        .expect("column [V2] iterated in a lambda must emit");
    let body = body_of_synthetic(&lua, "__lambda");
    assert!(
        !body.contains("_data"),
        "lambda body read the single `_data` backing of a column-stored [V2]:\n{body}"
    );
    assert_eq!(
        common::run_luajit(&format!("{lua}\nmain()\n"), "col_lambda"),
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
    let lua = common::try_emit_module(VEC_IN_NESTED_FN)
        .expect("column [V2] iterated in a nested fn must emit");
    let body = body_of_synthetic(&lua, "_inner_");
    assert!(
        !body.contains("next_VecIter_T_"),
        "nested fn called the UNSPECIALIZED VecIter `next` over column storage \
         (silent miscompile -- no validator error):\n{body}"
    );
    assert_eq!(
        common::run_luajit(&format!("{lua}\nmain()\n"), "col_nested"),
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
    let lua = common::try_emit_module(VEC_IN_CO_FN)
        .expect("column [V2] iterated in a co fn must emit");
    let body = body_of_synthetic(&lua, "__step");
    assert!(
        !body.contains("next_VecIter_T_"),
        "co fn `__step` called the UNSPECIALIZED VecIter `next` over column \
         storage (the raised error is swallowed by `__resume`'s pcall):\n{body}"
    );
    assert_eq!(
        common::run_luajit(&format!("{lua}\nmain()\n"), "col_cofn"),
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
    let lua = common::try_emit_module(VEC_IN_GLOBAL_INIT)
        .expect("column [V2] iterated in a global initializer must emit");
    let body = body_of_synthetic(&lua, "ginit_TOTAL");
    assert!(
        !body.contains("next_VecIter_T_"),
        "global initializer called the UNSPECIALIZED VecIter `next` over column \
         storage:\n{body}"
    );
    assert_eq!(common::run_luajit(&format!("{lua}\nmain()\n"), "col_ginit"), "4");
}

/// The two contexts specialization does not reach yet must REJECT rather than
/// emit. This is what keeps the tests above honest: they are red either way, so
/// without this a regression from "loud rejection" back to "silent miscompile"
/// would not fail anything.
///
/// Neither remaining context runs the inliner AT ALL, which is why hoisting did
/// not fix them the way it fixed the lambda and nested-fn cases: a coroutine
/// deliberately neither inlines nor is inlined into, and a global-init unit has
/// no inline pass. Reaching them wants the specialized callee emitted as its own
/// top-level function and CALLED, rather than spliced.
///
/// The global-initializer case additionally pins that the rejection travels the
/// hard-error channel and not `UnitOut::skipped`, which is silent unless
/// `SKULL_DEBUG_SKIPPED` is set.
#[test]
fn column_iteration_rejects_where_it_cannot_specialize() {
    for (ctx, src) in [("co fn", VEC_IN_CO_FN), ("global init", VEC_IN_GLOBAL_INIT)] {
        let err = common::try_emit_module(src)
            .err()
            .unwrap_or_else(|| panic!("{ctx}: unspliceable column iteration must not emit"));
        assert!(
            err.message.contains("was not spliced"),
            "{ctx}: rejected for the wrong reason: {}",
            err.message
        );
    }
}

const VEC_ENUMERATE: &str = r#"
value V2 { x: float, y: float }

fn main() {
    let xs: [V2] = [V2 { x: 1.0, y: 10.0 }, V2 { x: 2.0, y: 20.0 }, V2 { x: 3.0, y: 30.0 }]
    for (i, v) in enumerate(xs) { print("{i}:{v.x},{v.y}") }
    for pair in enumerate(xs) { print("t{pair.0}:{pair.1.x}") }
}
"#;

/// `enumerate` was the first confirmed casualty of the un-audited stdlib Vec
/// surface and the one real game code hit: bitgun's `update_survivors`
/// enumerates a `[Vec2]`, which stopped the game compiling.
///
/// In `for` position `enumerate` never reaches the stdlib overload -- bone keeps
/// the builtin template and skull lowers it directly -- so the fix is in the loop
/// lowering, not in `stdlib/unified.rock`. Both binding forms are covered: `(i,
/// v)` and the single binding to the whole `(index, element)` tuple, which take
/// different branches.
///
/// Values AND indices are asserted. A shape-only assertion would not catch a
/// decomposition that reads the right columns at the wrong offset, which is the
/// plausible failure once the element read is no longer a single slot.
#[test]
fn enumerate_over_column_storage_yields_correct_pairs() {
    let lua = common::try_emit_module(VEC_ENUMERATE).expect("`enumerate` over columns must emit");
    assert!(
        !common::function_body(&lua, "main").contains("_data"),
        "enumerate read the boxed `_data` backing of a column-stored [V2]"
    );
    assert_eq!(
        common::run_luajit(&format!("{lua}\nmain()\n"), "col_enumerate"),
        "0:1,10\n1:2,20\n2:3,30\nt0:1\nt1:2\nt2:3"
    );
}

const VEC_GET_VIA_PRIMITIVE: &str = r#"
value V2 { x: float, y: float }

fn main() {
    let xs: [V2] = [V2 { x: 1.0, y: 10.0 }, V2 { x: 2.0, y: 20.0 }]
    let g = xs.get(1)
    if g != null { print("{g.x},{g.y}") }
}
"#;

/// `__vec_data_get/_set/_unset` are the representation boundary: nearly the whole
/// stdlib Vec surface (`get`, `set`, `pop`, `clear`, `insert_at`, `remove_at`,
/// `swap_remove`) reaches storage only through them, so they are lowered
/// column-aware instead of each helper being intercepted by name.
///
/// It is a STATIC decision per instantiation, not a runtime test for whether
/// `_data` exists: `vec_column_elem_of` resolves through the active `SpecSubst`,
/// so the shared snapshot of a generic body keeps the boxed backing while the
/// snapshot specialized at `T = V2` lowers to column operations.
///
/// `get` is the one that proves the mechanism today, because `vec_get` is
/// `@inline` and so actually gets specialized at a concrete site. `set`, `pop`,
/// `clear`, `remove_at` and `swap_remove` are plain generics that nothing
/// specializes yet, so they still reach the shared `_data` body and still fail --
/// that is the standalone-specialized-callee work, not a gap in this lowering.
#[test]
fn vec_data_primitives_are_column_aware_when_specialized() {
    let lua = common::try_emit_module(VEC_GET_VIA_PRIMITIVE).expect("`get` over columns must emit");
    assert_eq!(common::run_luajit(&format!("{lua}\nmain()\n"), "col_vec_get"), "2,20");
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
    let lua = common::try_emit_module(VEC_BOXED_ELEMS_UNSPLICED)
        .expect("boxed element types must still emit in unspliceable contexts");
    assert_eq!(common::run_luajit(&format!("{lua}\nmain()\n"), "boxed_unspliced"), "4");
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
