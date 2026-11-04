//! JS emission reports EVERY rejected definition, not just the first one it
//! reaches.
//!
//! The `--backend js` skip histogram IS the emitter worklist -- it is what says
//! which constructs the subset still owes. Reporting only the first
//! unemittable definition per emission makes that worklist a floor: a test whose
//! closure needs three unsupported things reports one, and the next two only
//! appear after the first is built. Same defect (and same fix) as the luajit
//! path's `rock-lang/crates/skull/tests/multi_rejection.rs`.

use skull_mir::mir::FuncRef;

fn test_fn_ref(ws: &bone_semantic::inference::Workspace, name: &str) -> FuncRef {
    let mid = *ws
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("test module");
    for (fid, f) in ws.module(mid).functions.iter() {
        if f.name.as_str() == name {
            return FuncRef {
                module: mid,
                func: fid,
            };
        }
    }
    panic!("no fn `{name}` in <test> module");
}

/// Every reported rejection for the closure rooted at `root`: the primary plus
/// its siblings (the shape the driver's skip channel and every renderer expand).
fn reject_messages(src: &str) -> Vec<String> {
    let ws = crate::common::ws_with(src);
    let root = test_fn_ref(&ws, "root");
    let err = skull_codegen_js::emit::emit_with_closure(&ws, &[(root, "root".to_string())])
        .expect_err("must reject");
    std::iter::once(err.message.clone())
        .chain(err.siblings.iter().map(|s| s.message.clone()))
        .collect()
}

/// Two DIFFERENT definitions in one closure, each unemittable for its own
/// reason. The walk used to abort at the first body it could not finish, so `b`
/// was invisible until `a` was supported.
#[test]
fn every_failing_definition_is_reported_not_just_the_first() {
    let all = reject_messages(
        "inline(lua) fn lua_one(x: int) -> int = \"\"\" return x \"\"\"\n\
         inline(lua) fn lua_two(x: int) -> int = \"\"\" return x \"\"\"\n\
         fn a(x: int) -> int { let v = lua_one(x); v + 1 }\n\
         fn b(x: int) -> int { let v = lua_two(x); v + 2 }\n\
         fn root(y: int) -> int { a(y) + b(y) }\n",
    );
    for want in ["lua_one", "lua_two"] {
        assert!(
            all.iter().any(|m| m.contains(want)),
            "no rejection names `{want}`:\n{}",
            all.join("\n")
        );
    }
}

/// Two unmapped callees in ONE body: the discovery scan reported the first and
/// dropped the rest, so a body needing two intrinsics looked like it needed one.
#[test]
fn every_unmapped_callee_in_one_body_is_reported() {
    let all = reject_messages(
        "inline(lua) fn lua_one(x: int) -> int = \"\"\" return x \"\"\"\n\
         inline(lua) fn lua_two(x: int) -> int = \"\"\" return x \"\"\"\n\
         fn root(y: int) -> int { lua_one(y) + lua_two(y) }\n",
    );
    for want in ["lua_one", "lua_two"] {
        assert!(
            all.iter().any(|m| m.contains(want)),
            "no rejection names `{want}`:\n{}",
            all.join("\n")
        );
    }
}

/// Each `nobox` rejection names the body it was emitting. Without it an
/// accumulated list of sinks across several bodies reads as one site repeated,
/// which is exactly the misreading the accumulation exists to prevent.
#[test]
fn a_nobox_rejection_names_the_body_it_was_emitting() {
    // Interpolating a WHOLE tuple carrying the value is the sink: it asks for one
    // runtime-typed value, so the element materializes and reports through the
    // residue channel, which is the one that carries a body name.
    //
    // A `[(NV2, int)]` element used to stand here (case-matrix row 13) and stopped
    // materializing when the tuple box learned to carry the leaves. Neither plain
    // container works either: `[NV2]` column-stores, and so does a `Map<_, NV2>`
    // VALUE as well.
    const TWO_SINK_FNS: &str = "\
nobox value NV2 { x: float, y: float }
fn a(k: float) -> int { let t = (NV2{x:k,y:0.0}, 1); print(\"{t}\"); 0 }
fn b(k: float) -> int { let t = (NV2{x:0.0,y:k}, 2); print(\"{t}\"); 0 }
fn root(y: float) -> int { a(y) + b(y) }
";
    let all = reject_messages(TWO_SINK_FNS);
    for want in ["while emitting `a`", "while emitting `b`"] {
        assert!(
            all.iter().any(|m| m.contains(want)),
            "no rejection says `{want}`:\n{}",
            all.join("\n")
        );
    }
}

/// The reported ORDER is fixed. Definitions come off a worklist in post-order,
/// so the report is sorted by emitted name -- otherwise any test asserting the
/// primary would be flaky rather than wrong.
#[test]
fn the_rejection_order_is_deterministic() {
    const SRC: &str = "inline(lua) fn lua_one(x: int) -> int = \"\"\" return x \"\"\"\n\
         inline(lua) fn lua_two(x: int) -> int = \"\"\" return x \"\"\"\n\
         fn a(x: int) -> int { let v = lua_one(x); v + 1 }\n\
         fn b(x: int) -> int { let v = lua_two(x); v + 2 }\n\
         fn root(y: int) -> int { a(y) + b(y) }\n";
    let first = reject_messages(SRC);
    for _ in 0..4 {
        assert_eq!(first, reject_messages(SRC));
    }
}

/// A single rejection must not grow a spurious sibling.
#[test]
fn a_lone_rejection_has_no_siblings() {
    let all = reject_messages(
        "inline(lua) fn lua_one(x: int) -> int = \"\"\" return x \"\"\"\n\
         fn root(y: int) -> int { lua_one(y) }\n",
    );
    assert_eq!(
        all.len(),
        1,
        "expected exactly one rejection, got:\n{}",
        all.join("\n")
    );
}

#[test]
fn a_rejected_registration_target_names_the_unsupported_callee() {
    let ws = crate::common::ws_with(
        r#"
deco init hook() {}
inline(lua) fn unsupported_callback_leaf() = "print('native')"
@hook fn callback() { unsupported_callback_leaf() }
fn main() {}
"#,
    );
    let mid = ws.module_ids[&ustr::Ustr::from("<test>")];
    let error = skull_codegen_js::emit::emit_program(&ws, mid)
        .err()
        .expect("callback must reject");
    let messages: Vec<_> = std::iter::once(&error)
        .chain(error.siblings.iter())
        .map(|error| error.message.as_str())
        .collect();
    assert!(
        messages
            .iter()
            .any(|message| message.contains("unsupported_callback_leaf")),
        "callback rejection was hidden by staging: {messages:?}"
    );
}
