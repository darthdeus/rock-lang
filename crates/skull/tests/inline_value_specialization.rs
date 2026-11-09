//! Call-site specialization via the inliner. An `@inline` generic fn whose param/return bind to a VALUE
//! struct at the call site is spliced as a PER-INSTANTIATION specialized snapshot:
//! the body lowers as if hand-written for `V2`, so the value flows field-wise (no
//! `V2_ct(..)` box, no boxed `__nb_checked_set` store). The bounds-checked slot
//! accessor `__nb_checked_get` REMAINS -- it is how a scalarized store reaches the
//! flat slot to write its fields in place; only the box + the boxed store vanish.
//!
//! Pre-P2 this test pinned the BOXED status quo (`V2_ct(..)` + `__nb_checked_set`);
//! landing the feature flipped it to the zero-box assertions below (git `c42bd06ce`
//! is the boxed-premise revision, for the before/after diff).
use crate::common::{compile_and_run, emit_module, function_body};

// A generic `@inline` set/get over a `NativeBuffer<T>`, instantiated at the value
// struct `V2`. Mirrors the FlatVec push/get shape (`buf[len] = v` / `return buf[i]`).
const VALUE_INST: &str = r#"
value V2 { x: float, y: float }

@nomangle inline(lua) fn __nb_alloc_V2(n: int) -> Ptr<V2> = 'return ffi.new("V2[?]", n)'

@inline fn gset<T>(b: NativeBuffer<T>, i: int, v: T) { b[i] = v }
@inline fn gget<T>(b: NativeBuffer<T>, i: int) -> T { return b[i] }

fn use_buf(a: V2) -> V2 {
    let b: NativeBuffer<V2> = NativeBuffer { ptr: __nb_alloc_V2(4), len: 4 }
    gset(b, 0, a)
    return gget(b, 0)
}

fn main() { let r = use_buf(V2 { x: 1.0, y: 2.0 }); print("{r.x}") }
"#;

#[test]
fn generic_inline_at_value_type_splices_zero_box() {
    let lua = emit_module(VALUE_INST);
    let body = function_body(&lua, "use_buf");

    // The value never boxes: the specialized splice threads it as scalar leaves.
    assert!(
        !body.contains("V2_ct("),
        "specialized splice must not box the value (`V2_ct(..)`); got:\n{body}"
    );
    // The boxed whole-value store is gone -- the store scalarizes to field writes.
    assert!(
        !body.contains("__nb_checked_set"),
        "specialized store must be field-wise, not the boxed `__nb_checked_set`; got:\n{body}"
    );
    // Positive: the element is bounds-checked once by INDEX and its leaves are
    // written through typed views -- no slot reference is fetched, so the
    // specialized store allocates nothing on any path, not just on trace.
    assert!(
        body.contains("__nb_checked_index"),
        "specialized store must bounds-check the element access; got:\n{body}"
    );
    assert!(
        !body.contains("__nb_checked_get"),
        "specialized store should take no slot reference; got:\n{body}"
    );
}

// A generic `@inline` fn calling `native_buffer_alloc<T>` -- the intrinsic the
// shared (opaque-`T`) body cannot lower. Uses the stdlib `flat_vec_new<T>` /
// `push`; specialized at `V2`, the ctype resolves and the body emits the
// `ffi.new("V2[?]", ..)` it could not name generically. `nb_alloc` is a local
// generic wrapper exercising the intrinsic directly too.
const INTRINSIC_INST: &str = r#"
value V2 { x: float, y: float }

fn make() -> float {
    let fv: FlatVec<V2> = flat_vec_new(8)
    fv.push(V2 { x: 3.0, y: 4.0 })
    return fv.get(0).x
}

fn main() { let _ = make() }
"#;

#[test]
fn generic_inline_intrinsic_resolves_ctype_at_value_instantiation() {
    let lua = emit_module(INTRINSIC_INST);
    let body = function_body(&lua, "make");
    // The specialized `native_buffer_alloc<T>` names the concrete element ctype --
    // the whole point of P2 for the alloc intrinsic (P1 could only do it at a
    // concrete, non-generic call site).
    assert!(
        body.contains(r#"ffi.new("V2[?]","#),
        "specialized intrinsic must emit `ffi.new(\"V2[?]\", ..)`; got:\n{body}"
    );
    // No dangling call to the shared generic `flat_vec_new` survives.
    assert!(
        !body.contains("flat_vec_new"),
        "the specialized `flat_vec_new` must be spliced, not left as a call; got:\n{body}"
    );
}

const TRANSITIVE_CONCRETE_INTRINSIC: &str = r#"
value TV2 { x: float, y: float }

fn make_indirect<T>(cap: int) -> FlatVec<T> {
    return flat_vec_new(cap)
}

fn main() {
    let fv: FlatVec<TV2> = make_indirect(4)
    print("{fv.len}")
}
"#;

#[test]
fn non_inline_generic_chain_uses_concrete_native_buffer_instance() {
    let lua = emit_module(TRANSITIVE_CONCRETE_INTRINSIC);
    assert!(
        lua.contains(r#"ffi.new("TV2[?]"#),
        "the transitive concrete instance must own the native-buffer ctype, got:\n{lua}"
    );
}

#[test]
fn inlined_numeric_parameters_preserve_float_coercions() {
    assert_eq!(
        compile_and_run(
            r#"
fn fit(w: int, h: int, available: float) -> Vec2 {
    let width = 256
    let height = max(1, (width as float * h as float / w as float) as int)
    let size = v(width as float, height as float)
    let factor = min(1.0, available / size.x)
    return size * factor
}
fn main() {
    let small = fit(128, 64, 32.0)
    let large = fit(128, 64, 512.0)
    print("{small.x as int} {small.y as int} {large.x as int} {large.y as int}")
}
"#
        )
        .trim(),
        "32 16 256 128"
    );
}
