//! MapNext on JS (D1, docs/js-deployment-plan.md): the `Inst::MapNext` emit arm
//! + the `__map_next` cursor-cached keys-iterator shim (`JS_MAP_ITER_RUNTIME`).
//! The emit-shape test needs no node; the shim behavior pins run the ACTUAL
//! prelude const under node, node-gated with a skip-log.

use skull_codegen_js::JS_MAP_ITER_RUNTIME;
use crate::common::{emit_user, ws_with};

#[test]
fn map_iter_emits_map_next_and_get() {
    let ws = ws_with(
        r#"
fn f(m: Map<string, int>) -> int {
    let total = 0
    for (k, v) in m { total = total + v }
    return total
}
"#,
    );
    let js = emit_user(&ws, "f");
    // Cursor step writes the key cell in place, value cell reads `.get` guarded
    // on the exhaustion sentinel.
    assert!(js.contains("__map_next("), "no __map_next call in:\n{js}");
    assert!(
        js.contains("=== null ? null :") && js.contains(".get("),
        "no guarded value read in:\n{js}"
    );
}

fn node_available() -> bool {
    crate::common::node()
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Run the map-iter prelude + `body`, return trimmed stdout.
fn node_run(body: &str) -> String {
    let program = format!("{JS_MAP_ITER_RUNTIME}\n{body}");
    let out = crate::common::node()
        .arg("-e")
        .arg(&program)
        .output()
        .expect("spawn node");
    assert!(
        out.status.success(),
        "node failed:\n--- program ---\n{program}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Drive `__map_next` to exhaustion from a null seed, collecting the keys it
/// yields -- the exact loop shape the emitter produces.
const DRAIN: &str = "function drain(m){var out=[],k=null; while(true){k=__map_next(m,k); if(k===null)break; out.push(k);} return out;}";

#[test]
fn fresh_iteration_yields_insertion_order() {
    if !node_available() {
        eprintln!("SKIP fresh_iteration_yields_insertion_order: node not found");
        return;
    }
    let got = node_run(&format!(
        "{DRAIN} var m=new Map(); m.set('a',1); m.set('b',2); m.set('c',3); console.log(drain(m).join(','));"
    ));
    assert_eq!(got, "a,b,c");
}

#[test]
fn empty_map_yields_nothing() {
    if !node_available() {
        eprintln!("SKIP empty_map_yields_nothing: node not found");
        return;
    }
    let got = node_run(&format!("{DRAIN} var m=new Map(); console.log('['+drain(m).join(',')+']');"));
    assert_eq!(got, "[]");
}

#[test]
fn exhaustion_returns_null_then_restarts_fresh() {
    if !node_available() {
        eprintln!("SKIP exhaustion_returns_null_then_restarts_fresh: node not found");
        return;
    }
    // First drain exhausts; a second null-seeded drain restarts from the top.
    let got = node_run(&format!(
        "{DRAIN} var m=new Map(); m.set('x',1); m.set('y',2); var a=drain(m).join(','); var b=drain(m).join(','); console.log(a+'|'+b);"
    ));
    assert_eq!(got, "x,y|x,y");
}

#[test]
fn break_then_restart_same_map() {
    if !node_available() {
        eprintln!("SKIP break_then_restart_same_map: node not found");
        return;
    }
    // Advance once (break mid-loop), then a fresh null seed restarts from 'a'.
    let got = node_run(&format!(
        "var m=new Map(); m.set('a',1); m.set('b',2); m.set('c',3); \
         var first=__map_next(m,null); \
         var out=[],k=null; while(true){{k=__map_next(m,k); if(k===null)break; out.push(k);}} \
         console.log(first+'|'+out.join(','));"
    ));
    assert_eq!(got, "a|a,b,c");
}

#[test]
fn nested_iteration_over_same_map() {
    if !node_available() {
        eprintln!("SKIP nested_iteration_over_same_map: node not found");
        return;
    }
    // Outer and inner loops over the SAME map interleave through one cursor
    // cache; the rescan-on-mismatch keeps the outer walk correct after the inner
    // drains. Expect the full cross product in insertion order.
    let got = node_run(&format!(
        "var m=new Map(); m.set('a',1); m.set('b',2); \
         var pairs=[]; var ko=null; \
         while(true){{ko=__map_next(m,ko); if(ko===null)break; \
           var ki=null; while(true){{ki=__map_next(m,ki); if(ki===null)break; pairs.push(ko+ki);}} }} \
         console.log(pairs.join(','));"
    ));
    assert_eq!(got, "aa,ab,ba,bb");
}

#[test]
fn deletion_during_iteration_follows_live_iterator() {
    if !node_available() {
        eprintln!("SKIP deletion_during_iteration_follows_live_iterator: node not found");
        return;
    }
    // Deleting a not-yet-visited key mid-walk drops it (live JS keys iterator,
    // backend-defined per doctrine).
    let got = node_run(&format!(
        "var m=new Map(); m.set('a',1); m.set('b',2); m.set('c',3); \
         var out=[],k=null; while(true){{k=__map_next(m,k); if(k===null)break; out.push(k); if(k==='a')m.delete('b');}} \
         console.log(out.join(','));"
    ));
    assert_eq!(got, "a,c");
}

#[test]
fn map_mutated_between_loops() {
    if !node_available() {
        eprintln!("SKIP map_mutated_between_loops: node not found");
        return;
    }
    // A fresh loop after an insert sees the new key (each null seed re-reads
    // m.keys()).
    let got = node_run(&format!(
        "{DRAIN} var m=new Map(); m.set('a',1); var a=drain(m).join(','); m.set('b',2); var b=drain(m).join(','); console.log(a+'|'+b);"
    ));
    assert_eq!(got, "a|a,b");
}
