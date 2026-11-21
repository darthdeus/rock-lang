//! JS emission reports EVERY rejected definition, not just the first one it
//! reaches.
//!
//! The `--backend js` skip histogram IS the emitter worklist -- it is what says
//! which constructs the subset still owes. Reporting only the first
//! unemittable definition per emission makes that worklist a floor: a test whose
//! closure needs three unsupported things reports one, and the next two only
//! appear after the first is built. Same defect (and same fix) as the luajit
//! path's `rock-lang/crates/skull/tests/multi_rejection.rs`.

/// Every reported rejection for the closure rooted at `root`: the primary plus
/// its siblings (the shape the driver's skip channel and every renderer expand).
fn reject_messages(src: &str) -> Vec<String> {
    let ws = crate::common::ws_with(src);
    let root = crate::common::test_fn_ref(&ws, "root");
    let err = skull_codegen_js::emit::emit_with_closure(&ws, &[(root, "root".to_string())])
        .expect_err("must reject");
    std::iter::once(err.message.clone())
        .chain(err.siblings.iter().map(|s| s.message.clone()))
        .collect()
}

const TWO_UNMAPPED_DEFINITIONS: &str = "inline(lua) fn lua_one(x: int) -> int = \"\"\" return x \"\"\"\n\
     inline(lua) fn lua_two(x: int) -> int = \"\"\" return x \"\"\"\n\
     fn a(x: int) -> int { let v = lua_one(x); v + 1 }\n\
     fn b(x: int) -> int { let v = lua_two(x); v + 2 }\n\
     fn root(y: int) -> int { a(y) + b(y) }\n";

/// Each `nobox` rejection names the body it was emitting. Without it an
/// accumulated list of sinks across several bodies reads as one site repeated,
/// which is exactly the misreading the accumulation exists to prevent.
#[test]
fn a_nobox_rejection_names_the_body_it_was_emitting() {
    // Tuple interpolation materializes the value and attributes the rejection
    // to its body.
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
