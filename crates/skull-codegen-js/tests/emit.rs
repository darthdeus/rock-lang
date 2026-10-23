//! MIR->JS emitter tests. Lowers each of the eight
//! differential intrinsics' scalar fallback bodies and pins the emitted JS
//! exactly -- the two control-flow shapes (clamp's returning-arms diamond,
//! sign's store-then-merge via post-dominators) and the straight-line
//! expression bodies (lerp/round/fract/smoothstep, incl. intrinsic-call
//! rendering through the JS table). The emitted JS is node-verified against the
//! luajit reference values by the execution harness (`differential.rs`); these tests pin the
//! structure. Vec overloads (multi-value return) reject -- the reject-don't-
//! fallback discipline that keeps the emitter honest as the subset grows.

use crate::common::{emit_scalar, lower_test_fn, lower_test_ref, stdlib_ws, ws_with};

// clamp: the returning-arms diamond -- every branch arm `return`s, so the
// post-dominator is the virtual exit and the `if` has no fall-through.
#[test]
fn clamp_emits_returning_arms_diamond() {
    let ws = stdlib_ws();
    assert_eq!(
        emit_scalar(&ws, "clamp", 3),
        "\
function clamp_fb(c0, c1, c2) {
  let v0 = c0;
  let v1 = c1;
  if ((v0 < v1)) {
    let v3 = c1;
    return v3;
  } else {
    let v5 = c0;
    let v6 = c2;
    if ((v5 > v6)) {
      let v8 = c2;
      return v8;
    } else {
      let v10 = c0;
      return v10;
    }
  }
}
"
    );
}

// sign: store-then-merge -- arms write a result cell then rejoin at the merge
// block (the immediate post-dominator) that returns it. Pins the CHK ipdom
// structuring and the hoisted `let` for cells live across the merge.
#[test]
fn sign_emits_store_then_merge() {
    let ws = stdlib_ws();
    assert_eq!(
        emit_scalar(&ws, "sign", 1),
        "\
function sign_fb(c0) {
  let c1, c2;
  let v0 = c0;
  if ((v0 > 0)) {
    c1 = 1;
  } else {
    let v5 = c0;
    if ((v5 < 0)) {
      c2 = -1;
    } else {
      c2 = 0;
    }
    let v12 = c2;
    c1 = v12;
  }
  let v14 = c1;
  return v14;
}
"
    );
}

// Straight-line expression bodies; `round`/`fract`/`smoothstep` render inner
// intrinsic calls (`floor`, `clamp`) through the JS table.
#[test]
fn expression_bodies_emit() {
    let ws = stdlib_ws();
    assert_eq!(
        emit_scalar(&ws, "lerp", 3),
        "\
function lerp_fb(c0, c1, c2) {
  let v0 = c0;
  let v1 = c1;
  let v2 = c0;
  let v4 = c2;
  return (v0 + ((v1 - v2) * v4));
}
"
    );
    assert_eq!(
        emit_scalar(&ws, "round", 1),
        "\
function round_fb(c0) {
  let v0 = c0;
  let v3 = Math.floor((v0 + 0.5));
  return v3;
}
"
    );
    assert_eq!(
        emit_scalar(&ws, "fract", 1),
        "\
function fract_fb(c0) {
  let v0 = c0;
  let v1 = c0;
  let v2 = Math.floor(v1);
  return (v0 - v2);
}
"
    );
    assert_eq!(
        emit_scalar(&ws, "smoothstep", 3),
        "\
function smoothstep_fb(c0, c1, c2) {
  let c3;
  let v0 = c2;
  let v1 = c0;
  let v3 = c1;
  let v4 = c0;
  let v9 = Math.min(Math.max(((v0 - v1) / (v3 - v4)), 0), 1);
  c3 = v9;
  let v11 = c3;
  let v12 = c3;
  let v16 = c3;
  return ((v11 * v12) * (3 - (2 * v16)));
}
"
    );
}

// A Vec overload (scalarized value-struct ops) is outside the subset and
// EVERY one rejects -- reject, don't emit-and-hope. `Vec2.lerp` lowers to
// `CallMulti` value-struct calls feeding a multi-leaf return; its HIR still has
// 3 typed params (a, b: Vec2, t) so it can't be told apart by param count --
// it's identified by its rejection.
#[test]
fn vec_overload_rejects() {
    let ws = stdlib_ws();
    let mut lowered = 0;
    let mut reject_msgs: Vec<String> = Vec::new();
    for (_, &mid) in ws.module_ids.iter() {
        let module = ws.module(mid);
        for (fid, f) in module.functions.iter() {
            // Vec overloads only: skip the 3-float scalar fallback (it emits).
            if f.name.as_str() == "lerp"
                && f.inline_body.is_none()
                && !f.annotations.iter().any(|a| a.name.name.as_str() == "intrinsic")
            {
                let mf = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    lower_test_ref(&ws, mid, fid)
                }));
                let Ok(mf) = mf else { continue };
                lowered += 1;
                match skull_codegen_js::emit::emit_fn(&ws, &mf, "lerp_vec") {
                    Err(e) => reject_msgs.push(e.message),
                    Ok(js) => panic!("a Vec `lerp` overload emitted instead of rejecting:\n{js}"),
                }
            }
        }
    }
    assert!(lowered > 0, "no Vec `lerp` overload lowered");
    assert!(
        reject_msgs.iter().all(|m| m.contains("JS subset:")),
        "expected JS-subset rejections; saw: {reject_msgs:?}"
    );
}

// NativeBuffer subset pins: tagged struct construction,
// the checked `buf[i]` sugar routing through the `__nb_checked_*` shim names
// (JS_NB_CHECK_SHIMS provides the bodies), and a statement-position intrinsic
// call. Semantics are covered by the nb_differential cross-backend tests;
// these pin the emitted structure.
#[test]
fn native_buffer_subset_emits() {
    let ws = ws_with(
        "fn checked(b: NativeBuffer<float>) -> float { b[0] = 1.5; return b[0] }\n\
         fn stmt_call(b: NativeBuffer<float>) { nb_set(b, 0, 2.5); nb_set(b, 1, 3.5) }\n\
         fn nb_len(b: NativeBuffer<float>) -> int { return b.len }\n",
    );
    let checked = lower_test_fn(&ws, "checked");
    assert_eq!(
        skull_codegen_js::emit::emit_fn(&ws, &checked, "checked").unwrap(),
        "\
function checked(c0) {
  let v0 = c0;
  __nb_checked_set(v0, 0, 1.5);
  let v4 = c0;
  let v6 = __nb_checked_get(v4, 0);
  return v6;
}
"
    );
    // Both calls are statement-position because the function declares no result.
    let stmt_call = lower_test_fn(&ws, "stmt_call");
    assert_eq!(
        skull_codegen_js::emit::emit_fn(&ws, &stmt_call, "stmt_call").unwrap(),
        "\
function stmt_call(c0) {
  let v0 = c0;
  (v0.ptr[0] = 2.5);
  let v4 = c0;
  (v4.ptr[1] = 3.5);
  return;
}
"
    );
    let nb_len = lower_test_fn(&ws, "nb_len");
    assert_eq!(
        skull_codegen_js::emit::emit_fn(&ws, &nb_len, "nb_len").unwrap(),
        "\
function nb_len(c0) {
  let v0 = c0;
  let v1 = v0.len;
  return v1;
}
"
    );
}

// The stdlib `native_buffer_float` ctor (a plain Rock fn) emits through the
// MakeStruct subset: an object literal whose `ptr` is the TypedArray alloc.
#[test]
fn native_buffer_ctor_emits_object_literal() {
    let ws = stdlib_ws();
    for (_, &mid) in ws.module_ids.iter() {
        for (fid, f) in ws.module(mid).functions.iter() {
            if f.name.as_str() == "native_buffer_float" {
                let mf = lower_test_ref(&ws, mid, fid);
                assert_eq!(
                    skull_codegen_js::emit::emit_fn(&ws, &mf, "native_buffer_float").unwrap(),
                    "\
function native_buffer_float(c0) {
  let v0 = c0;
  let v1 = new Float32Array(v0);
  return ({__ty: \"NativeBuffer\", ptr: v1, len: v0});
}
"
                );
                return;
            }
        }
    }
    panic!("no native_buffer_float in stdlib");
}

// Vec indexing (`xs[i]`) emits the bounds-checked shim (T4).
#[test]
fn vec_index_emits_checked_get() {
    let ws = ws_with("fn vidx(xs: [int]) -> int { return xs[0] }\n");
    let vidx = lower_test_fn(&ws, "vidx");
    assert_eq!(
        skull_codegen_js::emit::emit_fn(&ws, &vidx, "vidx").unwrap(),
        "\
function vidx(c0) {
  let v0 = c0;
  let v2 = __vec_checked_get(v0, 0);
  return v2;
}
"
    );
}

// A general (non-NativeBuffer) struct constructs as a `{__ty: "Name", ...}`
// plain object -- the `__rock_create_struct` shape (a `__ty` string tag, no
// metatable) -- and a field read is JS property access (T4).
#[test]
fn general_struct_emits_tagged_object() {
    let ws = ws_with(
        "struct P { x: int, y: int }\n\
         fn mk() -> int { let p = P { x: 1, y: 2 }\n return p.x }\n",
    );
    let mk = lower_test_fn(&ws, "mk");
    assert_eq!(
        skull_codegen_js::emit::emit_fn(&ws, &mk, "mk").unwrap(),
        "\
function mk() {
  let c0;
  c0 = ({__ty: \"P\", x: 1, y: 2});
  let v4 = c0;
  let v5 = v4.x;
  return v5;
}
"
    );
}
