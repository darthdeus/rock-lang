//! Module-global emission (T8d): module-level `let`/`global` state renders into
//! a scoped `__G` object (`Inst::Global` read -> `__G.<name>`, `StoreGlobal`
//! write -> `__G.<name> = ...`), with each global's initializer emitted once at
//! load in transitive-dependency order; a top-level fn referenced as a VALUE
//! (`Inst::Global` naming a function) co-emits and renders as its JS name. Node
//! execution is the correctness oracle; `tests/js_module_globals.rock`
//! cross-checks against luajit.

use crate::common::ws_with;
use skull_mir::mir::FuncRef;

fn node_available() -> bool {
    crate::common::node()
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn emit_closure(ws: &bone_semantic::inference::Workspace, root: &str) -> Result<String, String> {
    // Target JS explicitly (as the real per-test path does): a fn taken by value
    // keeps its surface name only on the JS target, where JS resolves it through
    // its own `discover_globals`; a non-Js target mangles the value ref.
    let mid = *ws
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("test module");
    let mut r = None;
    for (fid, f) in ws.module(mid).functions.iter() {
        if f.name.as_str() == root {
            r = Some(FuncRef {
                module: mid,
                func: fid,
            });
        }
    }
    skull_codegen_js::emit::emit_with_closure(ws, &[(r.unwrap(), root.to_string())])
        .map_err(|e| e.message)
}

fn eval_int(defs: &str, root: &str, expect: i64) {
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
    // `__G` is the scoped module-globals object the emitter targets; the run
    // harness (js_test.rs) supplies it in its preamble.
    let program = format!(
        "{}{}{}{}{}\nvar __G = {{}};\n{defs}\nconsole.log({root}());",
        skull_codegen_js::JS_NB_CHECK_SHIMS,
        skull_codegen_js::JS_VEC_CHECK_SHIMS,
        skull_codegen_js::JS_CALL_HELPER,
        skull_codegen_js::JS_STRING_RUNTIME,
        skull_codegen_js::JS_GLOBALS_RUNTIME,
    );
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
    let s = String::from_utf8_lossy(&out.stdout);
    let got: i64 = s
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("not an int: {s:?}\n{program}"));
    assert_eq!(got, expect, "wrong result\n{program}");
}

// A const module global read renders `__G.CANS` and inits once at load.
#[test]
fn const_global_read() {
    let src = "const CANS: int = 7\nfn root() -> int { CANS * 3 }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("const global emits");
    assert!(js.contains("__G.CANS"), "scoped global read:\n{js}");
    eval_int(&js, "root", 21);
}

// Mutable state: StoreGlobal + Global read through a call, reset per test.
#[test]
fn mutable_global_state() {
    let src = "global COUNTER: int = 0\n\
               fn bump() -> int { COUNTER = COUNTER + 1; return COUNTER }\n\
               fn root() -> int { COUNTER = 0; bump(); bump(); return bump() }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("mutable global emits");
    assert!(js.contains("__rock_globals["), "StoreGlobal renders:\n{js}");
    eval_int(&js, "root", 3);
}

// A global initialized from another global -- init order must place BASE first.
#[test]
fn derived_global_init_order() {
    let src = "global BASE: int = 100\n\
               global DERIVED: int = BASE + 5\n\
               fn root() -> int { DERIVED }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("derived global emits");
    eval_int(&js, "root", 105);
}

// TRANSITIVE init order: `global ACT = register(..)` where the `REG` read lives
// inside `register`, NOT the init expression. The order must still place REG's
// init first -- a shallow "reads in the init body" edge misses it (a
// registry global read `undefined` at load).
#[test]
fn transitive_global_init_order_through_call() {
    let src = "global REG: Map<string, int> = map_new()\n\
               fn register(k: string) -> int { REG[k] = 1; return 42 }\n\
               global ACT: int = register(\"x\")\n\
               fn root() -> int { ACT }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("transitive global emits");
    // REG's init line must precede ACT's.
    let reg_at = js
        .find("__rock_globals[\"global:<test>::REG::value\"] = __ginit")
        .expect("REG init emitted");
    let act_at = js
        .find("__rock_globals[\"global:<test>::ACT::value\"] = __ginit")
        .expect("ACT init emitted");
    assert!(
        reg_at < act_at,
        "REG must init before ACT (transitive dep):\n{js}"
    );
    eval_int(&js, "root", 42);
}

// A top-level fn referenced as a value: Inst::Global naming a function.
#[test]
fn fn_as_value() {
    let src = "fn add_one(x: int) -> int { x + 1 }\n\
               fn root() -> int { let f = add_one; f(5) }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("fn-as-value emits");
    eval_int(&js, "root", 6);
}

// A `nobox` global's STORAGE is its leaves (`__nbg_<base>_<i>`), so its init
// line has to assign those leaves -- assigning the base name leaves every leaf
// `undefined`, and the reads go to the leaves. The luajit twin is pinned by
// `skull/tests/nobox_values.rs`; this is the JS side of the same rule.
#[test]
fn a_nobox_global_initializes_its_leaves() {
    let src = "nobox value P { x: int, y: int }\n\
               global ORIGIN: P = P { x: 3, y: 4 }\n\
               fn root() -> int { return ORIGIN.x * 10 + ORIGIN.y }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("nobox global emits");
    assert!(
        js.contains("__rock_globals[\"global:<test>::ORIGIN::leaf0\"] = ")
            && js.contains("__rock_globals[\"global:<test>::ORIGIN::leaf1\"] = "),
        "the init must assign both leaves, not the boxed base name:\n{js}"
    );
    eval_int(&js, "root", 34);
}

// A global initialized from a lambda stores the lambda's CodeRef, and the code
// slot registry it resolves through is only STAGED once every body is emitted.
// The init lines therefore have to run after that staging: running them first
// makes `__rock_code_slot_ref` throw "missing Rock code slot" at load, and every
// in-unit `@test` reports as an invalid-JS unit rather than a failure.
#[test]
fn a_global_lambda_initializer_runs_after_the_code_slots_are_staged() {
    let src = "global SQUARE: fn(int) -> int = (x: int) => x * x\n\
               const OPS: [fn(int) -> int] = [(x: int) => x + 1]\n\
               fn root() -> int { return SQUARE(7) + OPS[0](4) }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("global lambda emits");
    eval_int(&js, "root", 54);
}
