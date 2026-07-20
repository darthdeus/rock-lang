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
mod structure;

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
    // like the luajit cdata. TypedArray store conversion wraps mod 2^n, matching
    // LuaJIT's native truncation (differentially verified by tests/nb_differential.rs).
    // `int`/`float` allocs no longer route through a named intrinsic -- they lower
    // to the `FfiNewArray` MIR node (`native_buffer_alloc<T>`), rendered in
    // `emit.rs`; only the phantom-width variants keep a per-type template here.
    ("__nb_alloc_u8", 1, "new Uint8Array($1)"),
    ("__nb_alloc_i8", 1, "new Int8Array($1)"),
    ("__nb_alloc_u16", 1, "new Uint16Array($1)"),
    ("__nb_alloc_i16", 1, "new Int16Array($1)"),
    ("__nb_alloc_u32", 1, "new Uint32Array($1)"),
    ("nb_get", 2, "$1.ptr[$2]"),
    ("nb_set", 3, "($1.ptr[$2] = $3)"),
    // StringBuilder: a Lua table used as an array accumulator IS a JS array.
    // `sb_new` -> `[]`, `sb_push` -> `.push` (Lua `sb[#sb+1] = s`; the returned
    // length is discarded), `sb_build` -> `.join("")` (Lua `table.concat`).
    ("sb_new", 0, "[]"),
    ("sb_push", 2, "$1.push($2)"),
    ("sb_build", 1, "$1.join(\"\")"),
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

/// JS shims for the checked Vec indexing helpers codegen emits
/// (`__vec_checked_get`/`__vec_checked_set`), mirroring the stdlib `inline(lua)`
/// bodies: `_data` is 1-indexed (`_data[i + 1]`), bounds-erroring on miss. The
/// error message shape follows the luajit stdlib (`f5efe7604`); it is not
/// compared in-scope (an OOB index throws -> the harness reports FAIL, matching
/// luajit -- `try_call` catching is out of scope).
pub const JS_VEC_CHECK_SHIMS: &str = "\
function __vec_checked_get(vec, i) {
  if (i < 0 || i >= vec.len) { throw new Error(\"Vec index out of bounds: index \" + i + \" is outside [0, \" + vec.len + \")\"); }
  return vec._data[i + 1];
}
function __vec_checked_set(vec, i, v) {
  if (i < 0 || i >= vec.len) { throw new Error(\"Vec index out of bounds: index \" + i + \" is outside [0, \" + vec.len + \")\"); }
  vec._data[i + 1] = v;
}
// `Vec.sort(key_fn)`: the stdlib inline(lua) is `table.sort(_data, |a,b| key(a) <
// key(b))`. Sort only the logical `1..len` slice (index 0 is the layout hole),
// then write back, so the 1-indexed `_data` shape is preserved. `key_fn` is a fn
// value invoked through `__rock_call`; keys compare with `<` (numbers/strings).
// JS sort is stable where luajit's is not, so equal-key relative order can
// differ -- capture-free key spec keeps this observable only for genuine ties.
function __vec_sort(vec, key_fn) {
  var n = vec.len;
  var part = [];
  for (var i = 1; i <= n; i++) part.push(vec._data[i]);
  part.sort(function(a, b) {
    var ka = __rock_call(key_fn, a), kb = __rock_call(key_fn, b);
    return ka < kb ? -1 : (ka > kb ? 1 : 0);
  });
  for (var i = 0; i < n; i++) vec._data[i + 1] = part[i];
}
// `__range(a, b)`: the materialized half-open range `a..b` as a Vec-shaped
// object (index 0 is the 1-indexed layout hole, `_data[1..len]` = a..b-1),
// matching the stdlib inline(lua) `{_data, len}`.
function __range(a, b) {
  var d = [null];
  for (var i = a; i < b; i++) d.push(i);
  return { _data: d, len: b - a };
}
";

/// Runtime-polymorphic untyped table primitives. In Lua a `Map` IS a raw table,
/// so `table_get(m, k)` and `m[k]` are the same op; on JS a `Map` is a real
/// `Map` (keys not string-coerced) while arrays/objects index directly, so these
/// dispatch on `instanceof Map` to keep the two access paths consistent.
pub const JS_TABLE_PRIM: &str = r#"
function __table_get(t, k) { return (t instanceof Map) ? t.get(k) : t[k]; }
function __table_set(t, k, v) { if (t instanceof Map) t.set(k, v); else t[k] = v; }
function __table_unset(t, k) { if (t instanceof Map) t.delete(k); else delete t[k]; }
"#;

/// The uniform indirect-call dispatch helper (`docs/closure-conversion-mir-plan.md`):
/// a fn value is either a raw JS function (a named fn / capture-free hoisted
/// lambda, referenced by global name) or a closure ENV RECORD `{ __fn, .. }`
/// (a capturing lambda, built by `convert_closures`). `__rock_call` invokes both
/// -- a function directly, a record through its `__fn(record, ...)` -- so every
/// `CallValue` emits `__rock_call(f, ...args)`. Prepend to any emitted program
/// that calls a fn value. (The Lua path uses a `__call` metatable instead; JS
/// objects carry no metatable, so the dispatch is explicit here.)
pub const JS_CALL_HELPER: &str = r#"
function __rock_call(f) {
  var a = Array.prototype.slice.call(arguments, 1);
  if (typeof f === "function") return f.apply(null, a);
  return f.__fn.apply(null, [f].concat(a));
}
// `try_call(f)`: run `f` and report whether it errored (Lua `pcall`; returns
// `not ok`). A JS throw is the pcall failure; the error itself is discarded by
// design (the caller only wants the did-it-error bool, matching the stdlib body).
function __try_call(f) {
  try { __rock_call(f); return false; } catch (e) { return true; }
}
"#;

/// The `__op_eq` runtime: deep structural equality matching Rock's stdlib
/// `__op_eq` (verified on luajit: reference structs, value structs, tuples,
/// enums, newtypes all compare BY CONTENTS; primitives by value). Lua `nil` maps
/// to both JS `null` and `undefined` (an absent arg / an array hole), so both
/// count as nil -- this also reconciles a Vec's index-0 placeholder (`null` in a
/// literal vs a hole after `push`). Structs are `{__ty, ...}` plain objects (the
/// `__rock_create_struct` shape: a `__ty` string tag, no metatable), so a
/// key-walk that includes `__ty` covers the type check for free. A capturing
/// lambda is a closure ENV RECORD `{ __fn, .. }` (`convert_closures`): a callable
/// value, so -- like a raw fn -- it compares by IDENTITY, never by walking its
/// captures (matching luajit's `__rock_fn_mt` identity-leaf rule and pre-arc
/// arrow `===`).
pub const JS_STRUCTURAL_EQ: &str = r#"
function __op_eq(a, b) {
  var an = (a === null || a === undefined), bn = (b === null || b === undefined);
  if (an || bn) return an && bn;
  if (a === b) return true;
  if (typeof a !== "object" || typeof b !== "object") return a === b;
  if (a.__fn !== undefined || b.__fn !== undefined) return a === b;
  var aa = Array.isArray(a), ab = Array.isArray(b);
  if (aa || ab) {
    if (!aa || !ab || a.length !== b.length) return false;
    for (var i = 0; i < a.length; i++) if (!__op_eq(a[i], b[i])) return false;
    return true;
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
/// object (keys sorted by `lua_tostring`, `__ty`/`__variant`/`variant`/`__tag`
/// skipped), a Vec `{_data (1-indexed), len, __ty:"Vec"}`, a JS `Map` (raw-table
/// analog), a JS array (Lua array-table analog), and scalars. A capturing-lambda
/// env record (`__fn`) is an opaque callable LEAF (`String(fn)`), matching the
/// luajit `__rock_fn_mt` rule; a `visited` Map detects cycles. NOTE: most inspect
/// tests are VACUOUS -- a `@test(output=..)` whose expected contains `{` is an
/// interpolated string, so its expectation is dropped (`out=nil`) and the test
/// only checks that inspect RUNS; the byte-checked cases are the brace-free
/// scalars (`"str"`, numbers, bools). Compact/multiline threshold + key sort
/// reproduce the Lua renderer for the cross-backend `inspect(x) == "..."` pins.
pub const JS_INSPECT_RUNTIME: &str = r#"
function __rock_inspect_key_lt(a, b) {
  var sa = lua_tostring(a), sb = lua_tostring(b);
  return sa < sb ? -1 : (sa > sb ? 1 : 0);
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
  if (t.__fn !== undefined) return "function: builtin";
  if (visited) {
    if (visited.has(t)) return "<CYCLE: " + visited.get(t) + ">";
    visited.set(t, path);
  }
  var type_name = t.__ty;
  if (type_name === "Vec" && t._data && t.len !== undefined) {
    var vparts = [];
    for (var i = 1; i <= t.len; i++) {
      vparts.push(__rock_inspect_value(t._data[i], indent, path + "[" + (i - 1) + "]", visited, precision));
    }
    if (visited) visited.delete(t);
    if (vparts.length === 0) return "Vec { }";
    return "Vec { " + vparts.join(", ") + " }";
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
          k !== "__variant" && k !== "__ty" && k !== "variant" && k !== "__tag") {
        pairs.push([k, t[k]]);
      }
    }
  }
  pairs.sort(function(a, b) { return __rock_inspect_key_lt(a[0], b[0]); });
  var variant = t.__variant !== undefined ? t.__variant : t.variant;
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
  // with luajit over a 0-based runtime. Record-variant fields (string keys) and
  // struct fields are unaffected.
  var enumBox = variant !== undefined;
  var keyStr = function(k) {
    if (enumBox && typeof k === "string" && /^[0-9]+$/.test(k)) return "[" + (parseInt(k, 10) + 1) + "]";
    return typeof k === "string" ? k : ("[" + lua_tostring(k) + "]");
  };
  if (pairs.length <= 2) {
    var cparts = pairs.map(function(kv) {
      return keyStr(kv[0]) + " = " + __rock_inspect_value(kv[1], indent, path + "." + lua_tostring(kv[0]), visited, precision);
    });
    result = header !== "" ? header + " { " + cparts.join(", ") + " }" : "{ " + cparts.join(", ") + " }";
  } else {
    var lines = pairs.map(function(kv) {
      return indent + "  " + keyStr(kv[0]) + " = " + __rock_inspect_value(kv[1], indent + "  ", path + "." + lua_tostring(kv[0]), visited, precision);
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
/// ONCE, a cycle terminates. Primitives and bare functions (a closure's `__fn`)
/// pass through by identity; a TypedArray (NativeBuffer `.ptr`) is `.slice()`d; a
/// `Map` and an array (a Vec's `_data`) copy element-wise; every other object (a
/// struct `{__ty,...}`, an env record `{__fn,...}`, a box) copies own keys. A
/// copied env record keeps its `__fn` and its copied box, so `__rock_call` still
/// dispatches and shared-box aliasing survives (the closures read captures from
/// the record arg, not lexically).
pub const JS_DEEPCOPY_RUNTIME: &str = r#"
function __rock_deepcopy_value(obj, seen) {
  if (obj === null || obj === undefined) return obj;
  var t = typeof obj;
  if (t !== "object" && t !== "function") return obj;
  if (t === "function") return obj;
  if (seen.has(obj)) return seen.get(obj);
  if (ArrayBuffer.isView(obj)) { var tc = obj.slice(); seen.set(obj, tc); return tc; }
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

/// The logging runtime: JS ports of the `@nomangle inline(lua)` dedup/throttle
/// impls in `stdlib/logging.rock`. Rendered by their original names
/// (`is_host_log_fn`), so `log_once`/`log_throttled` (macros -> `_log_once_impl`
/// / `_log_throttled_*_impl`) route here. `__stone_emit`'s no-sink fallback is
/// `print("[cat] msg")`, so lines land in the same per-test buffer as `print`.
/// The one Rock global (`__stone_log_level`) lives in `__G` (unified with the
/// module-globals path); dedup/throttle/now are bare vars (never Rock globals).
/// `__stone_log_now` uses `Date.now()` as the os.clock() analog (the fallback
/// test uses a huge interval, so the absolute value never matters).
pub const JS_LOG_RUNTIME: &str = r#"
function __stone_log_init() {
  if (__G.__stone_log_level == null || __G.__stone_log_level < 0) __G.__stone_log_level = 2;
}
function __stone_emit(level, category, msg) {
  if (category && category.length > 0) print("[" + category + "] " + msg);
  else print(msg);
}
function __stone_log(level, category, msg) {
  __stone_log_init();
  if (level > __G.__stone_log_level) return;
  __stone_emit(level, category, msg);
}
function log_set_level(level) { __G.__stone_log_level = level; }
var __stone_log_dedup = {};
var __stone_log_throttle = {};
var __stone_log_now_override = null;
function _log_once_impl(category, msg, site) {
  __stone_log_init();
  if (0 > __G.__stone_log_level) return;
  if (__stone_log_dedup[site]) return;
  __stone_log_dedup[site] = true;
  __stone_emit(0, category, msg);
}
function _log_throttled_with_now_impl(now_sec, category, msg, interval_sec, site) {
  __stone_log_init();
  if (0 > __G.__stone_log_level) return;
  var last = __stone_log_throttle[site];
  if (last != null && (now_sec - last) < interval_sec) return;
  __stone_log_throttle[site] = now_sec;
  __stone_emit(0, category, msg);
}
function stone_log_set_now(now_sec) { __stone_log_now_override = now_sec; }
function stone_log_clear_now() { __stone_log_now_override = null; }
function __stone_log_now() {
  if (__stone_log_now_override != null) return __stone_log_now_override;
  return Date.now() / 1000;
}
function _log_throttled_impl(category, msg, interval_sec, site) {
  _log_throttled_with_now_impl(__stone_log_now(), category, msg, interval_sec, site);
}
function log_reset_dedup() { __stone_log_dedup = {}; __stone_log_throttle = {}; }
function log_dedup_count() { return Object.keys(__stone_log_dedup).length; }
"#;

/// The coroutine runtime: JS ports of the stdlib `__rock_coroutine_kill` /
/// `__rock_coroutine_run_defers` inline(lua) bodies (`stdlib/unified.rock`),
/// plus the `error` global backing the transform's `Inst::Global("error")` +
/// `CallValue` raise shape (layout-stamp check, `yield*` re-raise). Defer
/// failures go to STDERR, never stdout -- the harness output-compare must not
/// see them. Nil checks are truthiness (`!c.__defers`, `if (c.__inner)`) so
/// `undefined` and `null` both count as nil, matching the subset's loose rule.
pub const JS_CORO_RUNTIME: &str = r#"
function error(m) { throw new Error(m); }
function __rock_coroutine_run_defers(c) {
  if (!c.__defers) return;
  for (var i = c.__defers.length - 1; i >= 0; i--) {
    try { c.__defers[i](c); } catch (e) {
      var m = (e && e.message !== undefined) ? e.message : e;
      process.stderr.write("coroutine defer failed during cleanup: " + m + "\n");
    }
  }
}
function __rock_coroutine_kill(c) {
  if (c.__state < 0) return;
  c.__state = -1;
  if (c.__inner) { __rock_coroutine_kill(c.__inner); }
  __rock_coroutine_run_defers(c);
  if (c.__on_killed) { c.__on_killed(c); }
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
