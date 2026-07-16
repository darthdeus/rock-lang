//! MIR->JS emitter tests (docs/js-backend-plan.md S2). Lowers each of the eight
//! differential intrinsics' scalar fallback bodies and pins the emitted JS
//! exactly -- the two control-flow shapes (clamp's returning-arms diamond,
//! sign's store-then-merge via post-dominators) and the straight-line
//! expression bodies (lerp/round/fract/smoothstep, incl. intrinsic-call
//! rendering through the JS table). The emitted JS is node-verified against the
//! luajit reference values by the S3 execution harness; these tests pin the
//! structure. Vec overloads (multi-value return) reject -- the reject-don't-
//! fallback discipline that keeps the emitter honest as the subset grows.

use skull_mir::lower::{lower_function, ScalarCtx};

mod common;
use common::{emit_scalar, stdlib_ws};

// clamp: the returning-arms diamond -- every branch arm `return`s, so the
// post-dominator is the virtual exit and the `if` has no fall-through.
#[test]
fn clamp_emits_returning_arms_diamond() {
    let ws = stdlib_ws();
    assert_eq!(
        emit_scalar(&ws, "clamp", 3),
        "\
function clamp_fb(c0, c1, c2) {
  if ((c0 < c1)) {
    return c1;
  } else {
    if ((c0 > c2)) {
      return c2;
    } else {
      return c0;
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
  if ((c0 > 0)) {
    c1 = 1;
  } else {
    if ((c0 < 0)) {
      c2 = -1;
    } else {
      c2 = 0;
    }
    c1 = c2;
  }
  return c1;
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
        "function lerp_fb(c0, c1, c2) {\n  return (c0 + ((c1 - c0) * c2));\n}\n"
    );
    assert_eq!(
        emit_scalar(&ws, "round", 1),
        "function round_fb(c0) {\n  return Math.floor((c0 + 0.5));\n}\n"
    );
    assert_eq!(
        emit_scalar(&ws, "fract", 1),
        "function fract_fb(c0) {\n  return (c0 - Math.floor(c0));\n}\n"
    );
    assert_eq!(
        emit_scalar(&ws, "smoothstep", 3),
        "\
function smoothstep_fb(c0, c1, c2) {
  let c3;
  c3 = Math.min(Math.max(((c2 - c0) / (c1 - c0)), 0), 1);
  return ((c3 * c3) * (3 - (2 * c3)));
}
"
    );
}

// A Vec overload (scalarized multi-value return) is outside the subset and
// rejects -- reject, don't emit-and-hope. `Vec2.lerp` lowers to a fn returning
// two leaves (`return v0, v1`); its HIR still has 3 typed params (a, b: Vec2, t)
// so it can't be told apart by param count -- it's identified by its rejection.
#[test]
fn vec_overload_rejects() {
    let ws = stdlib_ws();
    let scx = ScalarCtx::new(&ws);
    let mut reject_msgs: Vec<String> = Vec::new();
    for (_, &mid) in ws.module_ids.iter() {
        let module = ws.module(mid);
        for (fid, f) in module.functions.iter() {
            if f.name.as_str() == "lerp" && f.lua_body.is_none() {
                let Ok(mf) = lower_function(&ws, &scx, mid, fid) else { continue };
                if let Err(e) = skull_codegen_js::emit::emit_fn(&ws, &mf, "lerp_vec") {
                    reject_msgs.push(e.message);
                }
            }
        }
    }
    assert!(
        reject_msgs.iter().any(|m| m.contains("multi-value return")),
        "expected a Vec `lerp` overload to reject with multi-value return; saw: {reject_msgs:?}"
    );
}
