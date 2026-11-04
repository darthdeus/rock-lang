//! Emitter discipline pin: the JS backend hoists every fn to top-level, lowers
//! closures via the shared `convert_closures` MIR pass (each `Inst::Lambda`
//! becomes an env record + a hoisted sibling `function <name>(...)`), and lowers
//! coroutine bodies to a NAMED block trampoline
//! (`function <name>__step(..) { for (;;) switch (__bb) {..} }`). So a lambda /
//! nested-fn / coroutine-body emission never introduces an inline ANONYMOUS
//! `function (...)` expression -- every emitted fn carries a name, and captures
//! ride lexical `c<N>` cells, not an inline closure.
//!
//! `discipline_shapes_emit_no_inline_function` is the pin. The anonymous-
//! `function(` detector (`anon_function_hits`) also finds the TWO non-closure
//! glue paths that DO emit `function(` (pinned separately in
//! `glue_paths_use_anonymous_function` so a regression in either is noticed) --
//! these are fixed generated glue, NOT closure/body emission, hence out of scope
//! for the discipline pin:
//!   - `build_vt_eq_defs` (emit.rs): `__rock_vt_eq["N"] = function(a, b) {..}`,
//!     the type-aware eq registry for GC-carrying value structs.
//!   - coroutine-op intrinsic templates (emit.rs:2530): `resume`/`result`/
//!     `status`/`last_error`/`fire_hot_reload` render as `(function(c){..})(x)`
//!     IIFEs at the CONSUMER call site.
//! The fixed runtime preamble in `lib.rs` (`JS_CALL_HELPER`, `JS_STRUCTURAL_EQ`,
//! `JS_CORO_RUNTIME`, ...) also holds `function(` helpers; it is prepended by the
//! driver, never returned by `emit_with_closure`, so it is naturally out of
//! scope here.

use crate::common::ws_with;
use skull_mir::mir::FuncRef;

fn emit(ws: &bone_semantic::inference::Workspace, root: &str) -> String {
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
    skull_codegen_js::emit::emit_with_closure(ws, &[(r.expect(root), root.to_string())])
        .unwrap_or_else(|e| panic!("emit `{root}`: {}", e.message))
}

/// Occurrences of `function` immediately followed (modulo blanks) by `(` -- an
/// anonymous function expression. A hoisted top-level `function <name>(` never
/// matches (an identifier sits between `function` and `(`). Each hit carries a
/// bit of leading context for diagnosis.
fn anon_function_hits(js: &str) -> Vec<String> {
    let mut hits = Vec::new();
    let bytes = js.as_bytes();
    let mut i = 0;
    while let Some(off) = js[i..].find("function") {
        let start = i + off;
        let mut j = start + "function".len();
        while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
            j += 1;
        }
        if j < bytes.len() && bytes[j] == b'(' {
            let ctx_lo = start.saturating_sub(24);
            hits.push(js[ctx_lo..(j + 1).min(js.len())].replace('\n', " "));
        }
        i = start + "function".len();
    }
    hits
}

const CAPS_LAMBDA: &str = "\
fn caps_lambda(base: int) -> int {\n\
    let add = (x: int) => x + base\n\
    return add(5) + add(7)\n\
}\n";

const NESTED_FN: &str = "\
fn with_nested(n: int) -> int {\n\
    let apply = (f: (int) -> int, x: int) => f(x)\n\
    let g = (y: int) => y + n\n\
    return apply(g, 21)\n\
}\n";

const CO_FN: &str = "\
co fn gen_from(start: int) -> Coroutine<int> {\n\
    let i = start\n\
    loop {\n\
        yield i\n\
        i = i + 1\n\
    }\n\
}\n";

// An ordinary body the structurer rejects (nested-loop shared tail) -> the T8h
// block-trampoline fallback: NAMED `function <name>(` + `for (;;) switch`.
const TRAMPOLINE_BODY: &str = "\
fn nested_loop_tail(sel: int) -> int {\n\
    let out = match sel {\n\
        1 | 2 | 3 => {\n\
            let acc = 0\n\
            let i = 0\n\
            while i < 5 { acc = acc + i; i = i + 1 }\n\
            acc\n\
        }\n\
        _ => 100\n\
    }\n\
    return out + sel\n\
}\n";

#[test]
fn discipline_shapes_emit_no_inline_function() {
    for (label, src, root) in [
        ("caps_lambda", CAPS_LAMBDA, "caps_lambda"),
        ("nested_fn", NESTED_FN, "with_nested"),
        ("co_fn", CO_FN, "gen_from"),
        ("trampoline_body", TRAMPOLINE_BODY, "nested_loop_tail"),
    ] {
        let ws = ws_with(src);
        let js = emit(&ws, root);
        let hits = anon_function_hits(&js);
        assert!(
            hits.is_empty(),
            "{label}: emitted an inline anonymous `function(` -- closure/body \
             emission must hoist to a NAMED fn, not an inline closure:\n{hits:#?}\n\
             --- emitted ---\n{js}",
        );
        // Every emitted fn is a named declaration.
        assert!(
            js.contains("function "),
            "{label}: expected a hoisted `function <name>(`"
        );
    }
}

/// Positive markers so the pin can't pass by emitting nothing / a degenerate
/// shape: the coroutine body is a named block trampoline.
#[test]
fn coroutine_body_is_a_named_trampoline() {
    let ws = ws_with(CO_FN);
    let js = emit(&ws, "gen_from");
    assert!(
        js.contains("__step("),
        "co fn should emit a named __step fn:\n{js}"
    );
    assert!(
        js.contains("for (;;)"),
        "co fn step should be a block trampoline:\n{js}"
    );
    assert!(
        anon_function_hits(&js).is_empty(),
        "co fn body must not inline `function(`:\n{js}"
    );
}

const GC_EQ: &str = "\
value VecBox { xs: [int], n: int }\n\
fn eq_box(a: VecBox, b: VecBox) -> bool { return a == b }\n";

const CO_CONSUMER: &str = "\
co fn gen_from(start: int) -> Coroutine<int> {\n\
    let i = start\n\
    loop { yield i; i = i + 1 }\n\
}\n\
fn use_resume() -> int {\n\
    let c = gen_from(5)\n\
    return c.resume() ?? 0\n\
}\n";

/// The two NON-closure glue paths that legitimately emit anonymous `function(`.
/// This is a FINDING pin, not a violation: it documents WHERE the shape is
/// allowed (generated eq-registry + coroutine-op IIFE templates), so removing or
/// reshaping either glue is noticed here rather than silently.
#[test]
fn glue_paths_use_anonymous_function() {
    // vt_eq registry for a GC-carrying value struct.
    let ws = ws_with(GC_EQ);
    let js = emit(&ws, "eq_box");
    assert!(
        js.contains("__rock_vt_eq[") && !anon_function_hits(&js).is_empty(),
        "GC value-struct eq should emit `__rock_vt_eq[..] = function(a, b)`:\n{js}",
    );

    // resume() at a coroutine consumer -> an `(function(c){..})(x)` IIFE.
    let ws = ws_with(CO_CONSUMER);
    let js = emit(&ws, "use_resume");
    assert!(
        !anon_function_hits(&js).is_empty(),
        "resume() consumer should emit a `(function(c){{..}})` IIFE:\n{js}",
    );
}
