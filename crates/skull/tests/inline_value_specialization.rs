//! P2 premise oracle (docs/flatvec-plan.md "Mechanism: call-site specialization
//! via the inliner"). An `@inline` generic fn whose param/return bind to a VALUE
//! struct at the call site is spliced today as OPAQUE code: the generic body
//! lowers once with `T` erased, so a value flowing through it is BOXED (`V2_ct(..)`)
//! and indexed through the generic `__nb_checked_{set,get}` helpers -- the value
//! never scalarizes into field-wise cdata slot stores.
//!
//! This pins that boxed status quo. When P2's per-instantiation specialization
//! lands, the spliced body must lower as if hand-written for `V2`: the `V2_ct`
//! box and the `__nb_checked_*` boxed accessors both DISAPPEAR, replaced by
//! direct field-wise slot stores/loads. Flip the assertions (see the FLIP-ON-P2
//! block below) in the same commit that lands the feature.

mod common;
use common::{emit_module, function_body};

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
fn generic_inline_at_value_type_splices_boxed_status_quo() {
    let lua = emit_module(VALUE_INST);
    let body = function_body(&lua, "use_buf");

    // -- STATUS QUO (pre-P2): the spliced generic body is type-opaque. --
    // The value struct is BOXED to pass through the opaque-`T` store boundary.
    assert!(
        body.contains("V2_ct("),
        "premise: an opaque-`T` generic splice BOXES the value (`V2_ct(..)`); got:\n{body}"
    );
    // The store/load route through the generic (boxed-cdata) accessors, not
    // field-wise slot ops.
    assert!(
        body.contains("__nb_checked_set") && body.contains("__nb_checked_get"),
        "premise: the opaque splice indexes via the boxed `__nb_checked_*` helpers; got:\n{body}"
    );

    // -- FLIP-ON-P2 --------------------------------------------------------
    // When call-site specialization lands, replace the two asserts above with:
    //   assert!(!body.contains("V2_ct("),          "specialized splice must not box the value");
    //   assert!(!body.contains("__nb_checked_set"), "specialized store must be field-wise, not boxed");
    //   assert!(!body.contains("__nb_checked_get"), "specialized load must be field-wise, not boxed");
    // and add a positive assert on the field-wise slot store shape the
    // specialized body emits (e.g. `.ptr[0].x =` / `.ptr[0].y =`).
    // ----------------------------------------------------------------------
}
