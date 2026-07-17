//! The JS intrinsic table (docs/js-backend-plan.md): the THIRD
//! [`IntrinsicTable`] shape after luajit (in-source `inline(lua)` bodies) and
//! GLSL. Covers the eight scalar fallback+mapping intrinsics of the conformance
//! differential (`tests/intrinsics/fallback_reference.rock`), the base math
//! set, and the `@native_required` NativeBuffer four-fn surface
//! (`docs/nativebuffer-portability-design.md`: TypedArrays as `Ptr<T>`,
//! cross-backend-verified by `tests/nb_differential.rs`). The [`emit`] module
//! lowers Rock bodies in the supported subset to JS; the node execution
//! harness runs both differentials.
//!
//! **Mappings are algorithmically distinct from the Rock fallbacks** where a
//! second implementation exists (`min`/`max` -> `Math.min`/`max` vs the
//! fallback's comparisons; `clamp` -> nested min/max; `lerp` -> `a(1-t)+bt` vs
//! the fallback's `a+(b-a)t`; `sign` -> `Math.sign` vs the comparison chain), so
//! the S4 diff compares two implementations, not a thing to itself. `round`
//! (floor(x+0.5)) and `fract` (x-floor(x)) coincide with their fallback math --
//! the diff there only proves both emit paths produce runnable JS (logged in
//! js-backend-plan Risks).

use bone_semantic::hir::Function;
use skull_mir::intrinsics::{Emission, IntrinsicEntry, IntrinsicTable};

pub mod emit;

/// (name, arity) -> `$1..$n` JS expression template. Single source for both
/// [`JsIntrinsics::lookup`] and [`JsIntrinsics::entries`]. Every entry is backed
/// by an `@intrinsic` stdlib declaration (pinned by
/// `every_js_entry_is_backed_by_a_stdlib_intrinsic`).
pub const JS_INTRINSICS: &[(&str, usize, &str)] = &[
    // base math
    ("sin", 1, "Math.sin($1)"),
    ("cos", 1, "Math.cos($1)"),
    ("sqrt", 1, "Math.sqrt($1)"),
    ("abs", 1, "Math.abs($1)"),
    ("floor", 1, "Math.floor($1)"),
    ("ceil", 1, "Math.ceil($1)"),
    ("pow", 2, "Math.pow($1, $2)"),
    ("atan2", 2, "Math.atan2($1, $2)"),
    // conformance differential set (docs/intrinsics-plan.md slice 5)
    ("min", 2, "Math.min($1, $2)"),
    ("max", 2, "Math.max($1, $2)"),
    ("clamp", 3, "Math.min(Math.max($1, $2), $3)"),
    ("lerp", 3, "($1 * (1 - $3) + $2 * $3)"),
    ("smoothstep", 3, "(function(e0,e1,x){var t=Math.min(Math.max((x-e0)/(e1-e0),0),1);return t*t*(3-2*t);})($1, $2, $3)"),
    ("round", 1, "Math.floor($1 + 0.5)"),
    ("sign", 1, "Math.sign($1)"),
    ("fract", 1, "($1 - Math.floor($1))"),
    // NativeBuffer four-fn surface (docs/nativebuffer-portability-design.md):
    // `Ptr<T>` IS the TypedArray on JS, so `.ptr` indexes with brackets exactly
    // like the luajit cdata. `NativeBuffer<int>` is pinned 32-bit -> Int32Array;
    // TypedArray store conversion wraps mod 2^n, matching LuaJIT's native
    // truncation (differentially verified by tests/nb_differential.rs).
    ("__nb_alloc_float", 1, "new Float32Array($1)"),
    ("__nb_alloc_int", 1, "new Int32Array($1)"),
    ("__nb_alloc_u8", 1, "new Uint8Array($1)"),
    ("__nb_alloc_i8", 1, "new Int8Array($1)"),
    ("__nb_alloc_u16", 1, "new Uint16Array($1)"),
    ("__nb_alloc_i16", 1, "new Int16Array($1)"),
    ("__nb_alloc_u32", 1, "new Uint32Array($1)"),
    ("nb_get", 2, "$1.ptr[$2]"),
    ("nb_set", 3, "($1.ptr[$2] = $3)"),
    // The safe accessor: an IIFE so the receiver/index evaluate exactly once
    // (the template reads each twice), mirroring the inline(lua) body's
    // param-binding. Null on miss = Rock `T?` null.
    ("get", 2, "(function(b, i) { return (i >= 0 && i < b.len) ? b.ptr[i] : null; })($1, $2)"),
];

/// JS bodies for the debug-mode bounds-checked NativeBuffer access the
/// emitter's `Index`/`StoreIndex` lowering calls (`__nb_checked_get` /
/// `__nb_checked_set`) -- the JS analog of the stdlib inline(lua) helpers
/// (`stdlib/unified.rock`) that back the same calls on luajit, and of
/// `skull_codegen_lua::VEC_CHECK_SHIMS` in shape. Prepend to any emitted JS
/// program that indexes a buffer. The NaN-into-integer-buffer rejection is the
/// pinned store-edge debug check (`docs/nativebuffer-portability-design.md`,
/// decision 2); it is unconditional here -- the JS harness has no release mode.
pub const JS_NB_CHECK_SHIMS: &str = "\
function __nb_checked_get(buf, i) {
  if (i < 0 || i >= buf.len) {
    throw new Error(\"NativeBuffer index out of bounds: index \" + i + \" is outside [0, \" + buf.len + \")\");
  }
  return buf.ptr[i];
}
function __nb_checked_set(buf, i, v) {
  if (i < 0 || i >= buf.len) {
    throw new Error(\"NativeBuffer index out of bounds: index \" + i + \" is outside [0, \" + buf.len + \")\");
  }
  if (v !== v && !(buf.ptr instanceof Float32Array)) {
    throw new Error(\"NativeBuffer store: NaN into integer buffer\");
  }
  buf.ptr[i] = v;
}
";

/// The toy JS backend's [`IntrinsicTable`].
pub struct JsIntrinsics;

impl IntrinsicTable for JsIntrinsics {
    fn backend(&self) -> &'static str {
        "js"
    }
    fn lookup(&self, f: &Function) -> Option<Emission> {
        let n = f.name.as_str();
        let arity = f.param_decls.len();
        JS_INTRINSICS
            .iter()
            .find(|(name, ar, _)| *name == n && *ar == arity)
            .map(|(_, _, tmpl)| Emission::Template(tmpl))
    }
    fn entries(&self) -> Vec<IntrinsicEntry> {
        JS_INTRINSICS
            .iter()
            .map(|(name, arity, _)| IntrinsicEntry { name, arity: *arity })
            .collect()
    }
}

/// Fill `$1..$n` in an expression template with `args` -- the call-site render
/// primitive a real JS codegen would use (the same shape GLSL uses). Kept here
/// so the `Emission::Template` contract is exercised end to end by a test.
pub fn fill_template(template: &str, args: &[&str]) -> String {
    let mut s = template.to_string();
    for (i, a) in args.iter().enumerate() {
        s = s.replace(&format!("${}", i + 1), a);
    }
    s
}
