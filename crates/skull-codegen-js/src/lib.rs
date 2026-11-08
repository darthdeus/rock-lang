//! The JS backend: intrinsic table, emitter and runtime prelude. The THIRD
//! [`IntrinsicTable`] shape after luajit (in-source `inline(lua)` bodies) and
//! GLSL. Covers the eight scalar fallback+mapping intrinsics of the conformance
//! differential (`tests/intrinsics/fallback_reference.rock`), the base math
//! set, and the `@native_required` NativeBuffer four-fn surface
//! (TypedArrays as `Ptr<T>`,
//! cross-backend-verified by `tests/nb_differential.rs`). The [`emit`] module
//! lowers Rock bodies in the supported subset to JS; the node execution
//! harness runs both differentials.
//!
//! **Mappings are algorithmically distinct from the Rock fallbacks** where a
//! second implementation exists (`min`/`max` -> `Math.min`/`max` vs the
//! fallback's comparisons; `clamp` -> nested min/max; `lerp` -> `a(1-t)+bt` vs
//! the fallback's `a+(b-a)t`; `sign` -> `Math.sign` vs the comparison chain), so
//! the differential compares two implementations, not a thing to itself. `round`
//! (floor(x+0.5)) and `fract` (x-floor(x)) coincide with their fallback math --
//! the diff there only proves both emit paths produce runnable JS (logged in
//! optional backend risk).

use bone_semantic::hir::Function;
use skull_mir::intrinsics::{Emission, IntrinsicEntry, IntrinsicTable};

pub mod emit;

/// (declaration key, arity) -> JS emission. Keys are source names or declared
/// signature mangles for independently mapped overloads. Single source for both
/// [`JsIntrinsics::lookup`] and [`JsIntrinsics::entries`]. Every entry is backed
/// by an `@intrinsic` declaration in the stdlib or the test-local `put`
/// fixture (pinned by `every_js_entry_is_backed_by_a_declared_intrinsic`).
pub const JS_INTRINSICS: &[(&str, usize, Emission)] = &[
    // base math
    ("sin", 1, Emission::Template("Math.sin($1)")),
    ("cos", 1, Emission::Template("Math.cos($1)")),
    ("tan", 1, Emission::Template("Math.tan($1)")),
    ("sqrt", 1, Emission::Template("Math.sqrt($1)")),
    ("exp", 1, Emission::Template("Math.exp($1)")),
    // `log` is the NATURAL log on both hosts (Lua `math.log`, JS `Math.log`).
    // `log2` gets `Math.log2` rather than a mirror of luajit's
    // `math.log(x) / 0.6931471805599453`: it is the same function computed
    // better, exact on powers of two where the division is a bit short.
    ("log", 1, Emission::Template("Math.log($1)")),
    ("log2", 1, Emission::Template("Math.log2($1)")),
    ("abs", 1, Emission::Template("Math.abs($1)")),
    ("floor", 1, Emission::Template("Math.floor($1)")),
    ("ceil", 1, Emission::Template("Math.ceil($1)")),
    ("pow", 2, Emission::Template("Math.pow($1, $2)")),
    ("atan2", 2, Emission::Template("Math.atan2($1, $2)")),
    // conformance differential set
    ("min", 2, Emission::Template("Math.min($1, $2)")),
    ("max", 2, Emission::Template("Math.max($1, $2)")),
    (
        "clamp",
        3,
        Emission::Template("Math.min(Math.max($1, $2), $3)"),
    ),
    ("lerp", 3, Emission::Template("($1 * (1 - $3) + $2 * $3)")),
    (
        "smoothstep",
        3,
        Emission::Template(
            "(function(e0,e1,x){var t=Math.min(Math.max((x-e0)/(e1-e0),0),1);return t*t*(3-2*t);})($1, $2, $3)",
        ),
    ),
    ("round", 1, Emission::Template("Math.floor($1 + 0.5)")),
    ("sign", 1, Emission::Template("Math.sign($1)")),
    ("fract", 1, Emission::Template("($1 - Math.floor($1))")),
    // `join` over a `[string]`: the backing array IS the input both hosts want,
    // so this is one `Array.prototype.join` here and one `table.concat` on
    // luajit. `_data` is 0-based and may be longer than `len`, hence the slice.
    (
        "join",
        2,
        Emission::Template("$1._data.slice(0, $1.len).join($2)"),
    ),
    // String primitives declared in stdlib/unified.rock. Indices are native
    // units (Lua bytes, JS UTF-16); substring bounds are end-exclusive.
    // `find` returns a 0-based index, or -1 when the needle is absent.
    ("find", 3, Emission::Template("$1.indexOf($2, $3)")),
    ("substring", 3, Emission::Template("$1.slice($2, $3)")),
    ("unit_at", 2, Emission::Template("($1.charCodeAt($2) || 0)")),
    ("upper", 1, Emission::Template("$1.toUpperCase()")),
    ("lower", 1, Emission::Template("$1.toLowerCase()")),
    (
        "string_char",
        1,
        Emission::Template("String.fromCharCode($1)"),
    ),
    // Reference identity (stdlib `same_ref`). `===` on two objects is JS
    // identity, the same question luajit's `rawequal` answers; on primitives
    // both reduce to value equality, which is also what identity means there.
    ("same_ref", 2, Emission::Template("($1 === $2)")),
    // IEEE non-finite predicates. isinf excludes NaN
    // (a NaN is neither finite nor infinite). any/greater_than/to_vec3 co-emit
    // their portable Rock bodies -- no JS row.
    ("isnan", 1, Emission::Template("Number.isNaN($1)")),
    (
        "isinf",
        1,
        Emission::Template("(!Number.isFinite($1) && !Number.isNaN($1))"),
    ),
    // NativeBuffer four-fn surface:
    // `Ptr<T>` IS the TypedArray on JS, so `.ptr` indexes with brackets exactly
    // like the luajit cdata. TypedArray store conversion wraps mod 2^n, matching
    // LuaJIT's native truncation (differentially verified by tests/nb_differential.rs).
    // `int`/`float` allocs no longer route through a named intrinsic -- they lower
    // to the `FfiNewArray` MIR node (`native_buffer_alloc<T>`), rendered in
    // `emit.rs`; only the phantom-width variants keep a per-type template here.
    ("__nb_alloc_u8", 1, Emission::Template("new Uint8Array($1)")),
    ("__nb_alloc_i8", 1, Emission::Template("new Int8Array($1)")),
    (
        "__nb_alloc_u16",
        1,
        Emission::Template("new Uint16Array($1)"),
    ),
    (
        "__nb_alloc_i16",
        1,
        Emission::Template("new Int16Array($1)"),
    ),
    (
        "__nb_alloc_u32",
        1,
        Emission::Template("new Uint32Array($1)"),
    ),
    (
        "native_buffer_uint",
        1,
        Emission::Template("({ptr: new Uint32Array($1), len: $1})"),
    ),
    ("nb_get", 2, Emission::Template("$1.ptr[$2]")),
    ("nb_set", 3, Emission::Template("($1.ptr[$2] = $3)")),
    // StringBuilder: a Lua table used as an array accumulator IS a JS array.
    // `sb_new` -> `[]`, `sb_push` -> `.push` (Lua `sb[#sb+1] = s`; the returned
    // length is discarded), `sb_build` -> `.join("")` (Lua `table.concat`).
    ("sb_new", 0, Emission::Template("[]")),
    ("sb_push", 2, Emission::Template("$1.push($2)")),
    ("sb_build", 1, Emission::Template("$1.join(\"\")")),
    // A declared-signature key keeps Map get independently checked from
    // NativeBuffer get. It also wins before the latter's name/arity key.
    ("get_map_K_V_K", 2, Emission::Template("$1.get($2)")),
    // The safe accessor: an IIFE so the receiver/index evaluate exactly once
    // (the template reads each twice), mirroring the inline(lua) body's
    // param-binding. Null on miss = Rock `T?` null.
    (
        "get",
        2,
        Emission::Template(
            "(function(b, i) { return (i >= 0 && i < b.len) ? b.ptr[i] : null; })($1, $2)",
        ),
    ),
    // Host runtime bindings; declarations opt in with @intrinsic.
    ("tostring", 1, Emission::Template("lua_tostring($1)")),
    (
        "__rock_format_float",
        1,
        Emission::Template("__rock_format_float($1)"),
    ),
    ("int_to_float", 1, Emission::Template("$1")),
    ("to_float", 1, Emission::Template("$1")),
    ("phantom", 0, Emission::Template("null")),
    ("is_null", 1, Emission::Template("($1 == null)")),
    ("cast", 1, Emission::Template("$1")),
    ("transmute", 1, Emission::Template("$1")),
    // Raw table helpers dispatch on Map versus array/object storage.
    ("table_get", 2, Emission::Template("__table_get($1, $2)")),
    (
        "table_set",
        3,
        Emission::Template("__table_set($1, $2, $3)"),
    ),
    (
        "table_unset",
        2,
        Emission::Template("__table_unset($1, $2)"),
    ),
    ("table_len", 1, Emission::Template("__table_len($1)")),
    // Vec backing indices are 0-based on JS, including shifted Lua indices.
    ("__vec_data_get", 2, Emission::Template("$1._data[$2]")),
    ("__vec_data_set", 3, Emission::Template("$1._data[$2] = $3")),
    (
        "__vec_data_unset",
        2,
        Emission::Template("$1._data[$2] = undefined"),
    ),
    (
        "__vec_data_alloc",
        1,
        Emission::Template("({_data: new Array($1).fill(false), len: 0, __ty: \"Vec\"})"),
    ),
    (
        "__vec_data_capacity",
        1,
        Emission::Template("$1._data.length"),
    ),
    (
        "__vec_data_insert",
        3,
        Emission::Template("$1._data.splice($2 - 1, 0, $3)"),
    ),
    (
        "__vec_data_clone",
        1,
        Emission::Template("({_data: $1._data.slice(), len: $1.len, __ty: \"Vec\"})"),
    ),
    (
        "__vec_data_sort",
        2,
        Emission::Template("__vec_sort($1, $2)"),
    ),
    ("table_new", 0, Emission::Template("({})")),
    (
        "vec_new",
        0,
        Emission::Template("({_data: [], len: 0, __ty: \"Vec\"})"),
    ),
    (
        "vec_push",
        2,
        Emission::Template("($1._data[$1.len] = $2, $1.len = $1.len + 1)"),
    ),
    ("__op_eq", 2, Emission::Template("__op_eq($1, $2)")),
    ("__op_ne", 2, Emission::Template("(!__op_eq($1, $2))")),
    ("len", 1, Emission::Template("$1.length")),
    ("float", 1, Emission::Template("$1")),
    // The runtime encoder checks the code point like luajit, then emits UTF-16.
    (
        "from_codepoint",
        1,
        Emission::Template("__from_codepoint($1)"),
    ),
    ("parse_float", 1, Emission::Template("__parse_float($1)")),
    (
        "parse_float_at",
        3,
        Emission::Template("__parse_float($1.slice($2, $3))"),
    ),
    // RNG streams share LuaJIT's seedable, serializable transition.
    ("random", 0, Emission::Template("__rock_random()")),
    (
        "random_int",
        2,
        Emission::Template("__rock_random_int($1, $2)"),
    ),
    ("random_seed", 1, Emission::Template("__rock_rng_seed($1)")),
    (
        "random_get_state",
        0,
        Emission::Template("__rock_rng_state_get()"),
    ),
    (
        "random_set_state",
        1,
        Emission::Template("__rock_rng_state_set($1)"),
    ),
    (
        "presentation_random",
        0,
        Emission::Template("__rock_presentation_random()"),
    ),
    (
        "presentation_random_int",
        2,
        Emission::Template("__rock_presentation_random_int($1, $2)"),
    ),
    (
        "presentation_random_seed",
        1,
        Emission::Template("__rock_presentation_rng_seed($1)"),
    ),
    (
        "presentation_random_get_state",
        0,
        Emission::Template("__rock_presentation_rng_state_get()"),
    ),
    (
        "presentation_random_set_state",
        1,
        Emission::Template("__rock_presentation_rng_state_set($1)"),
    ),
    ("env_var", 1, Emission::Template("__rock_env_var($1)")),
    ("deepcopy", 1, Emission::Template("deepcopy($1)")),
    // Like LuaJIT, JS uses GC storage; a destination token does not change the copy.
    ("clone_into", 2, Emission::Template("deepcopy($1)")),
    ("map_storage", 1, Emission::Template("null")),
    ("storage", 1, Emission::Template("null")),
    (
        "__rock_deepcopy_value",
        2,
        Emission::Template("__rock_deepcopy_value($1, $2)"),
    ),
    // Omitted optional arguments default inside the runtime.
    ("inspect", 3, Emission::Template("inspect($1, $2, $3)")),
    (
        "inspect_formatted",
        2,
        Emission::Template("inspect_formatted($1, $2)"),
    ),
    (
        "__rock_fmt_pad",
        4,
        Emission::Template("__rock_fmt_pad($1, $2, $3, $4)"),
    ),
    (
        "__rock_fmt_grouping",
        3,
        Emission::Template("__rock_fmt_grouping($1, $2, $3)"),
    ),
    (
        "__rock_fmt_binary",
        1,
        Emission::Template("__rock_fmt_binary($1)"),
    ),
    (
        "__rock_fmt_percent",
        2,
        Emission::Template("__rock_fmt_percent($1, $2)"),
    ),
    (
        "format_float",
        3,
        Emission::Template("__rock_sprintf(\"%\" + $2 + \".\" + $3 + \"f\", $1)"),
    ),
    ("tuple_first", 1, Emission::Template("$1[0]")),
    ("tuple_second", 1, Emission::Template("$1[1]")),
    ("__range", 2, Emission::Template("__range($1, $2)")),
    ("__range_uint", 2, Emission::Template("__range($1, $2)")),
    ("try_call", 1, Emission::Template("__try_call($1)")),
    // Coroutine fields normalize unset JS values to Rock null.
    (
        "resume",
        1,
        Emission::Template(
            "(function(c){var v=__rock_call(c.__resume,c);return v===undefined?null:v;})($1)",
        ),
    ),
    (
        "resume",
        2,
        Emission::Template(
            "(function(c,a){var v=__rock_call(c.__resume,c,a);return v===undefined?null:v;})($1, $2)",
        ),
    ),
    (
        "result",
        1,
        Emission::Template("(function(c){var v=c.__return;return v===undefined?null:v;})($1)"),
    ),
    (
        "status",
        1,
        Emission::Template(
            "(function(c){return c.__state === -2 ? \"errored\" : (c.__state < 0 ? \"dead\" : \"suspended\");})($1)",
        ),
    ),
    ("completed", 1, Emission::Template("($1.__state === -3)")),
    (
        "last_error",
        1,
        Emission::Template("(function(c){var v=c.__error;return v===undefined?null:v;})($1)"),
    ),
    ("kill", 1, Emission::Template("__rock_coroutine_kill($1)")),
    (
        "fire_hot_reload",
        1,
        Emission::Template(
            "(function(c){ if (c.__state >= 0 && c.__on_hot_reload) __rock_call(c.__on_hot_reload,c); })($1)",
        ),
    ),
    (
        "error",
        1,
        Emission::Template("(() => { throw new Error($1); })()"),
    ),
    ("print", 1, Emission::Global("print")),
    ("put", 1, Emission::Global("put")),
    ("__rock_log_init", 0, Emission::Global("__rock_log_init")),
    ("__rock_log_emit", 4, Emission::Global("__rock_log_emit")),
    ("__rock_log", 3, Emission::Global("__rock_log")),
    (
        "__rock_log_wall_clock",
        3,
        Emission::Global("__rock_log_wall_clock"),
    ),
    ("log_set_level", 1, Emission::Global("log_set_level")),
    ("_log_once_impl", 4, Emission::Global("_log_once_impl")),
    (
        "_log_throttled_with_now_impl",
        7,
        Emission::Global("_log_throttled_with_now_impl"),
    ),
    ("log_set_now", 1, Emission::Global("log_set_now")),
    ("log_clear_now", 0, Emission::Global("log_clear_now")),
    ("__rock_log_now", 0, Emission::Global("__rock_log_now")),
    (
        "_log_throttled_impl",
        6,
        Emission::Global("_log_throttled_impl"),
    ),
    ("log_reset_dedup", 0, Emission::Global("log_reset_dedup")),
    ("log_dedup_count", 0, Emission::Global("log_dedup_count")),
    (
        "log_suppression_summary",
        0,
        Emission::Global("log_suppression_summary"),
    ),
    ("map_new", 0, Emission::Template("new Map()")),
];

/// JS bodies for the debug-mode bounds-checked NativeBuffer access the
/// emitter's `Index`/`StoreIndex` lowering calls (`__nb_checked_get` /
/// `__nb_checked_set`) -- the JS analog of the stdlib inline(lua) helpers
/// (`stdlib/unified.rock`) that back the same calls on luajit, and of
/// `skull_codegen_lua::VEC_CHECK_SHIMS` in shape. Prepend to any emitted JS
/// program that indexes a buffer. The NaN-into-integer-buffer rejection is the
/// pinned store-edge debug check; it is unconditional here -- the JS harness has no release mode.
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

/// JS shims for the checked Vec indexing helpers codegen emits
/// (`__vec_checked_get`/`__vec_checked_set`): `_data` is a 0-based dense array
/// (`_data[i]`), bounds-erroring on miss. The error message shape follows the
/// luajit stdlib (`f5efe7604`); it is not compared in-scope (an OOB index
/// throws -> the harness reports FAIL, matching luajit -- `try_call` catching is
/// out of scope).
pub const JS_VEC_CHECK_SHIMS: &str = "\
function __vec_checked_get(vec, i) {
  if (i < 0 || i >= vec.len) { throw new Error(\"Vec index out of bounds: index \" + i + \" is outside [0, \" + vec.len + \")\"); }
  return vec._data[i];
}
function __vec_checked_set(vec, i, v) {
  if (i < 0 || i >= vec.len) { throw new Error(\"Vec index out of bounds: index \" + i + \" is outside [0, \" + vec.len + \")\"); }
  vec._data[i] = v;
}
// Column storage checks the element index ONCE, then every per-leaf read/write
// indexes its column raw. Returns `i` where the luajit sibling returns `i + 1`
// (`stdlib/unified.rock`): a column indexes the way `_data` does on its own
// backend, and JS `_data` is 0-based. This is the only place that base
// difference lives.
function __vec_checked_index(vec, i) {
  if (i < 0 || i >= vec.len) { throw new Error(\"Vec index out of bounds: index \" + i + \" is outside [0, \" + vec.len + \")\"); }
  return i;
}
// `Vec.sort(key_fn)`: the stdlib inline(lua) is `table.sort(_data, |a,b| key(a) <
// key(b))`. Sort the logical `0..len` slice, then write back. `key_fn` is a fn
// value invoked through `__rock_call`; keys compare with `<` (numbers/strings).
// JS sort is stable where luajit's is not, so equal-key relative order can
// differ -- capture-free key spec keeps this observable only for genuine ties.
function __vec_sort(vec, key_fn) {
  var n = vec.len;
  var part = [];
  for (var i = 0; i < n; i++) part.push(vec._data[i]);
  part.sort(function(a, b) {
    var ka = __rock_call(key_fn, a), kb = __rock_call(key_fn, b);
    return ka < kb ? -1 : (ka > kb ? 1 : 0);
  });
  for (var i = 0; i < n; i++) vec._data[i] = part[i];
}
// `__range(a, b)`: the materialized half-open range `a..b` as a Vec-shaped
// object (0-based `_data[0..len)` = a..b-1), matching the stdlib inline(lua)
// `{_data, len}`.
function __range(a, b) {
  var d = [];
  for (var i = a; i < b; i++) d.push(i);
  return { _data: d, len: b - a };
}
";

/// Runtime-polymorphic untyped table primitives. In Lua a `Map` IS a raw table,
/// so `table_get(m, k)` and `m[k]` are the same op; on JS a `Map` is a real
/// `Map` (keys not string-coerced) while arrays/objects index directly, so these
/// dispatch on `instanceof Map` to keep the two access paths consistent.
pub const JS_TABLE_PRIM: &str = r#"
// `Lit::Omitted`: what a call passes for an omitted argument whose nullable
// parameter has a non-null default (EXPLICIT_NULL_DEFAULT). Every program
// assembly includes these primitives, so the marker lives here.
var __rock_omitted = Object.freeze({});
function __table_get(t, k) { return (t instanceof Map) ? t.get(k) : t[k]; }
function __table_set(t, k, v) { if (t instanceof Map) t.set(k, v); else t[k] = v; }
function __table_unset(t, k) { if (t instanceof Map) t.delete(k); else delete t[k]; }
// Lua `#t`. Defined for a Map and for a hole-free array; a plain object has no
// Lua sequence length, so throw rather than hand back a 0 that reads as empty.
function __table_len(t) {
  if (t instanceof Map) return t.size;
  if (Array.isArray(t)) return t.length;
  throw new Error("table_len of a non-sequence");
}
"#;

/// Map iteration (`Inst::MapNext`, the Lua `k, v = next(t, k)` protocol). JS has
/// no stateless `next`, so `__map_next(m, k)` walks the native Map's keys in
/// INSERTION order (Map order is backend-defined by doctrine -- Lua hash order vs
/// JS insertion order), returning the next key after `k` (null at exhaustion).
/// Backed by a per-map cursor cache (`WeakMap<Map, {it, lastKey}>`): `k === null`
/// starts a fresh `m.keys()` iterator (a NEW loop over the map restarts); `k ===
/// lastKey` is the O(1) common advance; any mismatch (nested / interleaved
/// iteration over the same map, or a cache lost to GC) rescans from a fresh
/// iterator to `k` then advances. Deletion/insertion mid-iteration follows the
/// live JS keys iterator. The value side is read separately (`m.get(k)`), so no
/// per-step pair is allocated.
pub const JS_MAP_ITER_RUNTIME: &str = r#"
var __map_cursor = new WeakMap();
function __map_next(m, k) {
  var c = __map_cursor.get(m);
  if (k === null || k === undefined) {
    c = { it: m.keys(), lastKey: null };
    __map_cursor.set(m, c);
  } else if (!c || c.lastKey !== k) {
    var it = m.keys(), r;
    do { r = it.next(); } while (!r.done && r.value !== k);
    c = { it: it, lastKey: k };
    __map_cursor.set(m, c);
  }
  var r = c.it.next();
  if (r.done) { __map_cursor.delete(m); return null; }
  c.lastKey = r.value;
  return r.value;
}
"#;

/// The uniform indirect-call dispatch helper:
/// Rock values are reusable slot objects, and capturing records carry a slot
/// plus their captures. Raw JS functions remain an explicit foreign boundary.
pub const JS_CALL_HELPER: &str = r#"
var __rock_code_slots = Object.create(null);
var __rock_code_slot_stage = null;
function __rock_code_slot_ref(key) {
  var slot = __rock_code_slots[key];
  if ((!slot || __rock_definition_plan_materializing) && __rock_code_slot_stage !== null) {
    var staged = __rock_code_slot_stage[key];
    if (staged) {
      var temporary = __rock_code_slot_pending_slots[key];
      if (!temporary) {
        temporary = {
          __rock_code_slot: true, key: key,
          current_body: staged.body, abi: staged.abi
        };
        __rock_code_slot_pending_slots[key] = temporary;
      }
      return temporary;
    }
  }
  if (!slot) throw new Error("missing Rock code slot " + key);
  return slot;
}
function __rock_code_slot_pending_ref(key) {
  var staged = __rock_code_slot_stage === null ? null : __rock_code_slot_stage[key];
  if (staged) {
    var temporary = __rock_code_slot_pending_slots[key];
    if (!temporary) {
      temporary = {
        __rock_code_slot: true, key: key,
        current_body: staged.body, abi: staged.abi
      };
      __rock_code_slot_pending_slots[key] = temporary;
    }
    return temporary;
  }
  return __rock_code_slot_ref(key);
}
var __rock_code_slot_pending_slots = Object.create(null);
var __rock_lifecycle_plan_pending = null;
var __rock_definition_plan_pending = null;
var __rock_definition_materializers_pending = null;
var __rock_definition_plan_materializing = false;
function __rock_code_slot_begin() {
  __rock_code_slot_stage = Object.create(null);
  __rock_code_slot_pending_slots = Object.create(null);
  __rock_definition_plan_materializing = false;
  __rock_lifecycle_plan_pending = null;
  __rock_definition_plan_pending = null;
  __rock_definition_materializers_pending = null;
}
function __rock_code_slot_abort() {
  __rock_code_slot_stage = null;
  __rock_code_slot_pending_slots = Object.create(null);
  __rock_definition_plan_materializing = false;
  __rock_lifecycle_plan_pending = null;
  __rock_definition_plan_pending = null;
  __rock_definition_materializers_pending = null;
}
function __rock_lifecycle_plan_stage(plan) {
  if (__rock_code_slot_stage === null) throw new Error("Rock code slot stage not open");
  __rock_lifecycle_plan_pending = plan;
}
function __rock_definition_plan_stage(plan, materializers) {
  if (__rock_code_slot_stage === null) throw new Error("Rock code slot stage not open");
  __rock_definition_plan_pending = plan;
  __rock_definition_materializers_pending = materializers;
}
function __rock_definition_catalog(catalog) {
  var plan = __G.__ROCK_DEFINITION_PLAN;
  if (plan === null || plan === undefined) throw new Error("DefinitionPlan is not published");
  var view = plan[catalog];
  if (view === null || view === undefined) throw new Error("DefinitionPlan catalog " + catalog + " is not published");
  return view;
}
function __rock_lifecycle_wire_due(oldPlan, candidatePlan) {
  var due = Object.create(null);
  var oldPhase = oldPlan && oldPlan.wire;
  var oldEntries = Object.create(null);
  if (oldPhase && oldPhase._data) {
    for (var i = 0; i < oldPhase.len; i++) {
      var oldEntry = oldPhase._data[i];
      if (oldEntry) oldEntries[oldEntry.key] = oldEntry;
    }
  }
  var candidatePhase = candidatePlan && candidatePlan.wire;
  if (!candidatePhase || !candidatePhase._data) return due;
  for (var j = 0; j < candidatePhase.len; j++) {
    var candidate = candidatePhase._data[j];
    var old = candidate && oldEntries[candidate.key];
    var changed = !old || candidate.qualification === "plain";
    if (!changed && candidate.qualification !== old.qualification) changed = true;
    if (!changed && candidate.body_fingerprint !== old.body_fingerprint) changed = true;
    if (!changed) {
      var candidateDeps = candidate.dependencies;
      var oldDeps = old.dependencies;
      if (!candidateDeps || !oldDeps || candidateDeps.len !== oldDeps.len) {
        changed = true;
      } else {
        for (var k = 0; k < candidateDeps.len; k++) {
          var candidateDep = candidateDeps._data[k];
          var oldDep = oldDeps._data[k];
          if (!candidateDep || !oldDep
              || candidateDep.key !== oldDep.key
              || candidateDep.fingerprint !== oldDep.fingerprint) {
            changed = true;
            break;
          }
        }
      }
    }
    if (changed) due[candidate.key] = true;
  }
  return due;
}
function __rock_lifecycle_schedule_all_wires() {
  __G.__ROCK_WIRE_DUE = __rock_lifecycle_wire_due(null, __G.__ROCK_LIFECYCLE_PLAN);
}
function __rock_lifecycle_take_wire(key) {
  var due = __G.__ROCK_WIRE_DUE;
  if (!due || !due[key]) return false;
  delete due[key];
  return true;
}
function __rock_definition_plan_materialize(plan, materializers) {
  __rock_definition_plan_materializing = true;
  Object.keys(plan).forEach(function(catalogKey) {
    var catalog = plan[catalogKey];
    if (!catalog || !catalog._data || catalog.len === undefined) return;
    for (var i = 0; i < catalog.len; i++) {
      var entry = catalog._data[i];
      if (entry && entry.factory !== undefined) {
        if (typeof entry.factory === "function") {
          // Explicit foreign factories already return one boxed result.
          entry.definition = entry.factory();
        } else {
          var materialize = materializers && materializers[entry.factory.key];
          if (typeof materialize !== "function") throw new Error("DefinitionPlan factory has no ABI-aware materializer");
          entry.definition = materialize(entry.factory);
        }
      }
    }
  });
  __rock_definition_plan_materializing = false;
  return plan;
}
function __rock_code_slot_stage_one(key, body, abi) {
  var old = __rock_code_slots[key];
  if (old && old.abi !== abi) throw new Error("incompatible Rock code slot " + key);
  if (__rock_code_slot_stage === null) throw new Error("Rock code slot stage not open");
  __rock_code_slot_stage[key] = { body: body, abi: abi };
}
function __rock_code_slot_publish() {
  if (__rock_code_slot_stage === null) throw new Error("Rock code slot stage not open");
  var definition_plan = __rock_definition_plan_pending;
  if (definition_plan !== null) definition_plan = __rock_definition_plan_materialize(definition_plan, __rock_definition_materializers_pending);
  var candidateLifecyclePlan = __rock_lifecycle_plan_pending;
  var wireDue = null;
  if (candidateLifecyclePlan !== null) {
    wireDue = __rock_lifecycle_wire_due(__G.__ROCK_LIFECYCLE_PLAN, candidateLifecyclePlan);
  }
  var stage = __rock_code_slot_stage;
  Object.keys(stage).forEach(function(key) {
    var staged = stage[key], slot = __rock_code_slots[key];
    if (slot) {
      slot.current_body = staged.body;
      slot.abi = staged.abi;
      var temporary = __rock_code_slot_pending_slots[key];
      if (temporary) {
        temporary.current_body = staged.body;
        temporary.abi = staged.abi;
        __rock_code_slots[key] = temporary;
      }
    } else {
      __rock_code_slots[key] = __rock_code_slot_pending_slots[key] || {
        __rock_code_slot: true, key: key,
        current_body: staged.body, abi: staged.abi
      };
    }
  });
  if (__rock_lifecycle_plan_pending !== null) __G.__ROCK_LIFECYCLE_PLAN = __rock_lifecycle_plan_pending;
  if (definition_plan !== null) __G.__ROCK_DEFINITION_PLAN = definition_plan;
  __rock_code_slot_stage = null;
  __rock_code_slot_pending_slots = Object.create(null);
  __rock_lifecycle_plan_pending = null;
  __rock_definition_plan_pending = null;
  __rock_definition_materializers_pending = null;
  if (wireDue !== null) __G.__ROCK_WIRE_DUE = wireDue;
}
function __rock_code_ref_kind(v) {
  if (v === null || typeof v !== "object") return null;
  if (v.__rock_code_slot === true
      && Object.prototype.hasOwnProperty.call(v, "key")
      && Object.prototype.hasOwnProperty.call(v, "current_body")) return "slot";
  if (Object.prototype.hasOwnProperty.call(v, "__slot")
      && __rock_code_ref_kind(v.__slot) === "slot") return "env";
  return null;
}
function __rock_call(f) {
  var a = Array.prototype.slice.call(arguments, 1);
  var kind = __rock_code_ref_kind(f);
  if (kind === "slot") return f.current_body.apply(null, a);
  if (typeof f === "function") return f.apply(null, a);
  if (kind === "env") return f.__slot.current_body.apply(null, [f].concat(a));
  throw new Error("cannot call a value that is not a Rock CodeRef or foreign function");
}
// `try_call(f)`: run `f` and report whether it errored (Lua `pcall`; returns
// `not ok`). A JS throw is the pcall failure; the error itself is discarded by
// design (the caller only wants the did-it-error bool, matching the stdlib body).
function __try_call(f) {
  try { __rock_call(f); return false; } catch (e) { return true; }
}
"#;

/// The explicit foreign-function boundary used when an artifact has indirect
/// calls but no escaping Rock CodeRefs.
pub const JS_FOREIGN_CALL_HELPER: &str = r#"
function __rock_call(f) {
  var a = Array.prototype.slice.call(arguments, 1);
  if (typeof f === "function") return f.apply(null, a);
  throw new Error("cannot call a value that is not a foreign function");
}
function __try_call(f) {
  try { __rock_call(f); return false; } catch (e) { return true; }
}
"#;

/// The `__op_eq` runtime: deep structural equality matching Rock's stdlib
/// `__op_eq` (verified on luajit: reference structs, value structs, tuples,
/// enums, newtypes all compare BY CONTENTS; primitives by value). Lua `nil` maps
/// to both JS `null` and `undefined` (an absent arg / an array hole), so both
/// count as nil. A Vec compares its live logical storage, not reserved capacity
/// or representation metadata, so a drained tail slot is invisible.
/// Structs are `{__ty, ...}` plain objects (the
/// `__rock_create_struct` shape: a `__ty` string tag, no metatable), so a
/// key-walk that includes `__ty` covers the type check for free. A capturing
/// lambda is a CodeRef env record `{ __slot, .. }` (`convert_closures`): a callable
/// value, so -- like a raw foreign fn -- it compares by IDENTITY, never by walking its
/// captures (matching luajit's `__rock_fn_mt` identity-leaf rule and pre-arc
/// arrow `===`).
pub const JS_STRUCTURAL_EQ: &str = r#"
// Per-type equality for GC-carrying value structs (`build_vt_eq_defs` fills it):
// GC-ref fields by reference, scalar/nested-value fields structural. An entry is
// the AUTHORITY, mirroring luajit's `_vt.__eq` metatable rule -- consulted before
// the generic key walk so a Vec/Map/ref-struct field is NOT contents-compared.
var __rock_vt_eq = {};
function __rock_native_buffer_prefix_eq(a, b, n) {
  if (n <= 0) return true;
  var ap = a.ptr, bp = b.ptr;
  if (ap == null || bp == null) return ap === bp;
  var as = ap.__rock_elem_size === undefined ? ap.BYTES_PER_ELEMENT : ap.__rock_elem_size;
  var bs = bp.__rock_elem_size === undefined ? bp.BYTES_PER_ELEMENT : bp.__rock_elem_size;
  if (as !== bs) return false;
  var av = new Uint8Array(ap.buffer, ap.byteOffset, n * as);
  var bv = new Uint8Array(bp.buffer, bp.byteOffset, n * bs);
  for (var i = 0; i < av.length; i++) if (av[i] !== bv[i]) return false;
  return true;
}
function __op_eq(a, b) {
  var an = (a === null || a === undefined), bn = (b === null || b === undefined);
  if (an || bn) return an && bn;
  if (a === b) return true;
  if (typeof a !== "object" || typeof b !== "object") return a === b;
  if (typeof __rock_code_ref_kind === "function" &&
      (__rock_code_ref_kind(a) !== null || __rock_code_ref_kind(b) !== null)) return a === b;
  // C buffers compare bytes, including floating-point bit patterns and padding,
  // just as LuaJIT does. FlatVec's buffer length is capacity, not live length.
  if (a.__ty === "FlatVec" || b.__ty === "FlatVec") {
    if (a.__ty !== "FlatVec" || b.__ty !== "FlatVec" || a.len !== b.len) return false;
    return __rock_native_buffer_prefix_eq(a.buf, b.buf, a.len);
  }
  if (a.__ty === "NativeBuffer" || b.__ty === "NativeBuffer") {
    if (a.__ty !== "NativeBuffer" || b.__ty !== "NativeBuffer" || a.len !== b.len) return false;
    return __rock_native_buffer_prefix_eq(a, b, a.len);
  }
  // A Vec compares live storage only. Each concrete element type has one
  // compiler-selected representation, so unlike representations cannot be
  // values of the same concrete Vec type.
  if (a.__ty === "Vec" || b.__ty === "Vec") {
    if (a.__ty !== "Vec" || b.__ty !== "Vec" || a.len !== b.len) return false;
    var ah = a["@vec"], bh = b["@vec"];
    var ar = ah !== null && typeof ah === "object" ? ah.repr : "boxed";
    var br = bh !== null && typeof bh === "object" ? bh.repr : "boxed";
    if (ar !== br) return false;
    if (ar === "strided") {
      if (ah.elem !== bh.elem || ah.stride !== bh.stride) return false;
      for (var si = 0; si < a.len * ah.stride; si++) {
        if (!__op_eq(a._data[si], b._data[si])) return false;
      }
      return true;
    }
    if (ar === "columns") {
      if (ah.elem !== bh.elem) return false;
      var ak = Object.keys(a).filter(function(k) { return k !== "len" && k !== "__ty" && k !== "@vec"; });
      var bk = Object.keys(b).filter(function(k) { return k !== "len" && k !== "__ty" && k !== "@vec"; });
      if (ak.length !== bk.length) return false;
      for (var ci = 0; ci < ak.length; ci++) {
        var ck = ak[ci];
        if (!Array.isArray(a[ck]) || !Array.isArray(b[ck])) return false;
        for (var cj = 0; cj < a.len; cj++) if (!__op_eq(a[ck][cj], b[ck][cj])) return false;
      }
      return true;
    }
    if (!Array.isArray(a._data) || !Array.isArray(b._data)) return false;
    for (var vi = 0; vi < a.len; vi++) if (!__op_eq(a._data[vi], b._data[vi])) return false;
    return true;
  }
  var aa = Array.isArray(a), ab = Array.isArray(b);
  if (aa || ab) {
    if (!aa || !ab || a.length !== b.length) return false;
    for (var i = 0; i < a.length; i++) if (!__op_eq(a[i], b[i])) return false;
    return true;
  }
  // A JS `Map` (Rock's hash `Map`) has no own enumerable keys, so the generic
  // key walk below would compare EVERY pair of Maps equal. Compare by size then
  // per-key `.get` -- order-free (no reliance on iteration order / MapNext).
  var am = (a instanceof Map), bm = (b instanceof Map);
  if (am || bm) {
    if (!am || !bm || a.size !== b.size) return false;
    var eq = true;
    a.forEach(function(v, k) { if (eq && !(b.has(k) && __op_eq(v, b.get(k)))) eq = false; });
    return eq;
  }
  var vt = a.__ty;
  if (vt !== undefined && vt === b.__ty && __rock_vt_eq[vt] !== undefined) {
    return __rock_vt_eq[vt](a, b);
  }
  var ka = Object.keys(a), kb = Object.keys(b);
  if (ka.length !== kb.length) return false;
  for (var i = 0; i < ka.length; i++) { var k = ka[i]; if (!__op_eq(a[k], b[k])) return false; }
  return true;
}
"#;

/// The boxed-enum ctor (T8e). `MakeEnum`/`MakeEnumDyn` box a non-cdata enum as
/// `{__variant, __ty, 0:.., 1:.., <field>:..}` -- native 0-based positional keys
/// (luajit uses 1-based table keys; the observable output text is unchanged). The
/// `kv` flat `[key, val, ..]` list drops NULL/UNDEFINED slots: a `MakeEnumDyn`
/// union-wide rebox leaves the inactive tuple/record slots nil, and luajit's table
/// constructor omits nil entries -- so a directly-constructed box (active keys
/// only) and a reboxed box of the same value must own the SAME keys, or
/// `__op_eq`'s key-count walk (`JS_STRUCTURAL_EQ`) would call them unequal.
pub const JS_ENUM_RUNTIME: &str = r#"
function __rock_make_enum(variant, ty, kv) {
  var o = { __variant: variant, __ty: ty };
  for (var i = 0; i < kv.length; i += 2) {
    var v = kv[i + 1];
    if (v !== null && v !== undefined) o[kv[i]] = v;
  }
  return o;
}
"#;

/// `inspect` / `inspect_formatted`: a faithful port of `__rock_inspect_value`
/// (stdlib/unified.rock). Adapts to the JS shapes: a struct/enum `{__ty, ...}`
/// object (keys sorted by `lua_tostring`, `__ty`/`__variant` skipped), a Vec
/// `{_data (0-based), len, __ty:"Vec"}`, a JS `Map` (raw-table
/// analog), a JS array (Lua array-table analog), and scalars. A capturing-lambda
/// CodeRef env record (`__slot`) is an opaque callable LEAF (`String(fn)`), matching the
/// luajit `__rock_fn_mt` rule; a `visited` Map detects cycles. NOTE: most inspect
/// tests are VACUOUS -- a `@test(output=..)` whose expected contains `{` is an
/// interpolated string, so its expectation is dropped (`out=nil`) and the test
/// only checks that inspect RUNS; the byte-checked cases are the brace-free
/// scalars (`"str"`, numbers, bools). Compact/multiline threshold + key sort
/// reproduce the Lua renderer for the cross-backend `inspect(x) == "..."` pins.
///
/// A struct renders through `__rock_inspect_ty[<__ty>]`, a per-type function
/// `build_inspect_ty_defs` emits for every realized struct and declared child: it
/// supplies the DECLARED fields, because a null field is an absent key and a key walk alone
/// therefore under-reports the shape, and an `embed` field is stored as flat
/// `<name>.<leaf>` slots that carry no declared name at all. That is luajit's
/// `_G.__ROCK_TYPE_TABLE` lookup replaced by compile-time-emitted dispatch
/// -- no universal type table on JS.
pub const JS_INSPECT_RUNTIME: &str = r#"
var __rock_inspect_ty = {};
var __rock_col_mat = {};
var __rock_enum_inspect_payloads = Object.create(null);
// A column-stored Vec has no `_data`: one plain array per leaf, sharing the
// Vec's `len`. Rendering its own keys would show the PHYSICAL layout (`@vec`,
// `x = { 1 }`), so materialize each element first -- luajit's
// `__rock_vec_column_elements`, over emitted per-type rebuilders instead of a
// runtime type table.
function __rock_col_header(t) {
  var h = t["@vec"];
  return (h !== null && typeof h === "object" && h.repr === "columns") ? h : null;
}
function __rock_stride_header(t) {
  var h = t["@vec"];
  return (h !== null && typeof h === "object" && h.repr === "strided") ? h : null;
}
function __rock_stride_element(t, h, index) {
  var base = index * h.stride;
  var out = {__variant: t._data[base], __ty: h.elem};
  for (var j = 1; j < h.stride; j++) {
    var value = t._data[base + j];
    if (value !== false) out[j - 1] = value;
  }
  return out;
}
function __rock_inspect_key_lt(a, b) {
  var sa = lua_tostring(a), sb = lua_tostring(b);
  return sa < sb ? -1 : (sa > sb ? 1 : 0);
}
// Reference-struct tier: declared fields (already sorted), plus any own key the
// declaration does not name -- minus the compiler-minted PHYSICAL slots, which
// are exactly the dotted keys (`embed` leaves, a flattened enum's `.#`/`.0`).
function __rock_inspect_struct(header, t, declared, indent, path, visited, precision) {
  var pairs = declared.slice();
  var seen = {};
  for (var i = 0; i < declared.length; i++) seen["$" + declared[i][0]] = true;
  var extra = false;
  for (var k in t) {
    if (Object.prototype.hasOwnProperty.call(t, k) && k !== "__ty" && k !== "__variant" &&
        seen["$" + k] !== true && k.indexOf(".") === -1) {
      pairs.push([k, t[k]]);
      extra = true;
    }
  }
  if (extra) pairs.sort(function(a, b) { return __rock_inspect_key_lt(a[0], b[0]); });
  if (pairs.length <= 2) {
    var cparts = pairs.map(function(kv) {
      return kv[0] + " = " + __rock_inspect_value(kv[1], indent, path + "." + kv[0], visited, precision);
    });
    return header + " { " + cparts.join(", ") + " }";
  }
  var lines = pairs.map(function(kv) {
    return indent + "  " + kv[0] + " = " + __rock_inspect_value(kv[1], indent + "  ", path + "." + kv[0], visited, precision);
  });
  return header + " {\n" + lines.join(",\n") + "\n" + indent + "}";
}
// FFI-clean `value` struct tier: luajit renders those from `__ROCK_C_STRUCT`
// (a cdata, not a table), which is a different renderer -- `: ` separators,
// declaration order, always one line, children through the top-level `inspect`.
function __rock_inspect_value_struct(header, pairs) {
  if (pairs.length === 0) return header + " { }";
  var parts = pairs.map(function(kv) { return kv[0] + ": " + inspect(kv[1]); });
  return header + " { " + parts.join(", ") + " }";
}
// Recover names for a tuple payload widened from a direct value-record leaf
// group. Keep this exact: a missing, extra, sparse, or colliding key makes the
// whole enum payload fall back to its positional labels.
function __rock_enum_payload_names(pairs, positions) {
  var max = -1, numeric = 0;
  var seen = Object.create(null);
  for (var i = 0; i < pairs.length; i++) {
    var key = String(pairs[i][0]);
    if (/^(0|[1-9][0-9]*)$/.test(key)) {
      var index = Number(key);
      if (!Number.isSafeInteger(index)) return null;
      numeric++;
      if (index > max) max = index;
    } else {
      seen["$" + key] = true;
    }
  }
  if (numeric === 0 || numeric !== max + 1) return null;
  var expected = 0;
  for (var p = 0; p < positions.length; p++) {
    var fields = positions[p];
    if (fields === null) {
      expected++;
    } else if (Array.isArray(fields) && fields.length > 0) {
      expected += fields.length;
    } else {
      return null;
    }
  }
  if (numeric !== expected) return null;
  var names = new Array(numeric);
  var slot = 0;
  for (var p = 0; p < positions.length; p++) {
    var fields = positions[p];
    if (fields === null) {
      slot++;
      continue;
    }
    for (var j = 0; j < fields.length; j++) {
      var name = fields[j];
      if (typeof name !== "string" || seen["$" + name] === true) return null;
      seen["$" + name] = true;
      names[slot++] = name;
    }
  }
  if (slot !== numeric) return null;
  return names;
}
function __rock_inspect_value(v, indent, path, visited, precision) {
  if (v === null || v === undefined) return "nil";
  var vt = typeof v;
  if (vt === "string") return "\"" + v + "\"";
  if (vt === "number") {
    if (precision >= 0 && v !== Math.floor(v)) return __c_f(v, precision);
    return lua_tostring(v);
  }
  if (vt === "boolean") return lua_tostring(v);
  // A raw fn value renders as Lua's `tostring(function)` form (`function: ..`);
  // the exact address is never asserted (tests check the `function: ` prefix).
  if (vt === "function") return "function: builtin";
  if (vt !== "object") return lua_tostring(v);
  var t = v;
  // A capturing-lambda env record is a callable LEAF (never walked) -- rendered
  // as its function, same `function: ` form as a raw fn.
  if (typeof __rock_code_ref_kind === "function" && __rock_code_ref_kind(t) !== null) return "function: builtin";
  if (visited) {
    if (visited.has(t)) return "<CYCLE: " + visited.get(t) + ">";
    visited.set(t, path);
  }
  var type_name = t.__ty;
  var colh = type_name === "Vec" && t.len !== undefined ? __rock_col_header(t) : null;
  var strideh = type_name === "Vec" && t.len !== undefined ? __rock_stride_header(t) : null;
  if (type_name === "Vec" && t.len !== undefined && (t._data || colh || strideh)) {
    var mat = colh ? __rock_col_mat[colh.elem] : null;
    if (colh && mat === undefined) {
      throw new Error("no column materializer emitted for element type " + colh.elem);
    }
    var vparts = [];
    for (var i = 0; i < t.len; i++) {
      var elem = colh ? mat(t, i) : strideh ? __rock_stride_element(t, strideh, i) : t._data[i];
      vparts.push(__rock_inspect_value(elem, indent, path + "[" + i + "]", visited, precision));
    }
    if (visited) visited.delete(t);
    if (vparts.length === 0) return "Vec { }";
    return "Vec { " + vparts.join(", ") + " }";
  }
  if (type_name !== undefined && t.__variant === undefined &&
      __rock_inspect_ty[type_name] !== undefined) {
    var sres = __rock_inspect_ty[type_name](t, indent, path, visited, precision);
    if (visited) visited.delete(t);
    return sres;
  }
  if (Array.isArray(t)) {
    if (visited) visited.delete(t);
    if (t.length === 0) return "{}";
    var aparts = [];
    for (var j = 0; j < t.length; j++) {
      aparts.push(__rock_inspect_value(t[j], indent, path + "[" + j + "]", visited, precision));
    }
    return "{ " + aparts.join(", ") + " }";
  }
  // Keyed case: struct / enum record / Map. Collect (key, value) pairs.
  var pairs = [];
  if (t instanceof Map) {
    t.forEach(function(vv, k) { pairs.push([k, vv]); });
  } else {
    for (var k in t) {
      if (Object.prototype.hasOwnProperty.call(t, k) &&
          k !== "__variant" && k !== "__ty" && k.indexOf(".") === -1) {
        pairs.push([k, t[k]]);
      }
    }
  }
  pairs.sort(function(a, b) { return __rock_inspect_key_lt(a[0], b[0]); });
  var variant = t.__variant;
  var enumBox = variant !== undefined;
  var payloadNames = null;
  if (enumBox && type_name !== undefined) {
    var enumPayloads = __rock_enum_inspect_payloads[type_name];
    var positions = enumPayloads && enumPayloads[variant];
    if (positions !== undefined) payloadNames = __rock_enum_payload_names(pairs, positions);
  }
  if (type_name !== undefined && variant !== undefined && pairs.length === 0) {
    if (visited) visited.delete(t);
    return type_name + "::" + variant;
  }
  var header = (type_name !== undefined && variant !== undefined) ? (type_name + "::" + variant)
    : (type_name !== undefined ? type_name : "");
  var result;
  // A boxed enum's positional payload takes native 0-based keys ("0".., for `==`
  // and `TupleIndex`); luajit's box uses 1-based array keys and inspect renders
  // them `[1]`.. So display an enum-box numeric key as `[<n+1>]` -- byte-parity
  // with luajit over a 0-based runtime. A proven flat value-record payload
  // overrides that label with its declared field name.
  var keyStr = function(k) {
    if (enumBox && typeof k === "string" && /^(0|[1-9][0-9]*)$/.test(k)) {
      var index = parseInt(k, 10);
      if (payloadNames && payloadNames[index] !== undefined) return payloadNames[index];
      return "[" + (index + 1) + "]";
    }
    return typeof k === "string" ? k : ("[" + lua_tostring(k) + "]");
  };
  var pathKey = function(k) {
    if (enumBox && typeof k === "string" && /^(0|[1-9][0-9]*)$/.test(k)) {
      var index = parseInt(k, 10);
      if (payloadNames && payloadNames[index] !== undefined) return payloadNames[index];
    }
    return lua_tostring(k);
  };
  if (pairs.length <= 2) {
    var cparts = pairs.map(function(kv) {
      return keyStr(kv[0]) + " = " + __rock_inspect_value(kv[1], indent, path + "." + pathKey(kv[0]), visited, precision);
    });
    result = header !== "" ? header + " { " + cparts.join(", ") + " }" : "{ " + cparts.join(", ") + " }";
  } else {
    var lines = pairs.map(function(kv) {
      return indent + "  " + keyStr(kv[0]) + " = " + __rock_inspect_value(kv[1], indent + "  ", path + "." + pathKey(kv[0]), visited, precision);
    });
    var body = lines.join(",\n");
    result = header !== "" ? header + " {\n" + body + "\n" + indent + "}" : "{\n" + body + "\n" + indent + "}";
  }
  if (visited) visited.delete(t);
  return result;
}
function inspect(obj, cycle_detect, precision) {
  if (cycle_detect === undefined) cycle_detect = true;
  if (precision === undefined) precision = -1;
  var ot = typeof obj;
  if (ot !== "object" || obj === null) {
    if (ot === "string") return "\"" + obj + "\"";
    if (ot === "function") return "function: builtin";
    if (ot === "number" && precision >= 0 && obj !== Math.floor(obj)) return __c_f(obj, precision);
    return lua_tostring(obj);
  }
  var visited = cycle_detect ? new Map() : null;
  return __rock_inspect_value(obj, "", "root", visited, precision);
}
function inspect_formatted(obj, precision) { return inspect(obj, true, precision); }
"#;

/// `deepcopy`: the recursive `__rock_deepcopy_value` clone (stdlib/unified.rock).
/// A `seen` Map (object identity) preserves the graph -- a shared reference copies
/// ONCE, a cycle terminates. Primitives and bare functions pass through by identity;
/// a TypedArray (NativeBuffer `.ptr` or a leaf view) is rebuilt over a copy of its
/// `ArrayBuffer`, copied once per buffer, so a value-struct buffer's leaf views
/// alias the copy's `.ptr` as they alias the original's; a `Map` and an array (a Vec's
/// `_data`) copy element-wise; every other object (a struct `{__ty,...}`, a CodeRef
/// env record `{__slot,...}`, a box) copies own keys. A copied env record keeps its
/// logical slot and copies its captures, so calls remain fresh while mutable boxes
/// do not alias the source.
pub const JS_DEEPCOPY_RUNTIME: &str = r#"
function __rock_deepcopy_value(obj, seen) {
  if (obj === null || obj === undefined) return obj;
  var t = typeof obj;
  if (t !== "object" && t !== "function") return obj;
  if (t === "function") return obj;
  if (typeof __rock_code_ref_kind === "function" && __rock_code_ref_kind(obj) === "slot") return obj;
  if (seen.has(obj)) return seen.get(obj);
  if (ArrayBuffer.isView(obj)) {
    var bc = seen.get(obj.buffer);
    if (bc === undefined) { bc = obj.buffer.slice(0); seen.set(obj.buffer, bc); }
    var tc = new obj.constructor(bc, obj.byteOffset, obj.byteLength / (obj.BYTES_PER_ELEMENT || 1));
    if (obj.__rock_elem_size !== undefined) tc.__rock_elem_size = obj.__rock_elem_size;
    seen.set(obj, tc);
    return tc;
  }
  if (obj instanceof Map) {
    var mc = new Map();
    seen.set(obj, mc);
    obj.forEach(function(v, k) { mc.set(__rock_deepcopy_value(k, seen), __rock_deepcopy_value(v, seen)); });
    return mc;
  }
  if (Array.isArray(obj)) {
    var ac = [];
    seen.set(obj, ac);
    for (var i = 0; i < obj.length; i++) ac[i] = __rock_deepcopy_value(obj[i], seen);
    return ac;
  }
  var oc = {};
  seen.set(obj, oc);
  for (var k in obj) if (Object.prototype.hasOwnProperty.call(obj, k)) oc[k] = __rock_deepcopy_value(obj[k], seen);
  return oc;
}
function deepcopy(obj) { return __rock_deepcopy_value(obj, new Map()); }
"#;

/// The logging runtime: JS ports of the `inline(lua)` dedup/throttle
/// impls in `stdlib/logging.rock`. Rendered by their original names
/// (`is_host_log_fn`), so `log_once`/`log_throttled` (macros -> `_log_once_impl`
/// / `_log_throttled_*_impl`) route here. `__rock_log_emit` hands records to
/// `__G.__rock_log_sink` when one is installed (the JS face of Lua's
/// `_G.__rock_log_sink` seam); its no-sink fallback is `print("[cat] msg")`,
/// so lines land in the same per-test buffer as `print`.
/// The one Rock global (`__rock_log_level`) lives in `__G` (unified with the
/// module-globals path); dedup/throttle/counts/sites/now are bare vars (never
/// Rock globals). `_log_once_impl` COUNTS rather than flagging, and the throttle
/// impl records its own counts, because `log_suppression_summary` reports the
/// totals -- a boolean dedup cannot answer "this site fired 38k times".
/// `__rock_log_now` uses `Date.now()` as the os.clock() analog (the fallback
/// test uses a huge interval, so the absolute value never matters).
pub const JS_LOG_RUNTIME: &str = r#"
function __rock_log_init() {
  if (__G.__rock_log_level == null || __G.__rock_log_level < 0) __G.__rock_log_level = 2;
}
function __rock_log_emit(level, category, msg, wall_clock) {
  var sink = __G.__rock_log_sink;
  if (sink != null) { sink(level, category || "", msg, wall_clock); return; }
  if (category && category.length > 0) print("[" + category + "] " + msg);
  else print(msg);
}
function __rock_log(level, category, msg) {
  __rock_log_init();
  if (level > __G.__rock_log_level) return;
  __rock_log_emit(level, category, msg, false);
}
function __rock_log_wall_clock(level, category, msg) {
  __rock_log_init();
  if (level > __G.__rock_log_level) return;
  __rock_log_emit(level, category, msg, true);
}
function log_set_level(level) { __G.__rock_log_level = level; }
// `__rock_log_dedup` holds a COUNT, not a flag: `log_suppression_summary`
// reports how many times a suppressed site fired, and a site that fired 38k
// times must not read like one that fired once.
var __rock_log_dedup = {};
var __rock_log_counts = {};
var __rock_log_sites = {};
var __rock_log_site_levels = {};
var __rock_log_summary_done = false;
var __rock_log_throttle = {};
var __rock_log_wall = {};
var __rock_log_now_override = null;
function _log_once_impl(level, category, msg, site) {
  __rock_log_init();
  if (level > __G.__rock_log_level) return;
  var n = (__rock_log_dedup[site] || 0) + 1;
  __rock_log_dedup[site] = n;
  __rock_log_sites[site] = (category || "") + "|" + msg;
  __rock_log_site_levels[site] = level;
  if (n > 1) return;
  __rock_log_emit(level, category, msg, false);
}
function _log_throttled_with_now_impl(now_sec, level, category, msg, interval_sec, site, wall_clock) {
  __rock_log_init();
  if (level > __G.__rock_log_level) return;
  __rock_log_counts[site] = (__rock_log_counts[site] || 0) + 1;
  __rock_log_sites[site] = (category || "") + "|" + msg;
  __rock_log_site_levels[site] = level;
  // `= nil` in the luajit sibling, which removes the key; keep the two tables
  // the same shape rather than storing a false.
  if (wall_clock) __rock_log_wall[site] = true;
  else delete __rock_log_wall[site];
  var last = __rock_log_throttle[site];
  if (last != null && (now_sec - last) < interval_sec) return;
  __rock_log_throttle[site] = now_sec;
  __rock_log_emit(level, category, msg, wall_clock);
}
function log_suppression_summary() {
  __rock_log_init();
  if (__rock_log_summary_done) return;
  __rock_log_summary_done = true;
  var rows = [];
  var collect = function(tbl) {
    for (var site in tbl) {
      if (Object.prototype.hasOwnProperty.call(tbl, site) && tbl[site] > 1) {
        rows.push({ site: site, n: tbl[site] });
      }
    }
  };
  collect(__rock_log_dedup);
  collect(__rock_log_counts);
  if (rows.length === 0) return;
  rows.sort(function(a, b) { return b.n - a.n || (a.site < b.site ? -1 : a.site > b.site ? 1 : 0); });
  for (var i = 0; i < rows.length; i++) {
    var r = rows[i];
    var rec = __rock_log_sites[r.site] || "|";
    var bar = rec.indexOf("|");
    var level = __rock_log_site_levels[r.site] || 0;
    if (level > __G.__rock_log_level) continue;
    __rock_log_emit(level, rec.slice(0, bar),
      "suppressed x" + r.n + " at " + r.site + ": " + rec.slice(bar + 1),
      __rock_log_wall[r.site] === true);
  }
}
function log_set_now(now_sec) { __rock_log_now_override = now_sec; }
function log_clear_now() { __rock_log_now_override = null; }
function __rock_log_now() {
  if (__rock_log_now_override != null) return __rock_log_now_override;
  return Date.now() / 1000;
}
function _log_throttled_impl(level, category, msg, interval_sec, site, wall_clock) {
  _log_throttled_with_now_impl(__rock_log_now(), level, category, msg, interval_sec, site, wall_clock);
}
function log_reset_dedup() {
  __rock_log_dedup = {}; __rock_log_throttle = {}; __rock_log_wall = {};
  __rock_log_counts = {}; __rock_log_sites = {}; __rock_log_site_levels = {};
  __rock_log_summary_done = false;
}
function log_dedup_count() { return Object.keys(__rock_log_dedup).length; }
"#;

/// The RNG runtime mirrors LuaJIT's 32-bit state transition: add `0x9E3779B9`,
/// apply xorshift 13/17/5, and return the unsigned state XOR its logical shift
/// by 16. State is stored as a signed 32-bit word, while draws are unsigned.
/// The fn surface provides `__rock_rng_seed`, `__rock_rng_state_get`/`_set` for
/// gameplay, and `__rock_presentation_rng_*` for presentation. Seeding is done
/// by [`JS_HOST_RUNTIME`] (it reads `ROCK_TEST_SEED` through the host), not here.
/// `random()` draws one 32-bit word / 2^32 in [0,1); `random_int(min,max)`
/// is `[min,max)` exclusive (`r=max-min`, `r<=0 -> min`), single draw for
/// `r<=2^32` else two draws composed into 53 bits then `% r` (range coverage is
/// a semantic, not a nicety).
pub const JS_RNG_RUNTIME: &str = r#"
var __rock_rng_state = 0;
function __rock_rng_current() { return __rock_rng_state; }
function __rock_rng_store(s) { __rock_rng_state = s | 0; }
function __rock_rng_seed(s) { __rock_rng_store(s); }
function __rock_rng_state_get() { return __rock_rng_current(); }
function __rock_rng_state_set(s) { __rock_rng_store(s); }
function __rock_rng_next32() {
  var state = (__rock_rng_current() + 0x9E3779B9) | 0;
  state = (state ^ (state << 13)) | 0;
  state = (state ^ (state >>> 17)) | 0;
  state = (state ^ (state << 5)) | 0;
  __rock_rng_store(state);
  return (state ^ (state >>> 16)) >>> 0;
}
function __rock_random() { return __rock_rng_next32() / 4294967296; }
function __rock_random_int(min, max) {
  var r = max - min;
  if (r <= 0) return min;
  if (r <= 4294967296) return min + (__rock_rng_next32() % r);
  var hi = __rock_rng_next32(), lo = __rock_rng_next32();
  return min + ((hi * 2097152 + (lo >>> 11)) % r);
}
var __rock_presentation_rng_state = 0;
function __rock_presentation_rng_current() { return __rock_presentation_rng_state; }
function __rock_presentation_rng_store(s) { __rock_presentation_rng_state = s | 0; }
function __rock_presentation_rng_seed(s) { __rock_presentation_rng_store(s); }
function __rock_presentation_rng_state_get() { return __rock_presentation_rng_current(); }
function __rock_presentation_rng_state_set(s) { __rock_presentation_rng_store(s); }
function __rock_presentation_rng_next32() {
  var state = (__rock_presentation_rng_current() + 0x9E3779B9) | 0;
  state = (state ^ (state << 13)) | 0;
  state = (state ^ (state >>> 17)) | 0;
  state = (state ^ (state << 5)) | 0;
  __rock_presentation_rng_store(state);
  return (state ^ (state >>> 16)) >>> 0;
}
function __rock_presentation_random() { return __rock_presentation_rng_next32() / 4294967296; }
function __rock_presentation_random_int(min, max) {
  var r = max - min;
  if (r <= 0) return min;
  if (r <= 4294967296) return min + (__rock_presentation_rng_next32() % r);
  var hi = __rock_presentation_rng_next32(), lo = __rock_presentation_rng_next32();
  return min + ((hi * 2097152 + (lo >>> 11)) % r);
}
"#;

pub const JS_GLOBALS_RUNTIME: &str = r#"
// Mutable globals and statics live in one object per runtime, created by the
// first full program and never replaced, so an incremental overlay runs against
// the values the program wrote. A program whose global layout differs from the
// one that created it cannot reuse it: its keys would name other globals.
var __rock_globals = null;
var __rock_globals_layout = null;
var __rock_globals_fresh = false;
function __rock_globals_open(layout) {
  if (__rock_globals === null) {
    __rock_globals = Object.create(null);
    __rock_globals_layout = layout;
    __rock_globals_fresh = true;
  } else if (__rock_globals_layout !== layout) {
    throw new Error("Rock global layout changed; restart required");
  } else {
    __rock_globals_fresh = false;
  }
  return __rock_globals;
}
// The global directory: one entry per mutable global,
// { name, identity, physical: [leaf names], slots: [storage keys] }.
var __ROCK_GLOBALS = [];
function __rock_globals_publish(directory) {
  __ROCK_GLOBALS = directory;
  __rock_globals_install_bridge();
}
// By name, identity or physical leaf name.
function __rock_global_entry(name) {
  for (var i = 0; i < __ROCK_GLOBALS.length; i++) {
    var entry = __ROCK_GLOBALS[i];
    if (entry.name === name || entry.identity === name) return entry;
    for (var j = 0; j < entry.physical.length; j++) {
      if (entry.physical[j] === name) return entry;
    }
  }
  return null;
}
function __rock_global_get(entry) {
  if (entry.slots.length === 1) return __rock_globals[entry.slots[0]];
  throw new Error("Rock global " + entry.identity + " has no single-value accessor");
}
function __rock_global_set(entry, value) {
  if (entry.slots.length === 1) { __rock_globals[entry.slots[0]] = value; return value; }
  throw new Error("Rock global " + entry.identity + " has no single-value accessor");
}
// The storage key a bridged name reads: the global's only slot, or the slot
// of the leaf it names.
function __rock_global_bridge_key(entry, name) {
  if (entry.slots.length === 1) return entry.slots[0];
  for (var i = 0; i < entry.physical.length; i++) {
    if (entry.physical[i] === name) return entry.slots[i];
  }
  throw new Error("Rock global " + entry.identity + " has no leaf " + name);
}
// Inline JS names a Rock global through `__G`; the bridge routes those reads
// and writes to the storage object.
function __rock_globals_install_bridge() {
  if (typeof __G === "undefined" || __G === null) return;
  if (__G.__rock_globals_bridge_target === true) return;
  var target = __G;
  Object.defineProperty(target, "__rock_globals_bridge_target", {
    value: true, writable: false, configurable: false, enumerable: false
  });
  __G = new Proxy(target, {
    get: function (object, key, receiver) {
      var entry = (typeof key === "string") ? __rock_global_entry(key) : null;
      if (entry !== null) return __rock_globals[__rock_global_bridge_key(entry, key)];
      return Reflect.get(object, key, receiver);
    },
    set: function (object, key, value, receiver) {
      var entry = (typeof key === "string") ? __rock_global_entry(key) : null;
      if (entry !== null) {
        __rock_globals[__rock_global_bridge_key(entry, key)] = value;
        return true;
      }
      return Reflect.set(object, key, value, receiver);
    },
    deleteProperty: function (object, key) {
      var entry = (typeof key === "string") ? __rock_global_entry(key) : null;
      if (entry !== null) {
        delete __rock_globals[__rock_global_bridge_key(entry, key)];
        return true;
      }
      return Reflect.deleteProperty(object, key);
    },
    has: function (object, key) {
      var entry = (typeof key === "string") ? __rock_global_entry(key) : null;
      return entry !== null || Reflect.has(object, key);
    }
  });
}
"#;

/// The coroutine runtime: JS ports of the stdlib `__rock_coroutine_kill` /
/// `__rock_coroutine_run_defers` inline(lua) bodies (`stdlib/unified.rock`),
/// plus the `error` global backing the transform's `Inst::Global("error")` +
/// `CallValue` raise shape (layout-stamp check, `await` re-raise). Defer
/// failures go through `__rock_log` (error level, `CO` tag) exactly as the
/// Lua body does, so an installed log sink sees them. Nil checks are truthiness (`!c.__defers`, `if (c.__inner)`) so
/// `undefined` and `null` both count as nil, matching the subset's loose rule.
pub const JS_CORO_RUNTIME: &str = r#"
function error(m) { throw new Error(m); }
function __rock_coroutine_run_defers(c) {
  if (!c.__defers) return;
  for (var i = c.__defers.length - 1; i >= 0; i--) {
    try { __rock_call(c.__defers[i], c); } catch (e) {
      var m = (e && e.message !== undefined) ? e.message : e;
      __rock_log(0, "CO", "coroutine defer failed during cleanup: " + m);
    }
  }
}
function __rock_coroutine_kill(c) {
  if (c.__state < 0) return;
  c.__state = -1;
  if (c.__inner) { __rock_coroutine_kill(c.__inner); }
  __rock_coroutine_run_defers(c);
  if (c.__on_killed) { __rock_call(c.__on_killed, c); }
}
"#;

/// Ordinary-fn `defer` unwinding (`skull_mir::mir::DEFER_PUSH`), the JS port of
/// the Lua prelude's cleanup stack (`skull::PRELUDE_TEMPLATE`). Registering a
/// defer pushes its cleanup closure and every scope-exit splice pops it, so the
/// stack holds exactly the cleanups of scopes still open. A body that pushes
/// catches what escapes it and calls `__rock_defer_unwind` with the depth it was
/// entered at. The original error wins: an `Error` (what `error` throws, the
/// Lua string error) or a string carries the cleanup's message; any other value
/// is rethrown unchanged and the message goes to the log.
pub const JS_DEFER_RUNTIME: &str = r#"
var __rock_defer_stack = [];
var __rock_defer_top = 0;
function __rock_defer_push(d) {
  __rock_defer_stack[__rock_defer_top] = d;
  __rock_defer_top = __rock_defer_top + 1;
}
function __rock_defer_pop() {
  __rock_defer_top = __rock_defer_top - 1;
  __rock_defer_stack[__rock_defer_top] = undefined;
}
// Never throws: a cleanup error's message or toString may itself throw.
function __rock_defer_describe(e) {
  try {
    var s = (e instanceof Error) ? e.message : lua_tostring(e);
    if (typeof s === "string") return s;
  } catch (_) {}
  return "<unprintable " + (e === null ? "nil" : typeof e) + " error>";
}
function __rock_defer_unwind(base, err) {
  while (__rock_defer_top > base) {
    var d = __rock_defer_stack[__rock_defer_top - 1];
    __rock_defer_pop();
    try { __rock_call(d); } catch (derr) {
      var note = "\n  (defer raised while unwinding: " + __rock_defer_describe(derr) + ")";
      if (err instanceof Error) {
        err.message = err.message + note;
      } else if (typeof err === "string") {
        err = err + note;
      } else {
        try {
          __rock_log_emit(1, "DEFER", "defer raised while unwinding a "
            + (err === null ? "nil" : typeof err) + " error: " + __rock_defer_describe(derr), false);
        } catch (_) {}
      }
    }
  }
  throw err;
}
"#;

/// JS runtime preamble for the string subset (T2): `lua_tostring` reproduces
/// Lua's `tostring`/`..` number formatting (`%.14g`), and `__rock_format_float`
/// ports the stdlib `@nomangle inline(lua)` helper (`stdlib/unified.rock`,
/// `%.2g` for tiny/huge magnitudes else `%.4f` trailing-zero-trimmed) that
/// backs the default float `{x}` interpolation hole. `__c_g` is C `printf`
/// `%.Pg`, reconstructed from a properly-rounded `toExponential` digit string
/// (no double-rounding). Prepend to any emitted program that touches strings;
/// the harness's `print` uses `lua_tostring` for the same parity.
pub const JS_STRING_RUNTIME: &str = r##"
// P significant decimal digits of a positive finite double, rounded
// half-to-EVEN like C printf (JS toExponential rounds half-UP -- diverges on
// exact ties, e.g. 123456789012345 -> ...234 not ...235). Exact via BigInt on
// the IEEE mantissa/exponent, so no double-rounding. Returns {digits, E} with
// value ~= digits[0].digits[1..] x 10^E.
function __bigdigits(ax, P) {
  var dv = new DataView(new ArrayBuffer(8));
  dv.setFloat64(0, ax);
  var hi = dv.getUint32(0), lo = dv.getUint32(4);
  var expBits = (hi >>> 20) & 0x7ff, mantHi = hi & 0xfffff, f, e;
  if (expBits === 0) { f = (BigInt(mantHi) << 32n) | BigInt(lo >>> 0); e = -1074; }
  else { f = (BigInt(mantHi | 0x100000) << 32n) | BigInt(lo >>> 0); e = expBits - 1075; }
  var num = e >= 0 ? f << BigInt(e) : f;
  var den = e >= 0 ? 1n : 1n << BigInt(-e);
  function roundHalfEven(k) {
    var n2 = num, d2 = den;
    if (k >= 0) d2 = d2 * (10n ** BigInt(k)); else n2 = n2 * (10n ** BigInt(-k));
    var q = n2 / d2, twice = (n2 - q * d2) * 2n;
    if (twice > d2 || (twice === d2 && q % 2n === 1n)) q += 1n;
    return q;
  }
  var D = Math.floor(Math.log10(ax));
  var N = roundHalfEven(D - P + 1), s = N.toString();
  // A wrong log10 estimate or a carry (999->1000) yields P+/-1 digits; re-round
  // at the corrected exponent.
  if (s.length !== P) { D += s.length - P; N = roundHalfEven(D - P + 1); s = N.toString(); }
  while (s.length < P) s = "0" + s;
  return { digits: s, E: D };
}
function __c_g(x, P) {
  // C printf %.Pg on a finite, already-sign-split magnitude reconstruction.
  var neg = x < 0 || (x === 0 && 1 / x === -Infinity);
  var ax = Math.abs(x);
  if (ax === 0) return neg ? "-0" : "0";
  var r = __bigdigits(ax, P);
  var digits = r.digits, E = r.E;
  var out;
  if (E < -4 || E >= P) {
    var frac = digits.slice(1).replace(/0+$/, "");
    var mant = digits.slice(0, 1) + (frac ? "." + frac : "");
    var esign = E < 0 ? "-" : "+";
    var eabs = Math.abs(E).toString();
    if (eabs.length < 2) eabs = "0" + eabs;
    out = mant + "e" + esign + eabs;
  } else if (E >= 0) {
    var intp = digits.slice(0, E + 1);
    var f2 = digits.slice(E + 1).replace(/0+$/, "");
    out = intp + (f2 ? "." + f2 : "");
  } else {
    var zeros = "";
    for (var k = 0; k < -E - 1; k++) zeros += "0";
    var f3 = (zeros + digits).replace(/0+$/, "");
    out = "0." + f3;
  }
  return neg ? "-" + out : out;
}
function lua_tostring(x) {
  if (typeof x === "number") {
    if (x !== x) return "nan";
    if (x === Infinity) return "inf";
    if (x === -Infinity) return "-inf";
    return __c_g(x, 14);
  }
  if (x === true) return "true";
  if (x === false) return "false";
  if (x === null || x === undefined) return "nil";
  return String(x);
}
function __rock_format_float(v) {
  // Non-finite first: C `%.2g` renders these as inf/-inf/nan, and __c_g below
  // reconstructs a value from its mantissa bits, so it would reach BigInt() with
  // Infinity and throw rather than format. Same spellings lua_tostring uses.
  if (v !== v) return "nan";
  if (v === Infinity) return "inf";
  if (v === -Infinity) return "-inf";
  var av = Math.abs(v);
  if (av !== 0 && (av < 0.0001 || av >= 1000000)) {
    return __c_g(v, 2);
  }
  var s = v.toFixed(4);
  s = s.replace(/0+$/, "").replace(/\.$/, "");
  return s;
}
// Lua `tonumber(s)`: a strict whole-string numeric parse (int/float/exponent/
// `0x` hex, surrounding whitespace trimmed), nil on anything else. JS `Number`
// agrees EXCEPT it reads an empty/whitespace-only string as 0 (handled here as
// null) and yields NaN (-> null) for a non-numeric tail. Rock nil is `null`.
function __parse_float(s) {
  if (typeof s !== "string" || s.trim() === "") return null;
  var n = Number(s);
  return n !== n ? null : n;
}
// stdlib from_codepoint: the same range and surrogate checks as luajit. JS
// strings are UTF-16, so the code point becomes UTF-16 text (as a literal does)
// rather than one char per UTF-8 byte, which would print as mojibake and
// compare unequal to the same literal.
function __from_codepoint(cp) {
  if (cp < 0 || cp > 0x10FFFF) throw new Error("from_codepoint: codepoint out of range: " + cp);
  if (cp >= 0xD800 && cp <= 0xDFFF) throw new Error("from_codepoint: surrogate is not a code point: " + cp);
  return String.fromCodePoint(cp);
}
// The `{x:spec}` string-interpolation format helpers (stdlib/strings.rock,
// @nomangle inline(lua)). ASCII fill/sep. `__rock_fmt_pad` pads `s` to `width`
// (`<` left, `^` centered, else right); `__rock_fmt_grouping` inserts `sep`
// every `group` digits from the right of the integer part (sign + fractional
// tail preserved); `__rock_fmt_binary` is sign + magnitude base-2 of floor(n);
// `__rock_fmt_percent` is `v*100` with `p` decimals (p<0 -> 6) + literal `%`.
function __rock_fmt_pad(s, width, align, fill) {
  s = lua_tostring(s);
  var n = s.length;
  if (n >= width) return s;
  var pad = width - n;
  if (align === "<") return s + fill.repeat(pad);
  if (align === "^") { var left = Math.floor(pad / 2); return fill.repeat(left) + s + fill.repeat(pad - left); }
  return fill.repeat(pad) + s;
}
function __rock_fmt_grouping(s, sep, group) {
  var m = s.match(/^([+\-]?)([a-zA-Z0-9]+)(\.?[\s\S]*)$/);
  if (!m) return s;
  var sign = m[1], num = m[2], frac = m[3];
  var reversed = num.split("").reverse().join("").replace(new RegExp("(.{" + group + "})", "g"), "$1" + sep);
  if (reversed.slice(-sep.length) === sep) reversed = reversed.slice(0, reversed.length - sep.length);
  return sign + reversed.split("").reverse().join("") + frac;
}
function __rock_fmt_binary(n) {
  var x = Math.floor(n);
  if (x === 0) return "0";
  var neg = x < 0;
  if (neg) x = -x;
  var s = "";
  while (x > 0) { s = (x % 2) + s; x = Math.floor(x / 2); }
  return (neg ? "-" : "") + s;
}
function __rock_fmt_percent(v, p) {
  var pp = p < 0 ? 6 : p;
  return __c_f(v * 100, pp) + "%";
}
// C `%f`: `x` rounded to `prec` fractional digits, round-half-to-EVEN (C printf;
// JS toFixed rounds half-up, diverging on exact ties like 2.5). Exact via BigInt
// on the IEEE mantissa/exponent -- no double-rounding. Sign split off; -0 and
// values that round to 0 from below keep the minus.
function __c_f(x, prec) {
  if (x !== x) return "nan";
  if (x === Infinity) return "inf";
  if (x === -Infinity) return "-inf";
  var neg = x < 0 || (x === 0 && 1 / x === -Infinity);
  var ax = Math.abs(x);
  var dv = new DataView(new ArrayBuffer(8));
  dv.setFloat64(0, ax);
  var hi = dv.getUint32(0), lo = dv.getUint32(4);
  var expBits = (hi >>> 20) & 0x7ff, mantHi = hi & 0xfffff, f, e;
  if (expBits === 0) { f = (BigInt(mantHi) << 32n) | BigInt(lo >>> 0); e = -1074; }
  else { f = (BigInt(mantHi | 0x100000) << 32n) | BigInt(lo >>> 0); e = expBits - 1075; }
  var tenP = 10n ** BigInt(prec);
  var num, den;
  if (e >= 0) { num = f * tenP * (1n << BigInt(e)); den = 1n; }
  else { num = f * tenP; den = 1n << BigInt(-e); }
  var q = num / den, twice = (num - q * den) * 2n;
  if (twice > den || (twice === den && q % 2n === 1n)) q += 1n;
  var digits = q.toString(), s;
  if (prec === 0) { s = digits; }
  else { while (digits.length <= prec) digits = "0" + digits; s = digits.slice(0, digits.length - prec) + "." + digits.slice(digits.length - prec); }
  return (neg ? "-" : "") + s;
}
// C `%e`: one digit, `prec` fractional digits, exponent `e[+-]NN` (>= 2 digits).
function __c_e(x, prec) {
  if (x !== x) return "nan";
  if (x === Infinity) return "inf";
  if (x === -Infinity) return "-inf";
  var neg = x < 0 || (x === 0 && 1 / x === -Infinity);
  var ax = Math.abs(x);
  var mant, E;
  if (ax === 0) { mant = "0"; if (prec > 0) mant += "." + "0".repeat(prec); E = 0; }
  else {
    var r = __bigdigits(ax, prec + 1);
    mant = r.digits.slice(0, 1) + (prec > 0 ? "." + r.digits.slice(1) : "");
    E = r.E;
  }
  var esign = E < 0 ? "-" : "+", eabs = Math.abs(E).toString();
  if (eabs.length < 2) eabs = "0" + eabs;
  return (neg ? "-" : "") + mant + "e" + esign + eabs;
}
// C `string.format(fmt, v)` for a SINGLE value -- the shape `Format`/`FormatDyn`
// emit. Scans `fmt`, copying literals (`%%` -> `%`), and applies the one
// conversion spec (`%[flags][width][.prec]conv`) to `v`. Conversions: d i u x X o
// f F e E g G s c. Flags: `-` (left), `0` (zero-pad), `+`/space (sign), `#`
// (0x/0). Signed integer / hex negatives above 32-bit and Lua's 64-bit %x wrap
// are out of subset (unused by the interpolation specs).
function __rock_sprintf(fmt, v) {
  var out = "", i = 0;
  while (i < fmt.length) {
    if (fmt[i] !== "%") { out += fmt[i++]; continue; }
    i++;
    if (fmt[i] === "%") { out += "%"; i++; continue; }
    var flags = "";
    while (i < fmt.length && "-+ 0#".indexOf(fmt[i]) >= 0) { flags += fmt[i++]; }
    var width = "";
    while (i < fmt.length && fmt[i] >= "0" && fmt[i] <= "9") { width += fmt[i++]; }
    width = width === "" ? -1 : parseInt(width, 10);
    var prec = -1;
    if (fmt[i] === ".") { i++; var pp = ""; while (i < fmt.length && fmt[i] >= "0" && fmt[i] <= "9") { pp += fmt[i++]; } prec = pp === "" ? 0 : parseInt(pp, 10); }
    var conv = fmt[i++];
    var body, sign = "", isNum = false, prefix = "";
    if (conv === "d" || conv === "i" || conv === "u") {
      isNum = true;
      var n = Math.trunc(Number(v));
      if (n < 0) { sign = "-"; n = -n; } else if (flags.indexOf("+") >= 0) sign = "+"; else if (flags.indexOf(" ") >= 0) sign = " ";
      body = n.toString();
      if (prec >= 0) while (body.length < prec) body = "0" + body;
    } else if (conv === "x" || conv === "X" || conv === "o") {
      isNum = true;
      var base = conv === "o" ? 8 : 16;
      var m = Math.trunc(Number(v));
      body = Math.abs(m).toString(base);
      if (conv === "X") body = body.toUpperCase();
      if (m < 0) sign = "-";
      if (prec >= 0) while (body.length < prec) body = "0" + body;
      if (flags.indexOf("#") >= 0 && m !== 0) prefix = conv === "o" ? "0" : (conv === "X" ? "0X" : "0x");
    } else if (conv === "f" || conv === "F") {
      isNum = true;
      var p = prec < 0 ? 6 : prec, r = __c_f(v, p);
      if (r[0] === "-") { sign = "-"; body = r.slice(1); } else { body = r; if (flags.indexOf("+") >= 0) sign = "+"; else if (flags.indexOf(" ") >= 0) sign = " "; }
    } else if (conv === "e" || conv === "E") {
      isNum = true;
      var p2 = prec < 0 ? 6 : prec, r2 = __c_e(v, p2);
      if (conv === "E") r2 = r2.toUpperCase();
      if (r2[0] === "-") { sign = "-"; body = r2.slice(1); } else { body = r2; if (flags.indexOf("+") >= 0) sign = "+"; else if (flags.indexOf(" ") >= 0) sign = " "; }
    } else if (conv === "g" || conv === "G") {
      isNum = true;
      var p3 = prec < 0 ? 6 : (prec === 0 ? 1 : prec), r3 = __c_g(v, p3);
      if (conv === "G") r3 = r3.toUpperCase();
      if (r3[0] === "-") { sign = "-"; body = r3.slice(1); } else { body = r3; if (flags.indexOf("+") >= 0) sign = "+"; else if (flags.indexOf(" ") >= 0) sign = " "; }
    } else if (conv === "c") {
      body = String.fromCharCode(Math.trunc(Number(v)));
    } else if (conv === "s") {
      body = lua_tostring(v);
      if (prec >= 0) body = body.slice(0, prec);
    } else {
      body = conv; // unknown: echo the conversion char
    }
    var core = sign + prefix + body;
    if (width > core.length) {
      var padn = width - core.length;
      if (flags.indexOf("-") >= 0) core = core + " ".repeat(padn);
      else if (flags.indexOf("0") >= 0 && isNum && !(prec >= 0 && (conv === "d" || conv === "i" || conv === "u" || conv === "x" || conv === "X" || conv === "o"))) core = sign + prefix + "0".repeat(padn) + body;
      else core = " ".repeat(padn) + core;
    }
    out += core;
  }
  return out;
}
"##;

/// Node's `__rock_host`, the default for tests and node-run programs.
pub const JS_NODE_HOST: &str = r#"
var __rock_host = {
  write: function(s) { process.stdout.write(s); },
  write_err: function(s) { process.stderr.write(s); },
  env_var: function(n) { var v = process.env[n]; return v === undefined ? null : v; }
};
"#;

/// The language half of host I/O, written against `__rock_host`: Lua-parity
/// `print` (args `lua_tostring`-joined by TAB, one trailing newline), raw
/// `put`, `env_var` (unset is Rock null, matching `os.getenv`), and one RNG
/// seed per program -- `ROCK_TEST_SEED` when set and finite, else `Date.now()`;
/// a program needing determinism calls `random_seed()` itself.
pub const JS_HOST_RUNTIME: &str = r#"
function print() {
  var p = [];
  for (var i = 0; i < arguments.length; i++) p.push(lua_tostring(arguments[i]));
  __rock_host.write(p.join("\t") + "\n");
}
function put(s) { __rock_host.write(s); }
function __rock_env_var(n) { var v = __rock_host.env_var(n); return v === undefined ? null : v; }
(function() {
  var v = __rock_env_var("ROCK_TEST_SEED");
  var s = v === null ? NaN : Number(v);
  var base = Number.isFinite(s) ? s : Date.now();
  __rock_rng_seed(base);
  __rock_presentation_rng_seed(base + 10007);
})();
"#;

/// The runtime prelude an emitted program runs after; append the program text
/// and its entry call (`main_js_name`) to the result.
///
/// Order: the language runtime blocks, then `extra_runtime` (an embedder's own
/// blocks, such as a platform layer; a function they declare under a name the
/// host runtime also declares is overridden by it), then `host`, then
/// [`JS_HOST_RUNTIME`] and the program's global table `__G`. `host` defines
/// `var __rock_host = { write(s), write_err(s), env_var(name) }`, the only
/// path from generated JS to the outside world: [`JS_NODE_HOST`] for node; a
/// browser Worker passes its own (`postMessage` writes, `env_var` returning
/// null). `emitted` and `has_code_refs` ([`emit::JsProgram::has_code_refs`])
/// select the call helper the program needs, if any.
pub fn assemble_prelude(
    emitted: &str,
    has_code_refs: bool,
    host: &str,
    extra_runtime: &[&str],
) -> String {
    let mut p = String::from(JS_NB_CHECK_SHIMS);
    p.push_str(JS_VEC_CHECK_SHIMS);
    p.push_str(JS_TABLE_PRIM);
    p.push_str(JS_MAP_ITER_RUNTIME);
    p.push_str(JS_STRUCTURAL_EQ);
    p.push_str(JS_ENUM_RUNTIME);
    if has_code_refs {
        p.push_str(JS_CALL_HELPER);
    } else if emitted.contains("__rock_call(") || emitted.contains("__try_call(") {
        p.push_str(JS_FOREIGN_CALL_HELPER);
    }
    p.push_str(JS_GLOBALS_RUNTIME);
    p.push_str(JS_CORO_RUNTIME);
    p.push_str(JS_DEFER_RUNTIME);
    p.push_str(JS_STRING_RUNTIME);
    p.push_str(JS_INSPECT_RUNTIME);
    p.push_str(JS_DEEPCOPY_RUNTIME);
    p.push_str(JS_LOG_RUNTIME);
    p.push_str(JS_RNG_RUNTIME);
    for block in extra_runtime {
        p.push_str(block);
    }
    p.push_str(host);
    p.push_str(JS_HOST_RUNTIME);
    p.push_str("\nvar __G = {};\n");
    p
}

/// The JS intrinsic table, with the workspace needed to match signature keys.
pub struct JsIntrinsics<'a>(pub &'a bone_semantic::inference::Workspace);

impl IntrinsicTable for JsIntrinsics<'_> {
    fn backend(&self) -> &'static str {
        "js"
    }
    fn lookup(&self, f: &Function) -> Option<Emission> {
        if !skull_mir::intrinsics::is_intrinsic(f) {
            return None;
        }
        let n = f.name.as_str();
        let arity = f.param_decls.len();
        let signature = std::cell::OnceCell::new();
        JS_INTRINSICS
            .iter()
            .find(|(key, ar, _)| {
                *ar == arity
                    && (*key == n
                        || (key
                            .strip_prefix(n)
                            .is_some_and(|tail| tail.starts_with('_'))
                            && *key
                                == signature
                                    .get_or_init(|| skull_mir::mangle::mangle_fn(self.0, f))))
            })
            .map(|(_, _, emission)| *emission)
    }
    fn entries(&self) -> Vec<IntrinsicEntry> {
        JS_INTRINSICS
            .iter()
            .map(|(name, arity, _)| IntrinsicEntry {
                name,
                arity: *arity,
            })
            .collect()
    }
}

/// The JS backend's realization oracle, as the artifact seam.
///
/// A zero-sized handle over [`emit::js_supply`] -- the SAME function
/// `render_call` and `discover_callees` branch on. `skull::artifact` cannot
/// depend on this crate (the backend is driven from `rock-cli`), so the view
/// takes the oracle through
/// [`skull_mir::intrinsics::BackendRealization`]; what it carries is the
/// behavior, not a description of it.
pub struct JsBackendRealization;

impl skull_mir::intrinsics::BackendRealization for JsBackendRealization {
    fn backend(&self) -> &'static str {
        "js"
    }
    fn supplies(&self, ws: &bone_semantic::inference::Workspace, f: &Function) -> bool {
        emit::js_supply(ws, f).is_some()
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
