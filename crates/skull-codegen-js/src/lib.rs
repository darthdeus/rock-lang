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
    ("tan", 1, "Math.tan($1)"),
    ("sqrt", 1, "Math.sqrt($1)"),
    ("exp", 1, "Math.exp($1)"),
    // `log` is the NATURAL log on both hosts (Lua `math.log`, JS `Math.log`).
    // `log2` gets `Math.log2` rather than a mirror of luajit's
    // `math.log(x) / 0.6931471805599453`: it is the same function computed
    // better, exact on powers of two where the division is a bit short.
    ("log", 1, "Math.log($1)"),
    ("log2", 1, "Math.log2($1)"),
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
    // `join` over a `[string]`: the backing array IS the input both hosts want,
    // so this is one `Array.prototype.join` here and one `table.concat` on
    // luajit. `_data` is 0-based and may be longer than `len`, hence the slice.
    ("join", 2, "$1._data.slice(0, $1.len).join($2)"),
    // Reference identity (stdlib `same_ref`). `===` on two objects is JS
    // identity, the same question luajit's `rawequal` answers; on primitives
    // both reduce to value equality, which is also what identity means there.
    ("same_ref", 2, "($1 === $2)"),
    // IEEE non-finite predicates (docs/shaders.md bvec tier). isinf excludes NaN
    // (a NaN is neither finite nor infinite). any/greater_than/to_vec3 co-emit
    // their portable Rock bodies -- no JS row.
    ("isnan", 1, "Number.isNaN($1)"),
    ("isinf", 1, "(!Number.isFinite($1) && !Number.isNaN($1))"),
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
    ("native_buffer_uint", 1, "({ptr: new Uint32Array($1), len: $1})"),
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
/// count as nil. A Vec compares by its tracked `len` over `_data[0..len)`, not
/// as a dense array -- mirroring luajit's `pairs`-walk, which is len-bounded, so
/// a drained tail slot (`_data[len] = undefined` after pop/clear) is invisible.
/// Structs are `{__ty, ...}` plain objects (the
/// `__rock_create_struct` shape: a `__ty` string tag, no metatable), so a
/// key-walk that includes `__ty` covers the type check for free. A capturing
/// lambda is a closure ENV RECORD `{ __fn, .. }` (`convert_closures`): a callable
/// value, so -- like a raw fn -- it compares by IDENTITY, never by walking its
/// captures (matching luajit's `__rock_fn_mt` identity-leaf rule and pre-arc
/// arrow `===`).
pub const JS_STRUCTURAL_EQ: &str = r#"
// Per-type equality for GC-carrying value structs (`build_vt_eq_defs` fills it):
// GC-ref fields by reference, scalar/nested-value fields structural. An entry is
// the AUTHORITY, mirroring luajit's `_vt.__eq` metatable rule -- consulted before
// the generic key walk so a Vec/Map/ref-struct field is NOT contents-compared.
var __rock_vt_eq = {};
function __op_eq(a, b) {
  var an = (a === null || a === undefined), bn = (b === null || b === undefined);
  if (an || bn) return an && bn;
  if (a === b) return true;
  if (typeof a !== "object" || typeof b !== "object") return a === b;
  if (a.__fn !== undefined || b.__fn !== undefined) return a === b;
  // A Vec compares by tracked `len` over `_data[0..len)` (len-bounded, like
  // luajit's pairs-walk), so a drained tail slot beyond `len` never breaks eq.
  if (a.__ty === "Vec" || b.__ty === "Vec") {
    if (a.__ty !== "Vec" || b.__ty !== "Vec" || a.len !== b.len) return false;
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
/// env record (`__fn`) is an opaque callable LEAF (`String(fn)`), matching the
/// luajit `__rock_fn_mt` rule; a `visited` Map detects cycles. NOTE: most inspect
/// tests are VACUOUS -- a `@test(output=..)` whose expected contains `{` is an
/// interpolated string, so its expectation is dropped (`out=nil`) and the test
/// only checks that inspect RUNS; the byte-checked cases are the brace-free
/// scalars (`"str"`, numbers, bools). Compact/multiline threshold + key sort
/// reproduce the Lua renderer for the cross-backend `inspect(x) == "..."` pins.
///
/// A struct renders through `__rock_inspect_ty[<__ty>]`, a per-type function
/// `build_inspect_ty_defs` emits for every reachable struct: it supplies the
/// DECLARED fields, because a null field is an absent key and a key walk alone
/// therefore under-reports the shape, and an `embed` field is stored as flat
/// `<name>.<leaf>` slots that carry no declared name at all. That is luajit's
/// `_G.__ROCK_TYPE_TABLE` lookup replaced by compile-time-emitted dispatch
/// (`docs/runtime-globals-audit.md` S4.2b) -- no universal type table on JS.
pub const JS_INSPECT_RUNTIME: &str = r#"
var __rock_inspect_ty = {};
var __rock_col_mat = {};
// A column-stored Vec has no `_data`: one plain array per leaf, sharing the
// Vec's `len`. Rendering its own keys would show the PHYSICAL layout (`@vec`,
// `x = { 1 }`), so materialize each element first -- luajit's
// `__rock_vec_column_elements`, over emitted per-type rebuilders instead of a
// runtime type table.
function __rock_col_header(t) {
  var h = t["@vec"];
  return (h !== null && typeof h === "object" && h.repr === "columns") ? h : null;
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
  var colh = type_name === "Vec" && t.len !== undefined ? __rock_col_header(t) : null;
  if (type_name === "Vec" && t.len !== undefined && (t._data || colh)) {
    var mat = colh ? __rock_col_mat[colh.elem] : null;
    if (colh && mat === undefined) {
      throw new Error("no column materializer emitted for element type " + colh.elem);
    }
    var vparts = [];
    for (var i = 0; i < t.len; i++) {
      var elem = colh ? mat(t, i) : t._data[i];
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
/// module-globals path); dedup/throttle/counts/sites/now are bare vars (never
/// Rock globals). `_log_once_impl` COUNTS rather than flagging, and the throttle
/// impl records its own counts, because `log_suppression_summary` reports the
/// totals -- a boolean dedup cannot answer "this site fired 38k times".
/// `__stone_log_now` uses `Date.now()` as the os.clock() analog (the fallback
/// test uses a huge interval, so the absolute value never matters).
pub const JS_LOG_RUNTIME: &str = r#"
function __stone_log_init() {
  if (__G.__stone_log_level == null || __G.__stone_log_level < 0) __G.__stone_log_level = 2;
}
function __stone_emit(level, category, msg, wall_clock) {
  if (category && category.length > 0) print("[" + category + "] " + msg);
  else print(msg);
}
function __stone_log(level, category, msg) {
  __stone_log_init();
  if (level > __G.__stone_log_level) return;
  __stone_emit(level, category, msg, false);
}
function __stone_log_wall_clock(level, category, msg) {
  __stone_log_init();
  if (level > __G.__stone_log_level) return;
  __stone_emit(level, category, msg, true);
}
function log_set_level(level) { __G.__stone_log_level = level; }
// `__stone_log_dedup` holds a COUNT, not a flag: `log_suppression_summary`
// reports how many times a suppressed site fired, and a site that fired 38k
// times must not read like one that fired once.
var __stone_log_dedup = {};
var __stone_log_counts = {};
var __stone_log_sites = {};
var __stone_log_summary_done = false;
var __stone_log_throttle = {};
var __stone_log_wall = {};
var __stone_log_now_override = null;
function _log_once_impl(category, msg, site) {
  __stone_log_init();
  if (0 > __G.__stone_log_level) return;
  var n = (__stone_log_dedup[site] || 0) + 1;
  __stone_log_dedup[site] = n;
  __stone_log_sites[site] = (category || "") + "|" + msg;
  if (n > 1) return;
  __stone_emit(0, category, msg, false);
}
function _log_throttled_with_now_impl(now_sec, category, msg, interval_sec, site, wall_clock) {
  __stone_log_init();
  if (0 > __G.__stone_log_level) return;
  __stone_log_counts[site] = (__stone_log_counts[site] || 0) + 1;
  __stone_log_sites[site] = (category || "") + "|" + msg;
  // `= nil` in the luajit sibling, which removes the key; keep the two tables
  // the same shape rather than storing a false.
  if (wall_clock) __stone_log_wall[site] = true;
  else delete __stone_log_wall[site];
  var last = __stone_log_throttle[site];
  if (last != null && (now_sec - last) < interval_sec) return;
  __stone_log_throttle[site] = now_sec;
  __stone_emit(0, category, msg, wall_clock);
}
// Ties order differently from the luajit sibling (JS sort is stable, table.sort
// is not); the counts themselves match.
function log_suppression_summary() {
  __stone_log_init();
  if (__stone_log_summary_done) return;
  __stone_log_summary_done = true;
  var rows = [];
  var collect = function(tbl) {
    for (var site in tbl) {
      if (Object.prototype.hasOwnProperty.call(tbl, site) && tbl[site] > 1) {
        rows.push({ site: site, n: tbl[site] });
      }
    }
  };
  collect(__stone_log_dedup);
  collect(__stone_log_counts);
  if (rows.length === 0) return;
  rows.sort(function(a, b) { return b.n - a.n; });
  for (var i = 0; i < rows.length; i++) {
    var r = rows[i];
    var rec = __stone_log_sites[r.site] || "|";
    var bar = rec.indexOf("|");
    __stone_emit(0, rec.slice(0, bar),
      "suppressed x" + r.n + " at " + r.site + ": " + rec.slice(bar + 1),
      __stone_log_wall[r.site] === true);
  }
}
function stone_log_set_now(now_sec) { __stone_log_now_override = now_sec; }
function stone_log_clear_now() { __stone_log_now_override = null; }
function __stone_log_now() {
  if (__stone_log_now_override != null) return __stone_log_now_override;
  return Date.now() / 1000;
}
function _log_throttled_impl(category, msg, interval_sec, site, wall_clock) {
  _log_throttled_with_now_impl(__stone_log_now(), category, msg, interval_sec, site, wall_clock);
}
function log_reset_dedup() {
  __stone_log_dedup = {}; __stone_log_throttle = {}; __stone_log_wall = {};
  __stone_log_counts = {}; __stone_log_sites = {}; __stone_log_summary_done = false;
}
function log_dedup_count() { return Object.keys(__stone_log_dedup).length; }
"#;

/// The RNG runtime: mulberry32 over a single 32-bit state word (`>>> 0` keeps it
/// unsigned; `Math.imul` for the 32-bit multiplies) -- zero allocation. The fn
/// surface mirrors the Lua splitmix prelude (`crates/skull/src/lib.rs`):
/// `__rock_rng_seed`, `__rock_rng_state_get`/`_set` (the single-word replay
/// serialization surface), and the draw. Streams differ per backend by doctrine
/// (Lua splitmix64, JS mulberry32) -- bit-parity is a non-goal; the SEMANTIC
/// (seedable + deterministic + single serializable stream) is upheld. Engine-
/// agnostic: seeding is done by the driver glue (it reads `ROCK_TEST_SEED`), not
/// here. `random()` draws one 32-bit word / 2^32 in [0,1); `random_int(min,max)`
/// is `[min,max)` exclusive (`r=max-min`, `r<=0 -> min`), single draw for
/// `r<=2^32` else two draws composed into 53 bits then `% r` (range coverage is
/// a semantic, not a nicety).
pub const JS_RNG_RUNTIME: &str = r#"
var __rock_rng_state = 0;
function __rock_rng_seed(s) { __rock_rng_state = s >>> 0; }
function __rock_rng_state_get() { return __rock_rng_state; }
function __rock_rng_state_set(s) { __rock_rng_state = s >>> 0; }
function __rock_rng_next32() {
  __rock_rng_state = (__rock_rng_state + 0x6D2B79F5) >>> 0;
  var t = __rock_rng_state;
  t = Math.imul(t ^ (t >>> 15), t | 1);
  t = (t + Math.imul(t ^ (t >>> 7), t | 61)) ^ t;
  return (t ^ (t >>> 14)) >>> 0;
}
function __rock_random() { return __rock_rng_next32() / 4294967296; }
function __rock_random_int(min, max) {
  var r = max - min;
  if (r <= 0) return min;
  if (r <= 4294967296) return min + (__rock_rng_next32() % r);
  var hi = __rock_rng_next32(), lo = __rock_rng_next32();
  return min + ((hi * 2097152 + (lo >>> 11)) % r);
}
"#;

/// The static browser runtime prelude for `rock web` (stone-web Arc 2). A leaf
/// layer only: it owns the raw-JS globals the engine's `extern(js)`/`inline(js)`
/// web leaves bind to (WebGL2 context boot, the DOM event queue, the rAF driver,
/// fetch preload, the canvas2d glyph-atlas rasterizer) -- everything that CAN
/// be Rock stays Rock (unfuck stance: no parallel engine in raw JS).
///
/// Environment seam: `__sw_env()` returns the GL/DOM facade. The REAL one boots
/// WebGL2 + DOM listeners; a test harness may predefine
/// `globalThis.__STONE_WEB_FAKE_ENV` (the node fake-GL smoke) and everything
/// below runs against it unchanged -- the per-frame GL sequence is then a
/// recorded-call assertion, not a mock of the engine.
pub const JS_WEB_RUNTIME: &str = r##"
// --- browser print/put (no node process.*) ---
function print() {
  var p = [];
  for (var i = 0; i < arguments.length; i++) p.push(lua_tostring(arguments[i]));
  console.log(p.join("\t"));
}
function put(s) { console.log(s); }
// --- env/config: a browser bundle has no process.env; a config object is
// swapped in by the boot line (M3+). Unset -> Rock null, matching os.getenv. ---
var __ROCK_WEB_CONFIG = (typeof globalThis.__ROCK_WEB_CONFIG === "object" && globalThis.__ROCK_WEB_CONFIG) || {};
function __rock_env_var(n) {
  var v = __ROCK_WEB_CONFIG[n];
  return v === undefined ? null : v;
}
// --- asset preload registry: the boot line fetches declared assets into memory
// BEFORE main() runs, so the sync `stone_file_read`-shape reads stay sync. ---
var __ROCK_WEB_ASSETS = (typeof globalThis.__ROCK_WEB_ASSETS === "object" && globalThis.__ROCK_WEB_ASSETS) || {};

// ===========================================================================
// stone web runtime state
// ===========================================================================
var __SW = {
  gl: null, canvas: null,
  logicalW: 0, logicalH: 0, dpr: 1,
  // GameConfig.resizable, carried from stone_create_window_ex: decides whether
  // the drawable buffer follows the container (see __sw_sync_canvas_size).
  resizable: false,
  initialized: false, quitReq: false, resized: false,
  events: [], mouseX: 0, mouseY: 0,
  // Polled input state (stone_is_key_down / _is_mouse_button_down): keyed by the
  // SDL key STRING (sdl_keycode_to_string vocabulary) and 1-based button index.
  keysDown: {}, mouseButtons: {},
  // one handle space for images AND canvas textures: handle -> WebGLTexture
  textures: {}, nextTex: 1, whiteImage: 0,
  // images: boot decodes path -> ImageBitmap (no GL yet); stone_load_image
  // creates the GL texture lazily+synchronously (GL exists by @init).
  // handle -> {w,h} feeds get_image_size.
  imageBitmaps: {}, imageSizes: {},
  imageData: {}, nextImageData: 0, imageRepeat: {}, samplers: {},
  canvases: {}, nextCanvas: 1, boundCanvas: 0, // 0 = default framebuffer
  shaders: {}, nextShader: 1, curShader: 0,
  meshVbo: null, // shared STREAM_DRAW vertex buffer
  rafId: 0,
  // WebAudio: lazily-created context + decoded-buffer handle table. Absent in a
  // no-AudioContext env (node fake-GL) -> every audio fn stays a silent stub.
  audio: { ctx: null, unavailable: false, resumeArmed: false, sounds: {}, nextSound: 1,
           sfxGain: null, musicGain: null, sfxVol: 1, musicVol: 1, musicSrc: null, sfxSources: {} },
};

function __sw_gl() { return __SW.gl; }

// SDL keycode from a DOM KeyboardEvent (lib.rock sdl_keycode_to_string is the
// inverse table games compare against).
function __sw_sdl_keycode(e) {
  var k = e.key;
  if (k.length === 1) {
    var c = k.charCodeAt(0);
    if (c >= 65 && c <= 90) return c + 32; // letters arrive lowercase in SDL
    if (c >= 32 && c <= 126) return c;
    return 0;
  }
  switch (k) {
    case "Escape": return 27;
    case "Enter": return 13;
    case "Tab": return 9;
    case "Backspace": return 8;
    case "Delete": return 127;
    case "ArrowRight": return 1073741903;
    case "ArrowLeft": return 1073741904;
    case "ArrowDown": return 1073741905;
    case "ArrowUp": return 1073741906;
    case "Shift": return e.location === 2 ? 1073742053 : 1073742049;
    case "Control": return e.location === 2 ? 1073742052 : 1073742048;
    case "Alt": return e.location === 2 ? 1073742054 : 1073742050;
  }
  if (k.length >= 2 && k[0] === "F") {
    var n = parseInt(k.slice(1), 10);
    if (n >= 1 && n <= 12) return 0x4000003A + (n - 1);
  }
  return 0;
}

function __sw_push_event(type, key, btn, x, y) {
  __SW.events.push({ type: type, key: key, mouse_button: btn, mouse_x: x, mouse_y: y });
}

// Mirror of lib.rock sdl_keycode_to_string: the polled key-state map is keyed by
// this string so stone_is_key_down(name) matches the native vocabulary.
function __sw_keycode_to_string(kc) {
  if (kc >= 97 && kc <= 122) return String.fromCharCode(kc); // a-z
  if (kc >= 48 && kc <= 57) return String.fromCharCode(kc);  // 0-9
  switch (kc) {
    case 32: return "space"; case 27: return "escape"; case 13: return "return";
    case 9: return "tab"; case 8: return "backspace"; case 127: return "delete";
    case 1073742049: return "lshift"; case 1073742053: return "rshift";
    case 1073742048: return "lctrl"; case 1073742052: return "rctrl";
    case 1073742050: return "lalt"; case 1073742054: return "ralt";
    case 1073741906: return "up"; case 1073741905: return "down";
    case 1073741904: return "left"; case 1073741903: return "right";
  }
  if (kc >= 0x4000003A && kc <= 0x4000003A + 11) return "f" + (kc - 0x4000003A + 1);
  if (kc >= 33 && kc <= 126) return String.fromCharCode(kc);
  return "" + kc;
}

// --- lifecycle ---
// Lazily create the AudioContext + sfx/music gain graph. Returns null (and
// pins `unavailable`) in a no-AudioContext env so every audio fn stays silent.
function __sw_audio_ctx() {
  var A = __SW.audio;
  if (A.ctx || A.unavailable) return A.ctx;
  var AC = (typeof AudioContext !== "undefined" && AudioContext) ||
           (typeof webkitAudioContext !== "undefined" && webkitAudioContext) || null;
  if (!AC) { A.unavailable = true; __sw_log_once("audio: no AudioContext in this environment; sound disabled"); return null; }
  A.ctx = new AC();
  A.sfxGain = A.ctx.createGain(); A.sfxGain.gain.value = A.sfxVol; A.sfxGain.connect(A.ctx.destination);
  A.musicGain = A.ctx.createGain(); A.musicGain.gain.value = A.musicVol; A.musicGain.connect(A.ctx.destination);
  // Autoplay policy: the context starts suspended; resume on the first user
  // gesture (the game's own DOM listeners fire independently).
  if (typeof window !== "undefined" && !A.resumeArmed) {
    A.resumeArmed = true;
    var resume = function () { if (A.ctx && A.ctx.state === "suspended") A.ctx.resume(); };
    window.addEventListener("pointerdown", resume);
    window.addEventListener("keydown", resume);
  }
  return A.ctx;
}
// Decode registry bytes into an AudioBuffer behind `id`. decodeAudioData
// DETACHES its input ArrayBuffer, so decode a COPY (the registry Uint8Array is
// shared with file_read). Async: the buffer fills in before first playback
// (loads happen at @early_init, plays on later input).
function __sw_audio_decode(id, path) {
  var ctx = __sw_audio_ctx();
  if (!ctx) return 0;
  var bytes = __sw_file_get(path);
  if (bytes === undefined) { __sw_log_once("stone_load_sound: no preloaded bytes for " + path); return 0; }
  var A = __SW.audio;
  if (!A.sounds[id]) A.sounds[id] = { buffer: null };
  var u8 = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
  ctx.decodeAudioData(u8.slice().buffer,
    function (buf) { A.sounds[id].buffer = buf; },
    function (e) { __sw_log_once("stone_load_sound: decode failed for " + path + " (" + (e && e.message) + ")"); });
  return id;
}
function stone_audio_init() { __sw_audio_ctx(); return true; }
function stone_quit() { __SW.quitReq = true; }
function stone_quit_requested() { return __SW.quitReq; }
function stone_is_initialized() { return __SW.initialized; }
function stone_get_backend_name_js() { return "stone-web"; }

function stone_create_window_ex(title, x, y, width, height, display, resizable, fullscreen) {
  if (fullscreen) { __sw_log_once("stone_create_window_ex: fullscreen=" + fullscreen + " ignored; the browser only enters fullscreen from a user gesture, so the canvas boots windowed"); }
  __SW.resizable = !!resizable;
  var fake = globalThis.__STONE_WEB_FAKE_ENV;
  if (fake) {
    __SW.gl = fake.gl;
    __SW.logicalW = width; __SW.logicalH = height; __SW.dpr = fake.dpr || 1;
    __SW.initialized = true;
    return { __sw_window: true };
  }
  var canvas = document.getElementById("stone-canvas");
  if (!canvas) {
    canvas = document.createElement("canvas");
    canvas.id = "stone-canvas";
    document.body.appendChild(canvas);
  }
  var dpr = window.devicePixelRatio || 1;
  canvas.style.width = width + "px";
  canvas.style.height = height + "px";
  canvas.width = Math.round(width * dpr);
  canvas.height = Math.round(height * dpr);
  var gl = canvas.getContext("webgl2", { alpha: false, antialias: false, preserveDrawingBuffer: true });
  if (!gl) { console.error("[STONE-WEB] WebGL2 unavailable"); return null; }
  gl.getExtension("EXT_color_buffer_float");
  __SW.canvas = canvas; __SW.gl = gl;
  __SW.logicalW = width; __SW.logicalH = height; __SW.dpr = dpr;
  if (title) document.title = title;

  window.addEventListener("keydown", function (e) {
    var kc = __sw_sdl_keycode(e);
    if (kc !== 0) { __SW.keysDown[__sw_keycode_to_string(kc)] = true; __sw_push_event(2, kc, 0, 0, 0); e.preventDefault(); }
  });
  window.addEventListener("keyup", function (e) {
    var kc = __sw_sdl_keycode(e);
    if (kc !== 0) { __SW.keysDown[__sw_keycode_to_string(kc)] = false; __sw_push_event(3, kc, 0, 0, 0); }
  });
  canvas.addEventListener("mousemove", function (e) {
    var r = canvas.getBoundingClientRect();
    __SW.mouseX = e.clientX - r.left; __SW.mouseY = e.clientY - r.top;
  });
  canvas.addEventListener("mousedown", function (e) {
    __SW.mouseButtons[e.button + 1] = true;
    __sw_push_event(4, 0, e.button + 1, Math.round(__SW.mouseX), Math.round(__SW.mouseY));
  });
  canvas.addEventListener("mouseup", function (e) {
    __SW.mouseButtons[e.button + 1] = false;
    __sw_push_event(5, 0, e.button + 1, Math.round(__SW.mouseX), Math.round(__SW.mouseY));
  });
  canvas.addEventListener("contextmenu", function (e) { e.preventDefault(); });
  canvas.addEventListener("wheel", function (e) {
    // No MOUSEWHEEL member in the C enum: the delta rides in mouse_y (type=6).
    // DOM deltaY<0 = scroll up = SDL +1.
    __sw_push_event(6, 0, 0, 0, e.deltaY < 0 ? 1 : -1);
    e.preventDefault();
  }, { passive: false });
  window.addEventListener("beforeunload", function () { __SW.quitReq = true; });

  __SW.initialized = true;
  return { __sw_window: true };
}

// Per-rAF: keep the drawable buffer at logical-extent x dpr.
//
// Only a RESIZABLE game takes its logical extent from the container. A
// non-resizable one is authored in absolute pixels (games/sample-game-b,
// games/sample-game lay out against a literal 1280x720) and a responsive
// container would otherwise move screen_size() out from under it every frame.
//
// dpr is tracked for both: density changes the buffer's resolution, not the
// logical extent games measure against, so honoring it costs a non-resizable
// game nothing and is what keeps the output-density font atlas correct on HiDPI.
function __sw_sync_canvas_size() {
  var canvas = __SW.canvas;
  if (!canvas || typeof window === "undefined") return;
  var logicalW = __SW.logicalW;
  var logicalH = __SW.logicalH;
  if (__SW.resizable) {
    var rect = canvas.getBoundingClientRect();
    logicalW = Math.max(1, Math.round(rect.width));
    logicalH = Math.max(1, Math.round(rect.height));
  }
  var dpr = window.devicePixelRatio || 1;
  var drawableW = Math.round(logicalW * dpr);
  var drawableH = Math.round(logicalH * dpr);
  if (__SW.logicalW === logicalW && __SW.logicalH === logicalH && __SW.dpr === dpr
      && canvas.width === drawableW && canvas.height === drawableH) return;
  __SW.logicalW = logicalW; __SW.logicalH = logicalH; __SW.dpr = dpr;
  canvas.width = drawableW; canvas.height = drawableH;
  __SW.resized = true;
}

// --- events / input ---
function stone_poll_event(ev) {
  var q = __SW.events;
  if (q.length === 0) return false;
  var e = q.shift();
  ev.type = e.type; ev.key = e.key; ev.mouse_button = e.mouse_button;
  ev.mouse_x = e.mouse_x; ev.mouse_y = e.mouse_y;
  return true;
}
function stone_get_mouse_position(px, py) { px[0] = Math.round(__SW.mouseX); py[0] = Math.round(__SW.mouseY); }
function stone_get_global_mouse_position(px, py) { px[0] = Math.round(__SW.mouseX); py[0] = Math.round(__SW.mouseY); }
function stone_is_key_down(key) { return !!__SW.keysDown[key]; }
function stone_is_mouse_button_down(button) { return !!__SW.mouseButtons[button]; }
// No separate scancode state on web; treat a pressed key-string equivalently.

// --- window / time ---
function stone_get_window_size(pw, ph) { pw[0] = __SW.logicalW; ph[0] = __SW.logicalH; }
function stone_get_drawable_size(pw, ph) {
  pw[0] = Math.round(__SW.logicalW * __SW.dpr); ph[0] = Math.round(__SW.logicalH * __SW.dpr);
}
// A canvas has no notch: the insets are zero, as on desktop. A viewport that
// grew one would read env(safe-area-inset-*) off the document element here.
function stone_get_safe_area(pt, pr, pb, pl) { pt[0] = 0; pr[0] = 0; pb[0] = 0; pl[0] = 0; }
function stone_get_dpi_scale() { return __SW.dpr; }
function stone_take_window_resized() { var r = __SW.resized; __SW.resized = false; return r; }
function stone_gl_finish() { if (__SW.gl && __SW.gl.finish) __SW.gl.finish(); }
function stone_gl_flush() { if (__SW.gl && __SW.gl.flush) __SW.gl.flush(); }

// --- sprite system / mesh draw ---
function stone_sprite_init() {
  var gl = __SW.gl;
  if (!gl) return false;
  __SW.meshVbo = gl.createBuffer();
  gl.disable(gl.DEPTH_TEST);
  gl.enable(gl.BLEND);
  gl.blendFunc(gl.SRC_ALPHA, gl.ONE_MINUS_SRC_ALPHA);
  return true;
}

// The layout is the CALLER's, same contract as the native stone_draw_mesh:
// component counts in location order, offsets accumulated from them. Nothing
// here states the sprite vertex; the Rock `Vertex` struct owns it and the
// caller derives both numbers from it. Returns -1 for no draw, 0 for an
// untextured draw, and 1 when the texture was bound.
function stone_draw_mesh(data, count, stride, attr_sizes, attr_count, tex) {
  var gl = __SW.gl;
  if (!gl || count <= 0) return -1;
  var sh = __SW.shaders[__SW.curShader];
  if (!sh) {
    __sw_log_once("stone_draw_mesh: no shader bound; skipping");
    return -1;
  }
  gl.useProgram(sh.prog);
  var t = __SW.textures[tex];
  var textureBound = false;
  if (t) {
    gl.activeTexture(gl.TEXTURE0);
    gl.bindTexture(gl.TEXTURE_2D, t);
    if (__SW.unitInt) __SW.unitInt[0] = false;
    textureBound = true;
  }
  __sw_fix_stale_int_units(gl, sh);
  gl.bindBuffer(gl.ARRAY_BUFFER, __SW.meshVbo);
  gl.bufferData(gl.ARRAY_BUFFER, data.subarray(0, count * stride), gl.STREAM_DRAW);
  var sum = 0;
  for (var i = 0; i < attr_count; i++) {
    var n = attr_sizes[i];
    if (n < 1 || n > 4) {
      __sw_log_once("stone_draw_mesh: attr sizes must be 1..4, got " + n);
      return -1;
    }
    sum += n;
  }
  if (sum !== stride) {
    __sw_log_once("stone_draw_mesh: attr sizes sum to " + sum + " but stride is " + stride);
    return -1;
  }
  var sb = stride * 4;
  var off = 0;
  for (var i = 0; i < attr_count; i++) {
    gl.vertexAttribPointer(i, attr_sizes[i], gl.FLOAT, false, sb, off * 4);
    gl.enableVertexAttribArray(i);
    off += attr_sizes[i];
  }
  gl.drawArrays(gl.TRIANGLES, 0, count);
  __sw_mark_drawn(sh);
  return textureBound ? 1 : 0;
}

var __sw_logged = {};
function __sw_log_once(msg) {
  if (__sw_logged[msg]) return;
  __sw_logged[msg] = true;
  console.error("[STONE-WEB] " + msg);
}

function stone_set_canvas_active(h, w, hh) {
  var c = __SW.canvases[h];
  if (!c) return;
  c.activeW = Math.min(Math.max(w, 1), c.w);
  c.activeH = Math.min(Math.max(hh, 1), c.h);
}
function __sw_target_size() {
  if (__SW.boundCanvas !== 0) {
    var c = __SW.canvases[__SW.boundCanvas];
    if (c) return [c.activeW || c.w, c.activeH || c.h];
  }
  return [Math.round(__SW.logicalW * __SW.dpr), Math.round(__SW.logicalH * __SW.dpr)];
}
// Ortho size for the bound target. The default FB is LOGICAL, not backing:
// geometry, screen_size() and mouse are all logical units, and the viewport
// (backing-sized) does the dpr upscale. A backing-sized ortho here renders the
// scene at 1/dpr and desyncs clicks from pixels on any dpr != 1 display.
function __sw_ortho_size() {
  if (__SW.boundCanvas !== 0) {
    var c = __SW.canvases[__SW.boundCanvas];
    // ACTIVE, not capacity: binding viewports and scissors to the active rect
    // and the ortho has to describe that same rectangle, or the world projects
    // to one place and rasterizes at another (stone-rs reports the active
    // extent as CURRENT_TARGET_SIZE for exactly this reason).
    if (c) return [c.activeW || c.w, c.activeH || c.h];
  }
  return [__SW.logicalW, __SW.logicalH];
}

// The camera matrices are built in Rock and arrive in the ENGINE block at
// binding 0, so there is no camera math here any more -- only the bound
// target's size, which Rock derives its ortho from.
function stone_current_target_width() { return __sw_ortho_size()[0]; }
function stone_current_target_height() { return __sw_ortho_size()[1]; }

// --- textures / images ---
function __sw_new_texture() {
  var gl = __SW.gl;
  var t = gl.createTexture();
  var h = __SW.nextTex++;
  __SW.textures[h] = t;
  return h;
}
function stone_get_white_image() {
  if (__SW.whiteImage !== 0) return __SW.whiteImage;
  var gl = __SW.gl;
  var h = __sw_new_texture();
  gl.bindTexture(gl.TEXTURE_2D, __SW.textures[h]);
  gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, 1, 1, 0, gl.RGBA, gl.UNSIGNED_BYTE, new Uint8Array([255,255,255,255]));
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.NEAREST);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.NEAREST);
  __SW.whiteImage = h;
  return h;
}
function stone_get_image_size(img, pw, ph) {
  var s = __SW.imageSizes[img];
  if (s) { pw[0] = s.w; ph[0] = s.h; return; }
  pw[0] = 1; ph[0] = 1; // white fallback / canvas texture default
}

// 0 = NEAREST, 1 = LINEAR. Anything else is a caller bug, not a default:
// silently picking one filter is invisible until it magnifies. Twin of
// stone-rs's gl_filter_for.
function __sw_gl_filter(mode) {
  var gl = __SW.gl;
  if (mode === 0) return gl.NEAREST;
  if (mode === 1) return gl.LINEAR;
  throw new Error("unknown texture filter mode " + mode);
}
// Sync texture creation from a boot-decoded ImageBitmap (handle IS the texture,
// js_ffi.rock:stone_get_image_texture identity). A registry miss is LOUD and
// returns 0, the same failure value stone-rs returns: handing back the WHITE
// PIXEL's handle would attach the missing asset's declared filter to the shared
// white pixel, so one absent Linear asset re-filtered every draw that uses it.
// (The node fake-GL env has no fetch, so every load hits this rung.)
function stone_load_image(path, filterMode) {
  var bmp = __SW.imageBitmaps[path];
  if (bmp === undefined) {
    __sw_log_once("stone_load_image: no preloaded image for " + path);
    return 0;
  }
  var gl = __SW.gl;
  var h = __sw_new_texture();
  var filt = __sw_gl_filter(filterMode);
  gl.bindTexture(gl.TEXTURE_2D, __SW.textures[h]);
  gl.pixelStorei(gl.UNPACK_FLIP_Y_WEBGL, false);
  gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, bmp);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, filt);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, filt);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
  __SW.imageSizes[h] = { w: bmp.width, h: bmp.height };
  return h;
}
// Whether a sampler is bound on `unit` -- the twin of stone-rs's
// stone_sampler_bound. See the note there; the sampler_release sim pins it.
function stone_sampler_bound(unit) {
  var gl = __SW.gl;
  if (!gl || unit < 0) return false;
  gl.activeTexture(gl.TEXTURE0 + unit);
  var bound = gl.getParameter(gl.SAMPLER_BINDING);
  gl.activeTexture(gl.TEXTURE0);
  return !!bound;
}
// Every unit glActiveTexture can select -- the twin of stone-rs's
// stone_max_texture_units, and 0 with no context so a scan's denominator
// assertion fails instead of its loop silently emptying.
function stone_max_texture_units() {
  var gl = __SW.gl;
  if (!gl) return 0;
  return gl.getParameter(gl.MAX_COMBINED_TEXTURE_IMAGE_UNITS) | 0;
}
// Immutable sampler objects, one per (filter, wrap) pair, created once and
// never mutated -- the twin of stone-rs's SAMPLERS. `image` selects the wrap
// half: a bound sampler overrides the texture's own wrap, so one that disagrees
// would silently clamp a REPEAT texture (trails); pass 0 for a canvas, whose
// texture is always CLAMP_TO_EDGE. mode < 0 releases `unit` back to
// texture-object filtering, which the paths that bind a texture without coming
// through here still use.
function stone_bind_sampler(unit, mode, image) {
  var gl = __SW.gl;
  if (!gl || unit < 0) return;
  if (mode < 0) { gl.bindSampler(unit, null); return; }
  var repeat = __SW.imageRepeat[image] ? 1 : 0;
  var filt = (mode === 1) ? 1 : 0;
  var key = filt + ":" + repeat;
  var s = __SW.samplers[key];
  if (!s) {
    s = gl.createSampler();
    var f = filt ? gl.LINEAR : gl.NEAREST;
    var w = repeat ? gl.REPEAT : gl.CLAMP_TO_EDGE;
    gl.samplerParameteri(s, gl.TEXTURE_MIN_FILTER, f);
    gl.samplerParameteri(s, gl.TEXTURE_MAG_FILTER, f);
    gl.samplerParameteri(s, gl.TEXTURE_WRAP_S, w);
    gl.samplerParameteri(s, gl.TEXTURE_WRAP_T, w);
    __SW.samplers[key] = s;
  }
  gl.bindSampler(unit, s);
}
function stone_image_set_wrap(h, repeat) {
  var gl = __SW.gl, t = __SW.textures[h];
  if (!t) return false;
  __SW.imageRepeat[h] = !!repeat;
  var mode = repeat ? gl.REPEAT : gl.CLAMP_TO_EDGE;
  gl.bindTexture(gl.TEXTURE_2D, t);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, mode);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, mode);
  return true;
}
function stone_unload_image(h) {
  var t = __SW.textures[h];
  if (t && __SW.gl) __SW.gl.deleteTexture(t);
  delete __SW.textures[h];
  delete __SW.imageSizes[h];
  delete __SW.imageRepeat[h];
}
function stone_reload_image(h, path) { return false; }
// No deferred-decode queue on web: the browser decodes into the texture at load
// time, so there is never anything outstanding to flush.
function stone_flush_image_uploads() {}
// CPU-side pixel buffers (image_data.rs's twin). These were stubs returning 0,
// which is the worst shape available: a caller got a handle-shaped 0, uploaded
// it, and silently drew the white fallback with no log line anywhere. A real
// buffer is a few lines; the codec-backed PNG entry points below stay
// unimplemented and say so once.
function __sw_byte(v) { v = Math.round(v * 255); return v < 0 ? 0 : v > 255 ? 255 : v; }
function stone_create_image_data(w, h) {
  if (w <= 0 || h <= 0) return 0;
  var id = ++__SW.nextImageData;
  __SW.imageData[id] = { w: w, h: h, px: new Uint8Array(w * h * 4) };
  return id;
}
function stone_destroy_image_data(h) { delete __SW.imageData[h]; }
function stone_image_data_clone(src) {
  var s = __SW.imageData[src];
  if (!s) return 0;
  var id = stone_create_image_data(s.w, s.h);
  if (id) __SW.imageData[id].px.set(s.px);
  return id;
}
function stone_image_data_copy_into(dst, src) {
  var d = __SW.imageData[dst], s = __SW.imageData[src];
  if (!d || !s || d.w !== s.w || d.h !== s.h) return false;
  d.px.set(s.px);
  return true;
}
function stone_image_data_set_pixel(h, x, y, r, g, b, a) {
  var d = __SW.imageData[h];
  if (!d || x < 0 || y < 0 || x >= d.w || y >= d.h) return;
  var o = (y * d.w + x) * 4;
  d.px[o] = __sw_byte(r); d.px[o + 1] = __sw_byte(g);
  d.px[o + 2] = __sw_byte(b); d.px[o + 3] = __sw_byte(a);
}
function stone_image_data_get_pixel(h, x, y, pr, pg, pb, pa) {
  var d = __SW.imageData[h];
  if (!d || x < 0 || y < 0 || x >= d.w || y >= d.h) {
    pr[0] = 0; pg[0] = 0; pb[0] = 0; pa[0] = 0;
    return;
  }
  var o = (y * d.w + x) * 4;
  pr[0] = d.px[o] / 255; pg[0] = d.px[o + 1] / 255;
  pb[0] = d.px[o + 2] / 255; pa[0] = d.px[o + 3] / 255;
}
function stone_image_data_fill_rgba(h, rgba, n) {
  var d = __SW.imageData[h];
  if (!d || !rgba) return;
  var len = Math.min(n * 4, d.px.length, rgba.length);
  d.px.set(rgba.subarray(0, len));
}
// fm < 0 leaves the sampler state alone -- an in-place update must not silently
// reset the filter the texture was created with.
function __sw_upload_image_data(h, d, fm) {
  var gl = __SW.gl;
  gl.bindTexture(gl.TEXTURE_2D, __SW.textures[h]);
  gl.pixelStorei(gl.UNPACK_FLIP_Y_WEBGL, false);
  gl.pixelStorei(gl.UNPACK_ALIGNMENT, 1);
  gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, d.w, d.h, 0, gl.RGBA, gl.UNSIGNED_BYTE, d.px);
  if (fm >= 0) {
    var filt = __sw_gl_filter(fm);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, filt);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, filt);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
  }
  __SW.imageSizes[h] = { w: d.w, h: d.h };
}
function stone_new_image_from_data(dh, fm) {
  var d = __SW.imageData[dh];
  if (!d) { __sw_log_once("stone_new_image_from_data: no ImageData " + dh); return 0; }
  var h = __sw_new_texture();
  __sw_upload_image_data(h, d, fm);
  return h;
}
function stone_image_update_from_data(ih, dh) {
  var d = __SW.imageData[dh];
  if (!d || !__SW.textures[ih]) return false;
  var s = __SW.imageSizes[ih];
  if (s && (s.w !== d.w || s.h !== d.h)) return false;
  __sw_upload_image_data(ih, d, -1);
  return true;
}
function stone_image_data_load_png(path) {
  __sw_log_once("stone_image_data_load_png unimplemented on the web (" + path + ")");
  return 0;
}
function stone_image_data_save_png(h, path) {
  __sw_log_once("stone_image_data_save_png unimplemented on the web (" + path + ")");
  return false;
}
function stone_save_png_rgba8(path, w, h, pixels) {
  __sw_log_once("stone_save_png_rgba8 unimplemented on the web (" + path + ")");
  return false;
}

// --- canvases (FBOs) ---
function stone_create_canvas(w, h, format, depth) {
  var gl = __SW.gl;
  var fbo = gl.createFramebuffer();
  var texHandle = __sw_new_texture();
  var tex = __SW.textures[texHandle];
  gl.bindTexture(gl.TEXTURE_2D, tex);
  // formats: 0 rgba8, 1 rgba16f, 2 rgba32f, 3 r32i (bindings.rock codes)
  var internal = gl.RGBA8, fmt = gl.RGBA, type = gl.UNSIGNED_BYTE;
  if (format === 1) { internal = gl.RGBA16F; fmt = gl.RGBA; type = gl.HALF_FLOAT; }
  else if (format === 2) { internal = gl.RGBA32F; fmt = gl.RGBA; type = gl.FLOAT; }
  else if (format === 3) { internal = gl.R32I; fmt = gl.RED_INTEGER; type = gl.INT; }
  gl.texImage2D(gl.TEXTURE_2D, 0, internal, w, h, 0, fmt, type, null);
  if (format === 3) (__SW.intTextures = __SW.intTextures || {})[texHandle] = true;
  // Sampling is per-draw sampler state; the texture object's own filter is
  // never observed by a sampled bind.
  var filter = gl.NEAREST;
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, filter);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, filter);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
  gl.bindFramebuffer(gl.FRAMEBUFFER, fbo);
  gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT0, gl.TEXTURE_2D, tex, 0);
  // The depth attachment is a TEXTURE, not a renderbuffer, so the
  // depth-visualize overlay can sample the world pass's own depth. It goes in
  // the same __SW.textures registry as any other texture, which is what lets
  // stone_bind_texture_to_unit take it.
  var depthTexHandle = 0;
  if (depth) {
    depthTexHandle = __sw_new_texture();
    gl.bindTexture(gl.TEXTURE_2D, __SW.textures[depthTexHandle]);
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.DEPTH_COMPONENT24, w, h, 0,
                  gl.DEPTH_COMPONENT, gl.UNSIGNED_INT, null);
    // Depth textures are not filterable and carry no mips: the default MIPMAP
    // min filter makes them INCOMPLETE, and every sample reads black -- which
    // is indistinguishable from "near".
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.NEAREST);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.NEAREST);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_COMPARE_MODE, gl.NONE);
    gl.bindTexture(gl.TEXTURE_2D, null);
    gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.DEPTH_ATTACHMENT, gl.TEXTURE_2D,
                            __SW.textures[depthTexHandle], 0);
  }
  var ok = gl.checkFramebufferStatus(gl.FRAMEBUFFER) === gl.FRAMEBUFFER_COMPLETE;
  gl.bindFramebuffer(gl.FRAMEBUFFER, null);
  if (!ok) { __sw_log_once("stone_create_canvas: incomplete FBO (format " + format + ")"); }
  var handle = __SW.nextCanvas++;
  __SW.canvases[handle] = { fbo: fbo, tex: texHandle, depthTex: depthTexHandle, w: w, h: h, format: format };
  return handle;
}
function stone_destroy_canvas(h) {
  var c = __SW.canvases[h];
  if (!c) return;
  var gl = __SW.gl;
  gl.deleteFramebuffer(c.fbo);
  gl.deleteTexture(__SW.textures[c.tex]);
  delete __SW.textures[c.tex];
  if (c.depthTex) {
    gl.deleteTexture(__SW.textures[c.depthTex]);
    delete __SW.textures[c.depthTex];
  }
  delete __SW.canvases[h];
}
function stone_set_canvas(h) {
  var gl = __SW.gl;
  if (h === 0) {
    gl.bindFramebuffer(gl.FRAMEBUFFER, null);
    __SW.boundCanvas = 0;
  } else {
    var c = __SW.canvases[h];
    if (!c) { __sw_log_once("stone_set_canvas: unknown canvas " + h); return; }
    gl.bindFramebuffer(gl.FRAMEBUFFER, c.fbo);
    __SW.boundCanvas = h;
  }
  var wh = __sw_target_size();
  gl.viewport(0, 0, wh[0], wh[1]);
  // glClear ignores the viewport but respects the scissor, so the scissor is
  // what keeps a clear (and any overdraw) off the inactive margin.
  var cc = __SW.boundCanvas !== 0 ? __SW.canvases[__SW.boundCanvas] : null;
  if (cc && (wh[0] < cc.w || wh[1] < cc.h)) {
    gl.enable(gl.SCISSOR_TEST);
    gl.scissor(0, 0, wh[0], wh[1]);
  } else {
    gl.disable(gl.SCISSOR_TEST);
  }
}
// The rendering path's canvas verbs: the handle is resolved here and no
// texture name reaches Rock. Twins of the native ones in stone-rs/src/canvas.rs.
function stone_bind_canvas_texture_to_unit(h, unit) {
  var c = __SW.canvases[h];
  return c ? stone_bind_texture_to_unit(c.tex, unit) !== 0 : false;
}
function stone_bind_canvas_depth_to_unit(h, unit) {
  var c = __SW.canvases[h];
  return c && c.depthTex ? stone_bind_texture_to_unit(c.depthTex, unit) !== 0 : false;
}
function stone_canvas_exists(h) { return !!__SW.canvases[h]; }
function stone_canvas_has_depth(h) { var c = __SW.canvases[h]; return !!(c && c.depthTex); }
function stone_sprite_draw_canvas(shader, h, tw, th, x, y, w, hh, sx, sy, sw, sh) {
  var c = __SW.canvases[h];
  if (!c) return;
  stone_sprite_draw_texture(shader, c.tex, tw, th, x, y, w, hh, sx, sy, sw, sh, 0, 1, 1, 1, 1, 1, 1);
}
// GL names, for the imgui path only.
function stone_get_canvas_texture(h) { var c = __SW.canvases[h]; return c ? c.tex : 0; }
function stone_get_canvas_depth_texture(h) { var c = __SW.canvases[h]; return c ? (c.depthTex || 0) : 0; }
// Sub-rect of the bound target, logical top-left px -> GL bottom-origin.
// Reset by re-binding the target, same contract as the native side.
//
// The default framebuffer is BACKING pixels while the caller's rect is logical,
// so it scales by dpr; a canvas is its own pixels and is 1:1. Same split
// `stone_set_canvas` makes, and the same one the native side makes between
// drawable and window size. Passing logical straight through would put the rect
// at 1/dpr size and the wrong place on any dpr != 1 display.
function stone_set_viewport_rect(x, y, w, h) {
  if (w <= 0 || h <= 0) { __sw_log_once("stone_set_viewport_rect: empty rect"); return; }
  var s = __SW.boundCanvas !== 0 ? 1 : __SW.dpr;
  var wh = __sw_target_size();
  var gw = Math.round(w * s), gh = Math.round(h * s);
  __SW.gl.viewport(Math.round(x * s), wh[1] - Math.round(y * s) - gh, gw, gh);
}
// The semantic submission's GpuRect: bottom-left, the bound target's own
// pixels, verbatim. Unlike stone_set_viewport_rect above it neither scales by
// dpr nor flips Y -- the caller stated the rect in GL's own convention.
function stone_set_gl_viewport(x, y, w, h) {
  __SW.gl.viewport(x, y, w, h);
}
// mode: 0 = nearest, 1 = linear. Integer formats refuse -- a linear-filtered
// integer texture samples zero.
function stone_set_canvas_filter(h, mode) {
  if (h === 0) return false;
  var c = __SW.canvases[h];
  if (!c) { __sw_log_once("stone_set_canvas_filter: unknown canvas " + h); return false; }
  if (c.format === 3) {
    __sw_log_once("stone_set_canvas_filter: canvas " + h + " is an integer format -- " +
      "linear filtering makes it incomplete; refusing");
    return false;
  }
  var gl = __SW.gl;
  var filter = mode === 0 ? gl.NEAREST : gl.LINEAR;
  gl.bindTexture(gl.TEXTURE_2D, __SW.textures[c.tex]);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, filter);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, filter);
  gl.bindTexture(gl.TEXTURE_2D, null);
  return true;
}
function stone_clear_canvas(r, g, b, a) {
  var gl = __SW.gl;
  gl.clearColor(r, g, b, a);
  // WebGL2 (unlike desktop GL) makes clear() INVALID_OPERATION when an
  // INTEGER draw buffer is enabled -- the engine's MRT color pass calls this
  // between set_draw_buffers(2) and the explicit clearBufferiv on the id
  // attachment. Narrow the list to [A0] for the float clear and restore; the
  // id attachment's per-frame reset is the engine's clearBufferiv, exactly as
  // on native (where glClear on the int attachment is undefined anyway).
  var c = __SW.canvases[__SW.boundCanvas];
  var narrow = c && c.idTex !== undefined && c.drawBufs === 2;
  if (narrow) gl.drawBuffers([gl.COLOR_ATTACHMENT0]);
  gl.clear(gl.COLOR_BUFFER_BIT);
  if (narrow) gl.drawBuffers([gl.COLOR_ATTACHMENT0, gl.COLOR_ATTACHMENT1]);
}
// Clear one INTEGER draw buffer of the currently bound FBO (glClear is
// undefined/forbidden on integer buffers; R32I id targets reset through this).
function stone_clear_canvas_int(drawBuffer, value) {
  var gl = __SW.gl;
  gl.clearBufferiv(gl.COLOR, drawBuffer, new Int32Array([value, 0, 0, 0]));
}
// MRT: attach the id-canvas's R32I texture as COLOR_ATTACHMENT1 of the
// color-canvas's FBO (mirrors stone-rs canvas.rs: size gate, completeness
// gate with rollback; draw-buffer list stays [A0] -- the color pass toggles).
function stone_canvas_attach_id_target(canvasH, idH) {
  var gl = __SW.gl;
  var c = __SW.canvases[canvasH], idc = __SW.canvases[idH];
  if (!c || !idc) { __sw_log_once("stone_canvas_attach_id_target: unknown handle(s)"); return false; }
  if (c.w !== idc.w || c.h !== idc.h) { __sw_log_once("stone_canvas_attach_id_target: size mismatch"); return false; }
  gl.bindFramebuffer(gl.FRAMEBUFFER, c.fbo);
  gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT1, gl.TEXTURE_2D, __SW.textures[idc.tex], 0);
  var ok = gl.checkFramebufferStatus(gl.FRAMEBUFFER) === gl.FRAMEBUFFER_COMPLETE;
  if (!ok) {
    __sw_log_once("stone_canvas_attach_id_target: FBO incomplete after MRT attach");
    gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT1, gl.TEXTURE_2D, null, 0);
  } else {
    c.idTex = idc.tex;
    c.drawBufs = 1;
  }
  var prev = __SW.canvases[__SW.boundCanvas];
  gl.bindFramebuffer(gl.FRAMEBUFFER, prev ? prev.fbo : null);
  return ok;
}
function stone_set_present_viewport(active, x, y, w, h) {
  var gl = __SW.gl;
  // Args arrive in logical units (PRESENT_OFFSET/SIZE, computed from
  // screen_size()); GL viewports are backing pixels.
  var d = __SW.dpr;
  if (active) gl.viewport(Math.round(x * d), Math.round(y * d), Math.round(w * d), Math.round(h * d));
  else { var wh = __sw_target_size(); gl.viewport(0, 0, wh[0], wh[1]); }
}
function stone_supports_float_canvas() {
  var gl = __SW.gl;
  return !!(gl && gl.getExtension && gl.getExtension("EXT_color_buffer_float"));
}
function stone_clear_depth(depth) {
  var gl = __SW.gl;
  gl.depthMask(true);
  gl.clearDepth(depth);
  gl.clear(gl.DEPTH_BUFFER_BIT);
}
function stone_set_depth_mode(compareMode, write) {
  var gl = __SW.gl;
  if (compareMode === "always" && !write) { gl.disable(gl.DEPTH_TEST); gl.depthMask(false); }
  else if (compareMode === "always") { gl.disable(gl.DEPTH_TEST); gl.depthMask(true); }
  else {
    gl.enable(gl.DEPTH_TEST);
    gl.depthFunc(compareMode === "less" ? gl.LESS : compareMode === "lequal" ? gl.LEQUAL : gl.ALWAYS);
    gl.depthMask(!!write);
  }
}

// --- blend (lib.rs:2095-2138) ---
function stone_set_blend_mode_i(mode) {
  var gl = __SW.gl;
  if (!gl) return;
  gl.blendEquation(gl.FUNC_ADD);
  if (mode === 0) { gl.enable(gl.BLEND); gl.blendFunc(gl.SRC_ALPHA, gl.ONE_MINUS_SRC_ALPHA); }
  else if (mode === 1) { gl.enable(gl.BLEND); gl.blendFunc(gl.SRC_ALPHA, gl.ONE); }
  else if (mode === 2) { gl.enable(gl.BLEND); gl.blendFunc(gl.DST_COLOR, gl.ZERO); }
  else if (mode === 3) { gl.enable(gl.BLEND); gl.blendEquation(gl.MAX); gl.blendFunc(gl.ONE, gl.ONE); }
  else if (mode === 4) { gl.disable(gl.BLEND); }
}

// --- shaders ---
// Split the combined source at the #version line (#define VERTEX / PIXEL,
// shader.rs:59-111) and do the ES rewrite (adapt_source_for_gles: WebGL2 IS
// GLSL ES 3.00): '#version 330 core' -> '#version 300 es' + precision.
// ANGLE (every browser's WebGL2 front-end) enforces the ES reserved-identifier
// rule: no `__` anywhere in an identifier. The unified mangle is `__`-free by
// construction, so no rewrite is needed -- instead assert loudly: a `__` in the
// adapted source is a real bug (a compiler-minted `__` leaked, or a user wrote
// `a__b` in shader Rock), and must fail at compile time, not silently.
function __sw_adapt_es(src) {
  if (src.indexOf("__") >= 0) {
    throw new Error("[STONE-WEB] adapted GLSL contains `__`, which ANGLE (WebGL2) rejects as a " +
      "reserved identifier; the unified mangle is `__`-free, so this is a leaked compiler name " +
      "or a `__` in shader Rock -- offending source:\n" + src);
  }
  if (src.indexOf("#version 330 core") === 0) {
    return "#version 300 es\nprecision highp float;\nprecision highp int;" + src.slice("#version 330 core".length);
  }
  return src;
}
var __sw_shader_errors = 0;
function stone_shader_error_count() { return __sw_shader_errors; }
function __sw_compile_stage(gl, type, src) {
  var s = gl.createShader(type);
  gl.shaderSource(s, src);
  gl.compileShader(s);
  if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) {
    console.error("[STONE-WEB] shader compile failed:\n" + gl.getShaderInfoLog(s) + "\n--- source ---\n" + src);
    __sw_shader_errors++;
    gl.deleteShader(s);
    return null;
  }
  return s;
}
function stone_create_shader_from_combined(src) {
  var gl = __SW.gl;
  if (!gl) return 0;
  var nl = src.indexOf("\n");
  var prefix = nl >= 0 ? src.slice(0, nl + 1) : "";
  var suffix = nl >= 0 ? src.slice(nl + 1) : src;
  var vertSrc = __sw_adapt_es(prefix + "#define VERTEX\n" + suffix);
  var fragSrc = __sw_adapt_es(prefix + "#define PIXEL\n" + suffix);
  var vs = __sw_compile_stage(gl, gl.VERTEX_SHADER, vertSrc);
  if (!vs) return 0;
  var fs = __sw_compile_stage(gl, gl.FRAGMENT_SHADER, fragSrc);
  if (!fs) { gl.deleteShader(vs); return 0; }
  var prog = gl.createProgram();
  gl.attachShader(prog, vs);
  gl.attachShader(prog, fs);
  gl.linkProgram(prog);
  if (!gl.getProgramParameter(prog, gl.LINK_STATUS)) {
    console.error("[STONE-WEB] shader link failed:\n" + gl.getProgramInfoLog(prog));
    __sw_shader_errors++;
    gl.deleteProgram(prog);
    return 0;
  }
  var h = __SW.nextShader++;
  // Float (non-integer) sampler2D uniforms, for the stale-int-unit fix. The
  // fake-GL env has no getActiveUniform -- empty list there.
  var floatSamplers = [];
  if (gl.getActiveUniform) {
    var nu = gl.getProgramParameter(prog, gl.ACTIVE_UNIFORMS) | 0;
    for (var ui = 0; ui < nu; ui++) {
      var info = gl.getActiveUniform(prog, ui);
      if (info && info.type === gl.SAMPLER_2D) floatSamplers.push(info.name);
    }
  }
  __SW.shaders[h] = { prog: prog, ulocs: {}, floatSamplers: floatSamplers, samplerUnits: {} };
  return h;
}
// Owns the program's WHOLE resource set, mirroring the native side. A rejected
// registration candidate is destroyed mid-registration, so it may already have
// bound blocks and allocated their buffers; deleting only the program leaked one
// buffer per block per reload.
function stone_destroy_shader(h) {
  var sh = __SW.shaders[h];
  if (!sh) return;
  if (sh.blocks) {
    for (var i = 0; i < sh.blocks.length; i++) {
      if (sh.blocks[i]) __SW.gl.deleteBuffer(sh.blocks[i].buffer);
    }
  }
  __SW.gl.deleteProgram(sh.prog);
  delete __SW.shaders[h];
}
function __sw_uloc(sh, name) {
  if (!(name in sh.ulocs)) sh.ulocs[name] = __SW.gl.getUniformLocation(sh.prog, name);
  return sh.ulocs[name];
}
function stone_set_shader(h) {
  if (h === 0) { __SW.gl.useProgram(null); __SW.curShader = 0; return; } // unbind, matching native
  var sh = __SW.shaders[h];
  if (!sh) { __sw_log_once("stone_set_shader: unknown shader " + h); return; }
  __SW.gl.useProgram(sh.prog);
  __SW.curShader = h;
  __sw_bind_blocks(h);
}
// std140 uniform blocks. `bindBufferBase` binds to a GLOBAL index, so a
// program's blocks are re-bound whenever that program goes current -- binding
// once at registration would leave whichever program registered last owning
// index 0 for all of them. Mirrors stone-rs/src/uniform_block.rs.
function __sw_bind_blocks(h) {
  var gl = __SW.gl, sh = __SW.shaders[h];
  if (!gl || !sh || !sh.blocks) return;
  for (var i = 0; i < sh.blocks.length; i++) {
    var b = sh.blocks[i];
    if (b) { gl.bindBufferBase(gl.UNIFORM_BUFFER, b.binding, b.buffer); }
  }
}
// Stamped at each draw with the SHADER THAT DRAW WENT THROUGH, not at bind and
// not from `curShader`: `stone_sprite_draw_texture` falls back to a passed
// handle when nothing is current, and a curShader-based stamp cannot see it.
function __sw_mark_drawn(sh) {
  if (!sh || !sh.blocks) return;
  for (var i = 0; i < sh.blocks.length; i++) {
    if (sh.blocks[i]) sh.blocks[i].drawn = true;
  }
}
// A block whose program is drawn with but which nothing ever fills reads zeroes
// in the shader, silently. Both flags are sticky, so this fires only for a block
// never filled across the whole run.
function __sw_report_unfilled_blocks() {
  var shaders = __SW.shaders;
  for (var h in shaders) {
    var sh = shaders[h];
    if (!sh || !sh.blocks) continue;
    for (var i = 0; i < sh.blocks.length; i++) {
      var b = sh.blocks[i];
      if (!b || b.uploaded || !b.drawn || b.reported) continue;
      b.reported = true;
      __sw_log_once("shader `" + b.key + "` was drawn with but nothing ever filled its uniform block `" +
        b.name + "` (binding " + b.binding + ") -- it is reading zeroes.");
    }
  }
}
function stone_unfilled_uniform_block_count() {
  var n = 0, shaders = __SW.shaders;
  for (var h in shaders) {
    var sh = shaders[h];
    if (!sh || !sh.blocks) continue;
    for (var i = 0; i < sh.blocks.length; i++) {
      var b = sh.blocks[i];
      if (b && b.drawn && !b.uploaded) n++;
    }
  }
  return n;
}
function stone_uniform_block_buffer_count() {
  var n = 0;
  for (var k in __SW.shaders) {
    var sh = __SW.shaders[k];
    if (sh && sh.blocks) for (var i = 0; i < sh.blocks.length; i++) if (sh.blocks[i]) n++;
  }
  return n;
}
// The JS runtime keys blocks off the shader record itself, so there is no
// second registry to go stale -- reported as the same number the Rock-side view
// computes, which keeps the cross-view arm meaningful on both tiers.
function stone_block_program_count() {
  var n = 0;
  for (var k in __SW.shaders) {
    var sh = __SW.shaders[k];
    if (sh && sh.blocks && sh.blocks.length) n++;
  }
  return n;
}
function stone_shader_bind_uniform_block(h, key, name, binding, size, fingerprint) {
  var gl = __SW.gl, sh = __SW.shaders[h];
  if (!gl || !sh) { __sw_log_once("bind_uniform_block: no shader " + h); return false; }
  if (!name || size <= 0) { __sw_log_once("bind_uniform_block: null name or non-positive size " + size); return false; }
  var idx = gl.getUniformBlockIndex(sh.prog, name);
  if (idx === gl.INVALID_INDEX) {
    __sw_log_once("bind_uniform_block: shader " + h + " declares no uniform block `" + name + "`");
    return false;
  }
  gl.uniformBlockBinding(sh.prog, idx, binding);
  var buf = gl.createBuffer();
  gl.bindBuffer(gl.UNIFORM_BUFFER, buf);
  // Zero-fill, so a member the packer never writes reads 0 rather than
  // whatever the driver handed back.
  gl.bufferData(gl.UNIFORM_BUFFER, size, gl.DYNAMIC_DRAW);
  gl.bindBuffer(gl.UNIFORM_BUFFER, null);
  if (!sh.blocks) sh.blocks = [];
  // A re-register (hot reload) replaces the buffer at this BINDING; a shrinking
  // program's stale tail is dropped, since registration walks bindings from 0.
  if (binding < sh.blocks.length && sh.blocks[binding]) gl.deleteBuffer(sh.blocks[binding].buffer);
  sh.blocks.length = binding;
  sh.blocks[binding] = { buffer: buf, size: size, binding: binding, fingerprint: fingerprint, key: key, name: name, uploaded: false, drawn: false, reported: false };
  return true;
}
function stone_shader_upload_uniform_block(h, data, size, fingerprint) {
  var gl = __SW.gl, sh = __SW.shaders[h];
  if (!gl || !sh || !sh.blocks) { __sw_log_once("upload_uniform_block: shader " + h + " has no bound block"); return false; }
  if (!data || size <= 0) { __sw_log_once("upload_uniform_block: null data or non-positive size " + size); return false; }
  // Selected by FINGERPRINT: a program can have several blocks, and the packed
  // bytes say which layout they are.
  var b = null;
  for (var i = 0; i < sh.blocks.length; i++) {
    if (sh.blocks[i] && sh.blocks[i].fingerprint === fingerprint) { b = sh.blocks[i]; break; }
  }
  if (!b) { __sw_log_once("upload_uniform_block: shader " + h + " has no block with layout " + fingerprint); return false; }
  if (size !== b.size) {
    __sw_log_once("upload_uniform_block: shader " + h + " upload of " + size + " bytes does not match its " + b.size + " byte block");
    return false;
  }
  gl.bindBuffer(gl.UNIFORM_BUFFER, b.buffer);
  gl.bufferSubData(gl.UNIFORM_BUFFER, 0, data.subarray ? data.subarray(0, size) : data);
  gl.bindBuffer(gl.UNIFORM_BUFFER, null);
  b.uploaded = true;
  return true;
}
function stone_shader_check_uniform_block_layout(h, name, names, names_len, offsets, count, plan_size) {
  var gl = __SW.gl, sh = __SW.shaders[h];
  if (!gl || !sh) { __sw_log_once("check_uniform_block_layout: no shader " + h); return false; }
  var idx = gl.getUniformBlockIndex(sh.prog, name);
  if (idx === gl.INVALID_INDEX) {
    __sw_log_once("check_uniform_block_layout: shader " + h + " declares no block `" + name + "`");
    return false;
  }
  var dataSize = gl.getActiveUniformBlockParameter(sh.prog, idx, gl.UNIFORM_BLOCK_DATA_SIZE);
  if (dataSize > plan_size) {
    __sw_log_once("check_uniform_block_layout: `" + name + "` needs " + dataSize + " bytes, the plan allocates " + plan_size);
    return false;
  }
  var members = names.split(String.fromCharCode(0)).filter(function (x) { return x.length > 0; });
  if (members.length !== count) {
    __sw_log_once("check_uniform_block_layout: " + members.length + " names for " + count + " offsets");
    return false;
  }
  var indices = gl.getUniformIndices(sh.prog, members);
  if (!indices) {
    __sw_log_once("check_uniform_block_layout: `" + name + "` uniform-index query failed");
    return false;
  }
  // Only the ACTIVE indices are queried. An inactive member is optimized out
  // and reports INVALID_INDEX; passing that straight to `getActiveUniforms`
  // makes the query generate an error, and WebGL 2 returns NULL for the whole
  // call rather than a per-entry blank (Khronos WebGL 2 spec, 3.7.5). So one
  // optimized-out member would take down the answer for every member beside it.
  // `map` carries each surviving index back to the member it came from, since
  // the driver's reply is now indexed by position in `active`, not in `members`.
  // The native sibling filters the same way.
  var active = [], map = [];
  for (var i = 0; i < members.length; i++) {
    if (indices[i] === gl.INVALID_INDEX || indices[i] == null) continue;
    active.push(indices[i]);
    map.push(i);
  }
  // ZERO comparisons is not a pass. Every member skipped means the loop below
  // runs zero times and returns its initial true -- a gate arm with no
  // population, reporting success for a block it never looked at. The native
  // sibling rejects here for the same reason, and the two must agree or the web
  // tier silently accepts what the desktop one refuses.
  //
  // The case that makes it load-bearing: an ARRAY block is queried as
  // `name[0]`, and if that spelling ever regresses to the bare name, EVERY
  // member misses at once -- so the block where this matters most is exactly
  // the one where the skip stops being partial coverage.
  if (active.length === 0) {
    __sw_log_once("check_uniform_block_layout: `" + name + "` has no active members -- nothing to verify");
    return false;
  }
  var driver = gl.getActiveUniforms(sh.prog, active, gl.UNIFORM_OFFSET);
  if (!driver) {
    __sw_log_once("check_uniform_block_layout: `" + name + "` uniform-offset query failed");
    return false;
  }
  var ok = true;
  for (var a = 0; a < map.length; a++) {
    if (driver[a] !== offsets[map[a]]) {
      __sw_log_once("check_uniform_block_layout: `" + name + "`.`" + members[map[a]] + "` is at " + driver[a] + ", the plan says " + offsets[map[a]]);
      ok = false;
    }
  }
  return ok;
}
function __sw_with_shader_uniform(h, name, f) {
  var gl = __SW.gl, sh = __SW.shaders[h];
  if (!gl || !sh) return;
  var prev = __SW.curShader;
  gl.useProgram(sh.prog);
  var loc = __sw_uloc(sh, name);
  if (loc) f(gl, loc);
  if (prev !== 0 && prev !== h && __SW.shaders[prev]) gl.useProgram(__SW.shaders[prev].prog);
}
function stone_shader_has_uniform(h, name) {
  var sh = __SW.shaders[h];
  return !!(sh && __sw_uloc(sh, name));
}
function stone_shader_set_float(h, name, v) { __sw_with_shader_uniform(h, name, function (gl, l) { gl.uniform1f(l, v); }); }
function stone_shader_set_int(h, name, v) { __sw_with_shader_uniform(h, name, function (gl, l) { gl.uniform1i(l, v); }); }
function stone_shader_set_bool(h, name, v) { __sw_with_shader_uniform(h, name, function (gl, l) { gl.uniform1i(l, v ? 1 : 0); }); }
function stone_shader_set_vec2(h, name, x, y) { __sw_with_shader_uniform(h, name, function (gl, l) { gl.uniform2f(l, x, y); }); }
function stone_shader_set_vec3(h, name, x, y, z) { __sw_with_shader_uniform(h, name, function (gl, l) { gl.uniform3f(l, x, y, z); }); }
function stone_shader_set_vec4(h, name, x, y, z, w) { __sw_with_shader_uniform(h, name, function (gl, l) { gl.uniform4f(l, x, y, z, w); }); }
function stone_shader_set_mat4(h, name, m) { __sw_with_shader_uniform(h, name, function (gl, l) { gl.uniformMatrix4fv(l, false, m.subarray ? m.subarray(0, 16) : m); }); }
function stone_shader_set_texture(h, name, unit) {
  var sh = __SW.shaders[h];
  if (sh && sh.samplerUnits) sh.samplerUnits[name] = unit;
  __sw_with_shader_uniform(h, name, function (gl, l) { gl.uniform1i(l, unit); });
}
function stone_shader_set_float_array(h, name, values, count) {
  __sw_with_shader_uniform(h, name, function (gl, l) { gl.uniform1fv(l, values.subarray ? values.subarray(0, count) : values); });
}
function stone_shader_set_int_array(h, name, values, count) {
  __sw_with_shader_uniform(h, name, function (gl, l) { gl.uniform1iv(l, values.subarray ? values.subarray(0, count) : values); });
}
function stone_shader_set_vec2_array(h, name, values, count) {
  __sw_with_shader_uniform(h, name, function (gl, l) { gl.uniform2fv(l, values.subarray ? values.subarray(0, count * 2) : values); });
}
function stone_shader_set_vec4_array(h, name, values, count) {
  __sw_with_shader_uniform(h, name, function (gl, l) { gl.uniform4fv(l, values.subarray ? values.subarray(0, count * 4) : values); });
}
function stone_bind_image_texture_to_unit(h, unit) { return stone_bind_texture_to_unit(h, unit) !== 0; }
function stone_bind_texture_to_unit(tex, unit) {
  var gl = __SW.gl, t = __SW.textures[tex];
  if (!gl || !t || unit < 0) return 0;
  gl.activeTexture(gl.TEXTURE0 + unit);
  gl.bindTexture(gl.TEXTURE_2D, t);
  gl.activeTexture(gl.TEXTURE0);
  (__SW.unitInt = __SW.unitInt || {})[unit] = !!(__SW.intTextures && __SW.intTextures[tex]);
  return 1;
}

// WebGL2 (unlike desktop GL) makes ANY draw INVALID_OPERATION when an active
// FLOAT sampler's unit holds an INTEGER texture -- even one the shader never
// actually reads this frame. The engine's outline detect pass leaves its R32I
// inputs bound on shared units (visible_id on 2, the unit sprite_batch's
// uShadowmapTex uses), which desktop GL tolerates. Neutralize before draw:
// rebind the white texture on any float-sampler unit left holding an integer
// texture. Sampler names/types come from link-time introspection; the
// name->unit map tracks stone_shader_set_texture.
function __sw_fix_stale_int_units(gl, sh) {
  var fs = sh.floatSamplers;
  if (!fs || fs.length === 0 || !__SW.unitInt) return;
  for (var i = 0; i < fs.length; i++) {
    var unit = sh.samplerUnits[fs[i]] || 0;
    if (__SW.unitInt[unit]) {
      gl.activeTexture(gl.TEXTURE0 + unit);
      gl.bindTexture(gl.TEXTURE_2D, __SW.textures[stone_get_white_image()]);
      gl.activeTexture(gl.TEXTURE0);
      __SW.unitInt[unit] = false;
    }
  }
}

// --- fullscreen quad (post-processing passes) ---
// Orientation picked by the BOUND TARGET, never the call site (sprite.rs
// stone_draw_fullscreen_quad): the default framebuffer gets the present quad
// (the one constant V-flip), a canvas target gets the identity quad (1:1,
// preserves content) -- so no pass chooses an orientation and the
// mirrored-window class can't recur.
var __sw_fsq_identity = null;
var __sw_fsq_present = null;
var __sw_fsq_scaled = null;
var __sw_fsq_uv = [1, 1];
function stone_set_fullscreen_uv_scale(sx, sy) {
  __sw_fsq_uv = [Math.max(sx, 0), Math.max(sy, 0)];
}
// The imperative fullscreen path's geometry -- the static buffers and the
// scaled respecification read it. The semantic submission does NOT: it goes
// through stone_draw_fullscreen_rect, which takes the corner mapping.
function __sw_fsq_verts(flipped, sx, sy) {
  var v0 = (flipped ? 1 : 0) * sy, v1 = (flipped ? 0 : 1) * sy;
  return new Float32Array([
    -1, -1, 0, v0,  1, -1, sx, v0,  1, 1, sx, v1,
    -1, -1, 0, v0,  1, 1, sx, v1,  -1, 1, 0, v1,
  ]);
}
// THIS tier's tessellation of the fullscreen rectangle for the corner UV
// mapping a semantic submission resolved: (u0,v0) at NDC (-1,-1), (u1,v1) at
// NDC (+1,+1). The order and the diagonal are deliberately NOT the native
// tier's (sprite.rs fs_rect_data splits the other way) -- the mapping is the
// contract, the tessellation is backend implementation data.
function __sw_fsq_rect(u0, v0, u1, v1) {
  return new Float32Array([
    -1, -1, u0, v0,   1, -1, u1, v0,   1, 1, u1, v1,
    -1, -1, u0, v0,   1,  1, u1, v1,  -1, 1, u0, v1,
  ]);
}
// Culling is disabled HERE rather than assumed: the two tiers wind opposite
// ways, so "no culling" is state this draw states.
function stone_draw_fullscreen_rect(u0, v0, u1, v1) {
  var gl = __SW.gl;
  var sh = __SW.shaders[__SW.curShader];
  if (!gl || !sh) return;
  gl.useProgram(sh.prog);
  __sw_fix_stale_int_units(gl, sh);
  if (!__sw_fsq_scaled) __sw_fsq_scaled = gl.createBuffer();
  gl.bindBuffer(gl.ARRAY_BUFFER, __sw_fsq_scaled);
  gl.bufferData(gl.ARRAY_BUFFER, __sw_fsq_rect(u0, v0, u1, v1), gl.DYNAMIC_DRAW);
  gl.disable(gl.CULL_FACE);
  gl.vertexAttribPointer(0, 2, gl.FLOAT, false, 16, 0);
  gl.enableVertexAttribArray(0);
  gl.vertexAttribPointer(1, 2, gl.FLOAT, false, 16, 8);
  gl.enableVertexAttribArray(1);
  gl.disableVertexAttribArray(2);
  gl.disableVertexAttribArray(3);
  gl.disableVertexAttribArray(4);
  gl.drawArrays(gl.TRIANGLES, 0, 6);
  __sw_mark_drawn(sh);
}
function __sw_fsq_buffer(gl, flipped) {
  var b = gl.createBuffer();
  gl.bindBuffer(gl.ARRAY_BUFFER, b);
  gl.bufferData(gl.ARRAY_BUFFER, __sw_fsq_verts(flipped, 1, 1), gl.STATIC_DRAW);
  return b;
}
function stone_draw_fullscreen_quad() {
  var gl = __SW.gl;
  var sh = __SW.shaders[__SW.curShader];
  if (!gl || !sh) return;
  if (!__sw_fsq_identity) {
    __sw_fsq_identity = __sw_fsq_buffer(gl, false);
    __sw_fsq_present = __sw_fsq_buffer(gl, true);
  }
  gl.useProgram(sh.prog);
  __sw_fix_stale_int_units(gl, sh);
  var sx = __sw_fsq_uv[0], sy = __sw_fsq_uv[1];
  if (sx === 1 && sy === 1) {
    gl.bindBuffer(gl.ARRAY_BUFFER, __SW.boundCanvas === 0 ? __sw_fsq_present : __sw_fsq_identity);
  } else {
    // Re-specify the scaled quad; six vertices, and it reaches custom vertex
    // stages the way a uniform would not.
    if (!__sw_fsq_scaled) __sw_fsq_scaled = gl.createBuffer();
    gl.bindBuffer(gl.ARRAY_BUFFER, __sw_fsq_scaled);
    gl.bufferData(gl.ARRAY_BUFFER,
      __sw_fsq_verts(__SW.boundCanvas === 0, sx, sy), gl.DYNAMIC_DRAW);
  }
  gl.vertexAttribPointer(0, 2, gl.FLOAT, false, 16, 0);
  gl.enableVertexAttribArray(0);
  gl.vertexAttribPointer(1, 2, gl.FLOAT, false, 16, 8);
  gl.enableVertexAttribArray(1);
  gl.disableVertexAttribArray(2);
  gl.disableVertexAttribArray(3);
  gl.disableVertexAttribArray(4);
  gl.drawArrays(gl.TRIANGLES, 0, 6);
  __sw_mark_drawn(sh);
}

// --- text: canvas2d glyph atlas (MC, docs/stone-web-plan.md § text) ---
// The boot preload registered each shipped TTF as a FontFace; the bake here
// is synchronous canvas2d against that family. All of ASCII 32..126 bakes as
// one 16-glyph-wide atlas; density changes explicitly rebake into the same
// texture handle. Atlas pixels are
// coverage-RGBA (r=g=b=a=coverage, built from the canvas ALPHA channel --
// raw white fillText would give (1,1,1,cov), wrong rgb under tinting).
// Metric bit-parity with fontdue is a NON-goal; invariants only.
//
// Faces: `faceMask` declares which synthesized faces to bake (bit 0 bold,
// bit 1 italic), exactly as stone-rs `FaceSet` does, and each declared face
// gets its own row block in the same texture.
//
// The bake never asks CSS for a bold or italic face. Browser synthesis picks
// its own angle -- the CSS default oblique is 14 degrees and Firefox uses
// 11.3-16.7 depending on the host -- so asking for one would leave the two
// legs agreeing on invariants while differing on the italic axis itself.
// Instead every face is drawn from the SAME font spec with native's own 12
// degrees and px/24 stroke applied here. `fontSynthesis = "none"` pins that:
// it is inert while the spec names no weight, and it is what keeps a future
// spec change from silently reintroducing the browser's angle.
var __SW_OBLIQUE_DEG = 12;             // stone-rs glyph_raster::OBLIQUE_DEGREES
var __SW_BOLD_PER_PX = 1 / 24;         // stone-rs glyph_raster::BOLD_STRENGTH_PER_PX
// Face::ALL order: regular, bold, italic, bold-italic.
function __sw_face_declared(mask, face) {
  var wantsBold = face === 1 || face === 3;
  var wantsItalic = face === 2 || face === 3;
  return (!wantsBold || (mask & 1) !== 0) && (!wantsItalic || (mask & 2) !== 0);
}
function __sw_font_path_key(path) { return String(path).replace(/^\.?\//, ""); }
function __sw_bake_font(path, pixelSize, existingAtlas, faceMask) {
  faceMask = faceMask | 0;
  if (globalThis.__STONE_WEB_FAKE_ENV || typeof document === "undefined") {
    __sw_log_once("stone_load_font: no canvas2d in this environment -- glyphless fallback font");
    return { em: pixelSize, atlas: existingAtlas || 0, glyphs: [{}, {}, {}, {}], maxH: pixelSize, fake: true, sourcePath: path, faceMask: faceMask };
  }
  var fams = __SW.fontFamilies || {};
  var fam = fams[__sw_font_path_key(path)];
  if (!fam) {
    for (var k in fams) { fam = fams[k]; break; } // shipped fallback font, like native's embedded DroidSans
    if (fam) __sw_log_once("stone_load_font: no FontFace for '" + path + "'; using the shipped fallback font");
  }
  if (!fam) {
    __sw_log_once("stone_load_font: no fonts were preloaded; text will not render");
    return null;
  }

  var mcanvas = document.createElement("canvas");
  var mctx = mcanvas.getContext("2d", { willReadFrequently: true });
  var fontSpec = pixelSize + "px " + fam;
  mctx.font = fontSpec;
  mctx.fontSynthesis = "none"; // never the browser's own faux face; see above
  mctx.textBaseline = "alphabetic";

  var baked = [];
  for (var f = 0; f < 4; f++) if (__sw_face_declared(faceMask, f)) baked.push(f);
  var shear = Math.tan((__SW_OBLIQUE_DEG * Math.PI) / 180);
  var embolden = pixelSize * __SW_BOLD_PER_PX;

  // Pass 1: metrics for all 95 glyphs of every declared face -> cell size.
  // The cell holds the WIDEST face: an oblique pushes ink past its own
  // advance, so sizing from regular alone would bleed bold-italic into the
  // next cell. Canvas gives no sheared metrics, so the extra extent is
  // derived from the shear and the glyph height, which is what the transform
  // will actually add.
  var infos = [];
  var cellW = 1, cellH = 1;
  for (var bi = 0; bi < baked.length; bi++) {
    var face = baked[bi];
    var isBold = face === 1 || face === 3;
    var isItalic = face === 2 || face === 3;
    var faceInfos = [];
    for (var cp = 32; cp <= 126; cp++) {
      var ch = String.fromCharCode(cp);
      var m = mctx.measureText(ch);
      var left = m.actualBoundingBoxLeft || 0;
      var right = m.actualBoundingBoxRight || 0;
      var ascent = m.actualBoundingBoxAscent || 0;
      var descent = m.actualBoundingBoxDescent || 0;
      var grow = isBold ? embolden : 0;
      var lean = isItalic ? Math.abs(ascent) * shear : 0;
      var gw = Math.ceil(left + right + grow + lean);
      var gh = Math.ceil(ascent + descent + grow);
      faceInfos.push({
        cp: cp, ch: ch, face: face, bold: isBold, italic: isItalic,
        left: left + grow / 2, ascent: ascent + grow / 2,
        w: gw, h: gh, advance: m.width + grow,
      });
      if (gw > cellW) cellW = gw;
      if (gh > cellH) cellH = gh;
    }
    infos.push(faceInfos);
  }
  var PAD = 2;
  cellW += PAD * 2;
  cellH += PAD * 2;
  var COLS = 16; // mirror native text.rs grid
  var rowsPerFace = Math.ceil(95 / COLS);
  var aw = COLS * cellW, ah = rowsPerFace * baked.length * cellH;

  var canvas = document.createElement("canvas");
  canvas.width = aw; canvas.height = ah;
  var ctx = canvas.getContext("2d", { willReadFrequently: true });
  ctx.font = fontSpec;
  ctx.fontSynthesis = "none";
  ctx.textBaseline = "alphabetic";
  ctx.fillStyle = "#ffffff";

  var glyphs = [{}, {}, {}, {}];
  var maxH = 1;
  for (var bi2 = 0; bi2 < baked.length; bi2++) {
    var faceRow = bi2 * rowsPerFace;
    var fi = infos[bi2];
    for (var i = 0; i < fi.length; i++) {
      var g = fi[i];
      var cx = (i % COLS) * cellW + PAD;
      var cy = (faceRow + Math.floor(i / COLS)) * cellH + PAD;
      // Pen at (cx + left, cy + ascent): ink box lands at [cx, cy, w, h].
      // The oblique is a shear about the BASELINE, so the transform is applied
      // around the pen rather than around the cell origin -- shearing about
      // the cell would move the whole glyph instead of leaning it.
      ctx.save();
      if (g.italic) {
        ctx.translate(cx + g.left, cy + g.ascent);
        ctx.transform(1, 0, -shear, 1, 0, 0);
        ctx.translate(-(cx + g.left), -(cy + g.ascent));
      }
      if (g.bold) {
        // Canvas has no outline-level embolden; a centred stroke over the fill
        // is the same construction native uses (fill unioned with a stroke of
        // the target width), not a second offset fill.
        ctx.lineWidth = embolden;
        ctx.lineJoin = "round";
        ctx.strokeStyle = "#ffffff";
        ctx.strokeText(g.ch, cx + g.left, cy + g.ascent);
      }
      ctx.fillText(g.ch, cx + g.left, cy + g.ascent);
      ctx.restore();
      // 9-float glyph info: [u0 v0 u1 v1 width height bearing_x bearing_y advance];
      // bearing_y is TOP-based (ink top above baseline = actualBoundingBoxAscent).
      glyphs[g.face][g.cp] = new Float32Array([
        cx / aw, cy / ah, (cx + g.w) / aw, (cy + g.h) / ah,
        g.w, g.h, -g.left, g.ascent, g.advance,
      ]);
      if (g.h > maxH) maxH = g.h;
    }
  }

  // Coverage-RGBA from the alpha channel.
  var src = ctx.getImageData(0, 0, aw, ah).data;
  var rgba = new Uint8Array(aw * ah * 4);
  for (var p = 0; p < aw * ah; p++) {
    var cov = src[p * 4 + 3];
    rgba[p * 4] = cov; rgba[p * 4 + 1] = cov; rgba[p * 4 + 2] = cov; rgba[p * 4 + 3] = cov;
  }
  var gl = __SW.gl;
  var texHandle = existingAtlas || __sw_new_texture();
  gl.bindTexture(gl.TEXTURE_2D, __SW.textures[texHandle]);
  gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, aw, ah, 0, gl.RGBA, gl.UNSIGNED_BYTE, rgba);
  var filter = gl.NEAREST; // sampling is per-draw sampler state (Font.sampling)
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, filter);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, filter);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
  __SW.imageSizes[texHandle] = { w: aw, h: ah };

  return { em: pixelSize, atlas: texHandle, glyphs: glyphs, maxH: maxH, sourcePath: path, faceMask: faceMask };
}

function stone_load_font(path, pixelSize, faceMask) {
  var baked = __sw_bake_font(path, pixelSize, 0, faceMask);
  if (!baked) return 0;

  __SW.fonts = __SW.fonts || {};
  var fid = (__SW.nextFont = (__SW.nextFont || 0) + 1);
  __SW.fonts[fid] = baked;
  console.log("[STONE-WEB] font atlas baked @ " + pixelSize + "px");
  return fid;
}
function __sw_font(fontId) { return (__SW.fonts || {})[fontId]; }
function stone_reload_font(fontId, path, pixelSize) {
  var old = __sw_font(fontId);
  if (!old || pixelSize < 1) return false;
  // A rebake keeps the DECLARED faces: reload and resize change the source
  // bytes or the size, never what the family declared at load.
  var baked = __sw_bake_font(path, pixelSize, old.atlas, old.faceMask);
  if (!baked) return false;
  __SW.fonts[fontId] = baked;
  console.log("[STONE-WEB] font atlas rebaked @ " + pixelSize + "px");
  return true;
}
function stone_resize_font(fontId, pixelSize) {
  var old = __sw_font(fontId);
  if (!old || pixelSize < 1) return false;
  var baked = __sw_bake_font(old.sourcePath, pixelSize, old.atlas, old.faceMask);
  if (!baked) return false;
  __SW.fonts[fontId] = baked;
  console.log("[STONE-WEB] font atlas rebaked @ " + pixelSize + "px");
  return true;
}
// 0 when the font has no atlas, matching stone-rs. Returning the WHITE PIXEL's
// handle instead made the atlas share an identity with it, so a font declaring
// Linear re-declared the shared white pixel for every other draw.
function stone_get_font_atlas_image_for(fontId) {
  var f = __sw_font(fontId);
  return f && f.atlas ? f.atlas : 0;
}
function stone_get_font_pixel_size_for(fontId) {
  var f = __sw_font(fontId);
  return f ? f.em : 48;
}
function stone_get_glyph_info_for(fontId, face, ch, out) {
  var f = __sw_font(fontId);
  if (!f || face < 0 || face > 3) return false;
  var g = f.glyphs[face][ch];
  if (!g) return false;
  for (var i = 0; i < 9; i++) out[i] = g[i];
  return true;
}
// Shim-side measure, from the shim's OWN advances (native-formula drift is
// the bug class this avoids): width = max line sum(advance)*scale; height
// single-line = max glyph height*scale, multi-line adds (lines-1)*em*1.2.
// The glyph max is over THIS TEXT's glyphs, as stone-rs `stone_measure_text`
// takes it -- the font-wide maximum is a different number and made web plain
// text sit lower than native for the same string.
function stone_measure_text_js(fontId, text, scale) {
  var f = __sw_font(fontId);
  if (!f) return [0, 0];
  var s = String(text);
  var lines = 1, w = 0, lineW = 0, maxH = 0;
  for (var i = 0; i < s.length; i++) {
    if (s[i] === "\n") { lines++; if (lineW > w) w = lineW; lineW = 0; continue; }
    var g = f.glyphs[0][s.charCodeAt(i)];
    if (g) { lineW += g[8]; if (g[5] > maxH) maxH = g[5]; }
  }
  if (lineW > w) w = lineW;
  var h = maxH * Math.abs(scale);
  if (lines > 1) h = (lines - 1) * f.em * Math.abs(scale) * 1.2 + maxH * Math.abs(scale);
  return [w * scale, h];
}
// Baseline = max TOP-based bearing_y over the text's glyphs, * |scale|.
function stone_get_text_baseline_for(fontId, text, scale) {
  var f = __sw_font(fontId);
  if (!f) return 0;
  var s = String(text);
  var b = 0;
  for (var i = 0; i < s.length; i++) {
    var g = f.glyphs[0][s.charCodeAt(i)];
    if (g && g[7] > b) b = g[7];
  }
  return b * Math.abs(scale);
}

// --- filesystem (registry-backed reads, in-memory writes, dir ops no-op) ---
var __ROCK_WEB_FILES = (globalThis.__ROCK_WEB_FILES = globalThis.__ROCK_WEB_FILES || {});
// Seed persisted writes (SRS saves, settings) from localStorage under the
// "rockfile:" namespace. localStorage may be absent (node fake-GL) or throw
// (private mode) -- persistence is optional, so skip seeding in that case.
(function () {
  if (typeof localStorage === "undefined") return;
  try {
    for (var i = 0; i < localStorage.length; i++) {
      var k = localStorage.key(i);
      if (k && k.indexOf("rockfile:") === 0) __ROCK_WEB_FILES[k.slice(9)] = localStorage.getItem(k);
    }
  } catch (e) { console.error("[STONE-WEB] localStorage seed skipped:", e && e.message); }
})();
function __sw_file_write(path, content) {
  __ROCK_WEB_FILES[path] = content;
  if (typeof localStorage !== "undefined") {
    try { localStorage.setItem("rockfile:" + path, content); }
    catch (e) { __sw_log_once("stone_file_write: localStorage mirror failed for " + path + " (" + (e && e.message) + ")"); }
  }
  return true;
}
function __sw_file_get(path) {
  if (path in __ROCK_WEB_FILES) return __ROCK_WEB_FILES[path];
  if (path in __ROCK_WEB_ASSETS) return __ROCK_WEB_ASSETS[path];
  return undefined;
}
function stone_file_exists(path) { return __sw_file_get(path) !== undefined; }
function stone_file_size(path) {
  var c = __sw_file_get(path);
  return c === undefined ? -1 : (c.byteLength !== undefined ? c.byteLength : c.length);
}
function stone_copy_file(src, dst) {
  var c = __sw_file_get(src);
  if (c === undefined) return false;
  __ROCK_WEB_FILES[dst] = c;
  return true;
}
function stone_rename(src, dst) {
  var c = __sw_file_get(src);
  if (c === undefined) return false;
  __ROCK_WEB_FILES[dst] = c;
  delete __ROCK_WEB_FILES[src];
  return true;
}
function stone_remove_file(path) { delete __ROCK_WEB_FILES[path]; return true; }

// --- subprocess (chess UCI): no child processes in a browser; the spawn is a
// loud stub returning 0 so sf_start drops to the built-in engine. ---
function stone_proc_spawn(path) {
  __sw_log_once("stone_proc_spawn: subprocess unavailable on web (" + path + "); falling back to the built-in engine");
  return 0;
}
function __sw_bytes_to_string(b) {
  var u8 = b instanceof Uint8Array ? b : new Uint8Array(b);
  var s = "";
  for (var i = 0; i < u8.length; i++) s += String.fromCharCode(u8[i]);
  return s;
}

// --- camera state / immediate sprite path (MB: batches only; the immediate
// sprite quad lands with the MD canvas-present blit) ---
function stone_current_shader_bound() { return __SW.curShader !== 0; }
// The canvas-present blit (sparkle trail et al): a rotated textured quad in
// pixel space through the bound program, else the passed basic-sprite handle
// with default uniforms -- mirrors sprite.rs stone_sprite_draw_texture
// (8-float pos2/uv2/color4 layout, prefer-current-shader rule, default
// pixel-ortho only when nothing was bound).
function stone_sprite_draw_texture(shader, tex, tw, th, x, y, w, h, sx, sy, sw, sh, rot, scx, scy, r, g, b, a) {
  var gl = __SW.gl;
  if (!gl) return;
  var t = __SW.textures[tex];
  if (!t) { __sw_log_once("stone_sprite_draw_texture: unknown texture " + tex); return; }
  var cur = __SW.curShader;
  var sh2 = cur !== 0 ? __SW.shaders[cur] : __SW.shaders[shader];
  if (!sh2) { __sw_log_once("stone_sprite_draw_texture: no shader bound and handle " + shader + " not in registry; skipping"); return; }
  gl.useProgram(sh2.prog);
  var u0 = sx / tw, v0 = sy / th, u1 = (sx + sw) / tw, v1 = (sy + sh) / th;
  var qw = w * scx, qh = h * scy;
  var cr = Math.cos(rot), sr = Math.sin(rot);
  var xs = [0, qw, qw, 0], ys = [0, 0, qh, qh];
  var px = [], py = [];
  for (var i = 0; i < 4; i++) {
    px.push(x + xs[i] * cr - ys[i] * sr);
    py.push(y + xs[i] * sr + ys[i] * cr);
  }
  var verts = new Float32Array([
    px[0], py[0], u0, v0, r, g, b, a,
    px[1], py[1], u1, v0, r, g, b, a,
    px[2], py[2], u1, v1, r, g, b, a,
    px[0], py[0], u0, v0, r, g, b, a,
    px[2], py[2], u1, v1, r, g, b, a,
    px[3], py[3], u0, v1, r, g, b, a,
  ]);
  gl.activeTexture(gl.TEXTURE0);
  gl.bindTexture(gl.TEXTURE_2D, t);
  gl.bindBuffer(gl.ARRAY_BUFFER, __SW.meshVbo);
  gl.bufferData(gl.ARRAY_BUFFER, verts, gl.STREAM_DRAW);
  gl.vertexAttribPointer(0, 2, gl.FLOAT, false, 32, 0);
  gl.enableVertexAttribArray(0);
  gl.vertexAttribPointer(1, 2, gl.FLOAT, false, 32, 8);
  gl.enableVertexAttribArray(1);
  gl.vertexAttribPointer(2, 4, gl.FLOAT, false, 32, 16);
  gl.enableVertexAttribArray(2);
  gl.disableVertexAttribArray(3);
  gl.disableVertexAttribArray(4);
  gl.drawArrays(gl.TRIANGLES, 0, 6);
  __sw_mark_drawn(sh2);
}

// --- audio (WebAudio; silent no-op in a no-AudioContext env) ---
function stone_load_sound(path) {
  var ctx = __sw_audio_ctx();
  if (!ctx) return 0;
  return __sw_audio_decode(__SW.audio.nextSound++, path);
}
function stone_reload_sound(h, path) {
  if (!__sw_audio_ctx()) return false;
  return __sw_audio_decode(h, path) !== 0;
}
function stone_unload_sound(h) { delete __SW.audio.sounds[h]; return true; }
// `bus` is STONE_BUS_* (lib.rock): 0 master, 1 sfx, 2 music, 3 ambience, 4 ui.
// The web mixer has only sfx and music nodes, so music routes to musicGain and
// every other bus shares sfxGain -- the topology is coarser than native, not
// ignored.
function stone_play_sound(h, bus, volume, speed, decay) {
  var A = __SW.audio, s = A.sounds[h];
  if (!A.ctx || !s || !s.buffer) return;
  var src = A.ctx.createBufferSource();
  src.buffer = s.buffer;
  src.playbackRate.value = Math.min(Math.max(speed, 0.05), 8);
  var g = A.ctx.createGain();
  var vol = Math.max(volume, 0);
  g.gain.value = vol;
  src.connect(g); g.connect(bus === 2 ? A.musicGain : A.sfxGain); src.start();
  if (decay > 0) {
    var t = A.ctx.currentTime;
    g.gain.setValueAtTime(vol, t);
    g.gain.linearRampToValueAtTime(0, t + decay);
    src.stop(t + decay);
  }
  A.sfxSources[h] = src;
}
function stone_get_sound_duration(h) {
  var s = __SW.audio.sounds[h];
  return (s && s.buffer) ? s.buffer.duration : 0;
}
function stone_stop_sound(h) {
  var src = __SW.audio.sfxSources[h];
  // stop() on an already-ended source throws InvalidStateError -- benign here.
  if (src) { try { src.stop(); } catch (e) {} __SW.audio.sfxSources[h] = null; }
}
function stone_play_music(h) {
  var A = __SW.audio, s = A.sounds[h];
  if (!A.ctx || !s || !s.buffer) return;
  stone_stop_music();
  var src = A.ctx.createBufferSource();
  src.buffer = s.buffer; src.loop = true; src.connect(A.musicGain); src.start();
  A.musicSrc = src;
}
function stone_stop_music() {
  var A = __SW.audio;
  if (A.musicSrc) { try { A.musicSrc.stop(); } catch (e) {} A.musicSrc = null; }
}
// No bus graph on web v1 (mixer.rock's five strips have no WebAudio
// counterpart yet); every bus/voice op below is a silent stub, same tier as
// the convolver reverb has always been.
function stone_bus_set_gain(bus, gain) { return false; }
function stone_bus_samples_pulled(bus) { return 0; }
function stone_bus_duck_count(bus) { return 0; }
function stone_bus_gain(bus) { return 1; }
function stone_bus_peak(bus) { return 0; }
function stone_bus_set_lowpass(bus, cutoff_hz, q) { return false; }
function stone_bus_set_distortion(bus, drive, mix) { return false; }
function stone_bus_set_reverb(bus, enabled, room_size, damping, wet, dry, width) { return false; }
function stone_bus_duck(bus, depth, attack_secs, release_secs) { return false; }
function stone_voice_active(voice) { return false; }
function stone_voice_stop(voice) { return false; }
function stone_voice_set_gain(voice, gain) { return false; }
function stone_voice_set_speed(voice, speed) { return false; }

// --- uniform introspection / gpu timer / delay / cursors / retained mesh ---
function stone_shader_uniform_count(h) {
  var gl = __SW.gl, sh = __SW.shaders[h];
  if (!gl || !sh || !gl.getProgramParameter) return 0;
  var n = gl.getProgramParameter(sh.prog, gl.ACTIVE_UNIFORMS);
  return typeof n === "number" ? n : 0;
}
function stone_shader_uniform_type(h, i) {
  var gl = __SW.gl, sh = __SW.shaders[h];
  if (!gl || !sh || !gl.getActiveUniform) return 0;
  var u = gl.getActiveUniform(sh.prog, i);
  return u ? u.type : 0;
}
function __sw_shader_uniform_name(h, i) {
  var gl = __SW.gl, sh = __SW.shaders[h];
  if (!gl || !sh || !gl.getActiveUniform) return "";
  var u = gl.getActiveUniform(sh.prog, i);
  // strip the GLSL array suffix ([0]) like the native side does
  return u ? u.name.replace(/\[0\]$/, "") : "";
}
function stone_mesh_create(mode, verts, vc, stride, sizes, ac) {
  __sw_log_once("stone_mesh_create: retained instanced mesh not implemented on web yet");
  return 0;
}

// Immediate polygon (shapes.rs port): fan-triangulate into the 10-float mesh
// format and submit through stone_draw_mesh under the caller's bound program
// (the shadowmap visibility fans + glow quads ride this). "fill" fans from
// vertex 0; outline mode draws 1px-wide rotated line quads per edge.
function stone_set_color_mask(r, g, b, a) { var gl = __SW.gl; if (gl) gl.colorMask(r, g, b, a); }
// Draw-buffer list of the CURRENTLY BOUND FBO (per-FBO state in WebGL2, same
// as GL): [A0, A1] during the MRT color pass, [A0] otherwise. Tracked on the
// bound canvas so stone_clear_canvas knows when to narrow for the float clear.
function stone_set_draw_buffers(count) {
  var gl = __SW.gl;
  var c = __SW.canvases[__SW.boundCanvas];
  var n = count >= 2 ? 2 : 1;
  gl.drawBuffers(n === 2 ? [gl.COLOR_ATTACHMENT0, gl.COLOR_ATTACHMENT1] : [gl.COLOR_ATTACHMENT0]);
  if (c) c.drawBufs = n;
}

// The (format, type, view, convert) readPixels shape for one pixel of canvas
// `c`: R32I ids read as RGBA_INTEGER/INT (exact in f32 -- ids are far below
// 2^24), float canvases as RGBA/FLOAT, rgba8 as bytes (red normalized).
function __sw_read_shape(gl, c) {
  if (c.format === 3) return { fmt: gl.RGBA_INTEGER, type: gl.INT, view: new Int32Array(4), get: function (v) { return v[0]; } };
  if (c.format === 1 || c.format === 2) return { fmt: gl.RGBA, type: gl.FLOAT, view: new Float32Array(4), get: function (v) { return v[0]; } };
  return { fmt: gl.RGBA, type: gl.UNSIGNED_BYTE, view: new Uint8Array(4), get: function (v) { return v[0] / 255.0; } };
}
// Sync single-pixel read (sim-only cost): finish + readPixels, top-left coords.
function stone_read_canvas_pixel(h, x, y) {
  var gl = __SW.gl;
  var c = __SW.canvases[h];
  if (!c) return 0;
  // Bounds-check and flip against the ACTIVE image, not the backing texture
  // (stone-rs canvas.rs does the same): a scene target is allocated at
  // capacity and rendered into its active rect, which is anchored at the GL
  // origin, so the top-left row converts against the active height.
  var aw = c.activeW || c.w, ah = c.activeH || c.h;
  if (x < 0 || y < 0 || x >= aw || y >= ah) return 0;
  var shape = __sw_read_shape(gl, c);
  gl.bindFramebuffer(gl.FRAMEBUFFER, c.fbo);
  gl.finish();
  gl.readPixels(x, ah - 1 - y, 1, 1, shape.fmt, shape.type, shape.view);
  var prev = __SW.canvases[__SW.boundCanvas];
  gl.bindFramebuffer(gl.FRAMEBUFFER, prev ? prev.fbo : null);
  return shape.get(shape.view);
}
// Read a w*h RGBA8 rect of the WINDOW into `out` in top-left coordinates --
// the twin of canvas.rs's stone_read_window_rect, and what WindowProbe runs
// on. Default framebuffer, so it is the DRAWABLE that bounds and flips it, not
// the logical size: the probe addresses window pixels exactly as native does.
function stone_read_window_rect(x, y, w, h, out) {
  var gl = __SW.gl;
  if (!gl || !out || w <= 0 || h <= 0) return;
  // The engine's window space is LOGICAL here -- the default framebuffer is
  // driven in CSS pixels (the hidpi ortho gate) while readPixels addresses the
  // backing store, so headless Chrome at deviceScaleFactor 1.5 reads 1920x1080
  // for a window the engine calls 1280x720. Native has no such split; at dpr 1
  // this reduces to a plain read.
  var bw = gl.drawingBufferWidth, bh = gl.drawingBufferHeight;
  var sx = bw / (__SW.logicalW || bw), sy = bh / (__SW.logicalH || bh);
  var rx = Math.round(x * sx), ry = Math.round(y * sy);
  var rw = Math.max(1, Math.round(w * sx)), rh = Math.max(1, Math.round(h * sy));
  if (rx < 0 || ry < 0 || rx + rw > bw || ry + rh > bh) return;
  gl.bindFramebuffer(gl.FRAMEBUFFER, null);
  gl.pixelStorei(gl.PACK_ALIGNMENT, 1);
  var buf = new Uint8Array(rw * rh * 4);
  gl.readPixels(rx, bh - ry - rh, rw, rh, gl.RGBA, gl.UNSIGNED_BYTE, buf);
  // Bottom-up device pixels -> top-down window pixels, POINT-sampled. A probe
  // reports what a window pixel shows; averaging would smear exactly the hard
  // steps probes exist to measure.
  for (var r = 0; r < h; r++) {
    var srow = rh - 1 - Math.min(rh - 1, Math.round(r * sy));
    for (var c = 0; c < w; c++) {
      var si = (srow * rw + Math.min(rw - 1, Math.round(c * sx))) * 4;
      var di = (r * w + c) * 4;
      out[di] = buf[si]; out[di + 1] = buf[si + 1];
      out[di + 2] = buf[si + 2]; out[di + 3] = buf[si + 3];
    }
  }
  var prev = __SW.canvases[__SW.boundCanvas];
  gl.bindFramebuffer(gl.FRAMEBUFFER, prev ? prev.fbo : null);
}
// WebGL2 has no DEPTH_COMPONENT readPixels at all (the GLES rule) -- far plane.
function stone_read_depth_pixel(h, x, y) {
  __sw_log_once("depth pixel readback unsupported on WebGL2; returning far plane");
  return 1.0;
}

// --- async single-pixel readback: double-buffered PIXEL_PACK_BUFFER ping-pong
// (mirrors stone-rs pixel_readback.rs). submit() promotes prior staging to
// ready + queues readPixels into staging with a fence; consume() reads the
// READY pbo (>= 1 frame old BY DESIGN -- mag's async pick). One-shot per
// promoted submit; OOB / unknown canvas / depth-on-web submits carry the
// sentinel (0 color, 1 depth far plane). ---
function stone_pixel_readback_new(isDepth) {
  var gl = __SW.gl;
  if (!gl || !gl.createBuffer) return 0;
  var R = (__SW.readbacks = __SW.readbacks || { slots: {}, next: 1 });
  var pbos = [gl.createBuffer(), gl.createBuffer()];
  for (var i = 0; i < 2; i++) {
    gl.bindBuffer(gl.PIXEL_PACK_BUFFER, pbos[i]);
    gl.bufferData(gl.PIXEL_PACK_BUFFER, 16, gl.STREAM_READ);
  }
  gl.bindBuffer(gl.PIXEL_PACK_BUFFER, null);
  var id = R.next++;
  R.slots[id] = {
    pbos: pbos, readyIdx: 0, stagingIdx: 1,
    readyPrimed: false, stagingPrimed: false,
    readyGarbage: false, stagingGarbage: false,
    readyFmt: 0, stagingFmt: 0, // 0 f32, 1 bytes, 2 ints
    fences: [null, null],
    depth: !!isDepth,
  };
  return id;
}
function stone_pixel_readback_destroy(id) {
  var R = __SW.readbacks;
  var s = R && R.slots[id];
  if (!s) return;
  var gl = __SW.gl;
  gl.deleteBuffer(s.pbos[0]); gl.deleteBuffer(s.pbos[1]);
  delete R.slots[id];
}
function stone_pixel_readback_submit(id, canvasH, x, y) {
  var R = __SW.readbacks;
  var s = R && R.slots[id];
  if (!s) return;
  var gl = __SW.gl;
  if (s.stagingPrimed) {
    var t = s.readyIdx; s.readyIdx = s.stagingIdx; s.stagingIdx = t;
    s.readyPrimed = true;
    s.readyGarbage = s.stagingGarbage;
    s.readyFmt = s.stagingFmt;
    s.stagingPrimed = false;
  }
  var c = __SW.canvases[canvasH];
  var aw = c ? (c.activeW || c.w) : 0, ah = c ? (c.activeH || c.h) : 0;
  var ok = !!c && x >= 0 && y >= 0 && x < aw && y < ah;
  if (ok && s.depth) {
    __sw_log_once("depth pixel readback unsupported on WebGL2; returning far plane");
    ok = false;
  }
  if (!ok) { s.stagingPrimed = true; s.stagingGarbage = true; return; }
  var shape = __sw_read_shape(gl, c);
  s.stagingFmt = (c.format === 3) ? 2 : (c.format === 1 || c.format === 2) ? 0 : 1;
  gl.bindFramebuffer(gl.READ_FRAMEBUFFER, c.fbo);
  gl.bindBuffer(gl.PIXEL_PACK_BUFFER, s.pbos[s.stagingIdx]);
  gl.pixelStorei(gl.PACK_ALIGNMENT, 1);
  gl.readPixels(x, ah - 1 - y, 1, 1, shape.fmt, shape.type, 0);
  gl.bindBuffer(gl.PIXEL_PACK_BUFFER, null);
  if (s.fences[s.stagingIdx]) gl.deleteSync(s.fences[s.stagingIdx]);
  s.fences[s.stagingIdx] = gl.fenceSync ? gl.fenceSync(gl.SYNC_GPU_COMMANDS_COMPLETE, 0) : null;
  var prev = __SW.canvases[__SW.boundCanvas];
  gl.bindFramebuffer(gl.FRAMEBUFFER, prev ? prev.fbo : null);
  s.stagingPrimed = true;
  s.stagingGarbage = false;
}
function stone_pixel_readback_consume(id) {
  var R = __SW.readbacks;
  var s = R && R.slots[id];
  if (!s) return 0;
  var sentinel = s.depth ? 1.0 : 0.0;
  if (!s.readyPrimed) return sentinel;
  s.readyPrimed = false;
  if (s.readyGarbage) { s.readyGarbage = false; return sentinel; }
  var gl = __SW.gl;
  var fence = s.fences[s.readyIdx];
  if (fence) { gl.clientWaitSync(fence, 0, 0); gl.deleteSync(fence); s.fences[s.readyIdx] = null; }
  var view = s.readyFmt === 2 ? new Int32Array(4) : s.readyFmt === 1 ? new Uint8Array(4) : new Float32Array(4);
  gl.bindBuffer(gl.PIXEL_PACK_BUFFER, s.pbos[s.readyIdx]);
  gl.getBufferSubData(gl.PIXEL_PACK_BUFFER, 0, view);
  gl.bindBuffer(gl.PIXEL_PACK_BUFFER, null);
  return s.readyFmt === 1 ? view[0] / 255.0 : view[0];
}

// --- window management (fixed canvas on web) ---
// Element.requestFullscreen() needs a transient user activation, which a
// rAF-driven frame never has -- so this can only ever report, not act.
function stone_set_fullscreen(mode) {
  if (mode) { __sw_log_once("stone_set_fullscreen(" + mode + "): not supported on web; needs a user gesture to call requestFullscreen"); }
}

// --- boot: preload assets, register fonts, then main (spec boot step 2) ---
// __ROCK_WEB_PRELOAD is emitted by the `rock web` assembler. Each entry is
// fetched into __ROCK_WEB_ASSETS (keeping the sync file_read-shape reads
// sync); .ttf/.otf entries also register as a FontFace so stone_load_font's
// canvas2d bake is synchronous. A preload failure is LOUD but still boots --
// the font path then falls back exactly like native's embedded-fallback rung.
function __rock_web_boot(mainFn) {
  var pre = typeof __ROCK_WEB_PRELOAD !== "undefined" ? __ROCK_WEB_PRELOAD : [];
  var fakeEnv = globalThis.__STONE_WEB_FAKE_ENV;
  // The node fake-GL smoke has no fetch, so nothing decodes and every load
  // would be a genuine miss -- the smoke would measure the missing-asset path
  // instead of the game. It opts into a 1x1 stub for the MANIFEST's images
  // only: the fake GL ignores pixel content, and a path absent from the
  // manifest still misses, so this cannot hide a bad asset path.
  if (fakeEnv && fakeEnv.stubPreloadedImages) {
    for (var pi = 0; pi < pre.length; pi++) {
      if (/\.(png|jpe?g|gif|bmp|webp)$/i.test(pre[pi])) {
        __SW.imageBitmaps[pre[pi]] = { width: 1, height: 1 };
      }
    }
  }
  if (fakeEnv || typeof fetch === "undefined" || pre.length === 0) {
    if (mainFn) mainFn();
    return;
  }
  Promise.all(pre.map(function (path) {
    // A single-file build (`rock web --single-file`) has already populated
    // __ROCK_WEB_ASSETS from base64 embedded in the page, so there is nothing
    // to fetch -- and under file:// a fetch would fail outright. Take the bytes
    // and fall through to the SAME decode chain: the font and image rungs below
    // are what make an asset usable, so skipping them would leave a preloaded
    // font unregistered and a preloaded image undecoded.
    var __inlined = __ROCK_WEB_ASSETS[path];
    var __bytes = __inlined
      ? Promise.resolve(__inlined.buffer.slice(
          __inlined.byteOffset, __inlined.byteOffset + __inlined.byteLength))
      : fetch(path).then(function (r) {
          if (!r.ok) throw new Error(path + " -> HTTP " + r.status);
          return r.arrayBuffer();
        });
    return __bytes.then(function (buf) {
      __ROCK_WEB_ASSETS[path] = new Uint8Array(buf);
      if (/\.(ttf|otf)$/i.test(path) && typeof FontFace !== "undefined") {
        var fam = "__sw_font_" + path.replace(/[^A-Za-z0-9]/g, "_");
        var face = new FontFace(fam, buf);
        return face.load().then(function (loaded) {
          document.fonts.add(loaded);
          (__SW.fontFamilies = __SW.fontFamilies || {})[path] = fam;
        });
      }
      // Decode images to ImageBitmaps now (no GL context yet); stone_load_image
      // uploads the GL texture lazily at @init. A per-image decode failure is
      // loud but non-fatal -- that path returns handle 0 at load.
      if (/\.(png|jpe?g|gif|bmp|webp)$/i.test(path) && typeof createImageBitmap !== "undefined") {
        return createImageBitmap(new Blob([buf])).then(function (bmp) {
          __SW.imageBitmaps[path] = bmp;
        }).catch(function (e) {
          console.warn("[STONE-WEB] image decode failed:", path, e && e.message);
        });
      }
    });
  })).then(function () {
    if (mainFn) mainFn();
  }).catch(function (e) {
    console.error("[STONE-WEB] asset preload failed:", e);
    if (mainFn) mainFn();
  });
}

// --- rAF driver ---
function __rock_web_arm_raf(frameFn) {
  var fake = globalThis.__STONE_WEB_FAKE_ENV;
  if (fake && fake.driveFrames) return; // harness drives frames itself
  function tick() {
    __SW.frames = (__SW.frames | 0) + 1; // driver-observable frame count (CDP smoke)
    __sw_sync_canvas_size();
    var running = true;
    try { running = frameFn(); }
    catch (e) { console.error("[STONE-WEB] frame error:", e); running = false; }
    if (running) __SW.rafId = requestAnimationFrame(tick);
    else console.log("[STONE-WEB] game loop ended");
  }
  __SW.rafId = requestAnimationFrame(tick);
}
"##;

/// The coroutine runtime: JS ports of the stdlib `__rock_coroutine_kill` /
/// `__rock_coroutine_run_defers` inline(lua) bodies (`stdlib/unified.rock`),
/// plus the `error` global backing the transform's `Inst::Global("error")` +
/// `CallValue` raise shape (layout-stamp check, `await` re-raise). Defer
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
// Byte-faithful UTF-8 encoder (stdlib from_codepoint): each output char code is
// one UTF-8 byte, so the result is a luajit-shaped byte-string that `chars()`
// decodes back to the same code point on both backends.
function __from_codepoint(cp) {
  if (cp < 0 || cp > 0x10FFFF) throw new Error("from_codepoint: codepoint out of range: " + cp);
  if (cp >= 0xD800 && cp <= 0xDFFF) throw new Error("from_codepoint: surrogate is not a code point: " + cp);
  if (cp < 0x80) return String.fromCharCode(cp);
  if (cp < 0x800) return String.fromCharCode(0xC0 + Math.floor(cp / 0x40), 0x80 + (cp % 0x40));
  if (cp < 0x10000) return String.fromCharCode(0xE0 + Math.floor(cp / 0x1000), 0x80 + (Math.floor(cp / 0x40) % 0x40), 0x80 + (cp % 0x40));
  return String.fromCharCode(0xF0 + Math.floor(cp / 0x40000), 0x80 + (Math.floor(cp / 0x1000) % 0x40), 0x80 + (Math.floor(cp / 0x40) % 0x40), 0x80 + (cp % 0x40));
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
