//! A function that holds several wide `nobox` values must still load: LuaJIT
//! caps a function frame at 250 slots (`LJ_MAX_SLOTS`, "function or expression
//! too complex") and at 200 active locals (`LJ_MAX_LOCVAR`, "function at line N
//! has more than 200 local variables"), and a 50-leaf value costs one of each
//! wherever it is live.
//!
//! The receive of a wide multi-return used to cost the width twice: the call
//! wrote its results into one set of locals and every leaf was then copied into
//! a second set that held the value. With three 50-leaf values live and a
//! 52-slot call on top, that copy was the difference between 202 slots and 252.
//! The result locals now ARE the value's locals.
//!
//! Past that ceiling the compiler REJECTS the body with a Rock span instead of
//! emitting Lua that fails at `loadfile` with a chunk position and no source
//! location (`skull_codegen_lua::frame_limits`). The model that decides this is
//! a proven lower bound of LuaJIT's own count, so the core regression is an
//! AGREEMENT test: for every member of a family that walks a boundary, the
//! compiler's verdict must equal what the bundled LuaJIT does with the very Lua
//! the emitter would have produced.

use crate::common::{
    emit_module_without_frame_limits, function_body, luajit_loads, run_luajit, try_emit_module,
    ws_with_stdlib,
};

#[test]
fn retained_optional_loader_results_fit_the_local_budget() {
    let fields = (0..8).map(|i| format!("item{i}: Item")).collect::<Vec<_>>().join(", ");
    let scalars = (0..20).map(|i| format!("let s{i} = scalar(seed, {i})\n")).collect::<String>();
    let loads = (0..8).map(|i| format!("let item{i} = load(seed + {i}) ?? return null\n")).collect::<String>();
    let sum = (0..20).map(|i| format!("s{i}")).collect::<Vec<_>>().join(" + ");
    let items = (0..8).map(|i| format!("item{i}")).collect::<Vec<_>>().join(", ");
    let source = format!(r#"
nobox value Pair {{ x: float, y: float }}
value Triple {{ a: int, b: int, c: int }}
value Item {{
    a: int? = null, b: int? = null, c: int? = null, d: int? = null,
    first: Entity<int>? = null, second: Entity<int>? = null,
    triple: Triple, width: int, height: int, pivot: Pair,
}}
struct Bundle {{ checksum: int, {fields} }}
inline(lua) fn scalar(x: int, k: int) -> int = "return x + k"
fn make_item(seed: int) -> Item? {{
    if seed < 0 {{ return null }}
    return Item {{ triple: Triple {{ a: seed, b: seed + 1, c: seed + 2 }},
        width: seed, height: seed + 1, pivot: Pair {{ x: 0.5, y: 0.5 }} }}
}}
@nomangle fn gather(seed: int) -> Bundle? {{
    {scalars}
    let load = x => make_item(x)
    {loads}
    return Bundle {{ checksum: {sum}, {items} }}
}}
fn main() {{ assert(gather(-20) == null); print(gather(1).unwrap().checksum) }}
"#);
    let lua = try_emit_module(&source).expect("retained loader results must fit LuaJIT's local limit");
    luajit_loads(&lua, "retained_optional_loader_results").expect("emitted loader must load");
    assert_eq!(run_luajit(&format!("{lua}\nmain()\n"), "retained_optional_loader_results"), "210");
}

/// `held` values of a `leaves`-wide nobox struct, all live across the calls
/// that read them back.
/// The reader binds the complete value so field demand cannot move
/// the fixture away from the frame boundary it tests.
fn fixture(leaves: usize, held: usize) -> String {
    let fields: Vec<String> = (0..leaves).map(|k| format!("f{k}: float")).collect();
    let inits: Vec<String> = (0..leaves).map(|k| format!("f{k}: x + {k}.0")).collect();
    let copies: Vec<String> = (1..leaves).map(|k| format!("f{k}: w.f{k}")).collect();
    let mut src = format!(
        "nobox value W {{ {} }}\n\
         fn mk(x: float) -> W {{ return W {{ {} }} }}\n\
         fn tweak(w: W) -> W {{ return W {{ f0: w.f0 + 1.0, {} }} }}\n\
         fn check(value: W, k: int) -> float {{ let w = value; return w.f0 + w.f{} + float(k) }}\n\
         fn run(x: float) -> float {{\n    let v0 = mk(x)\n",
        fields.join(", "),
        inits.join(", "),
        copies.join(", "),
        leaves - 1,
    );
    for i in 1..held {
        src.push_str(&format!("    let v{i} = tweak(v{})\n", i - 1));
    }
    src.push_str("    let total = 0.0\n");
    for i in 0..held {
        src.push_str(&format!("    total += check(v{i}, {i})\n"));
    }
    src.push_str("    return total\n}\nfn main() { print(\"{run(1.0)}\") }\n");
    src
}

#[test]
fn three_live_fifty_leaf_values_load_and_run() {
    let lua = try_emit_module(&fixture(50, 3)).expect("emits");
    // v0: 1 + 50, v1: 2 + 50 + 1, v2: 3 + 50 + 2.
    assert_eq!(run_luajit(&format!("{lua}\nmain()\n"), "wide_slot_budget_3x50"), "159");
}

#[test]
fn a_wide_multi_return_lands_in_the_locals_that_hold_the_value() {
    let lua = try_emit_module(&fixture(50, 2)).expect("emits");
    let run = function_body(&lua, "run");
    let receive = run
        .lines()
        .position(|line| line.contains("= mk_"))
        .unwrap_or_else(|| panic!("no receive of mk in:\n{run}"));
    let next = run.lines().nth(receive + 1).unwrap_or("");
    assert!(
        !(next.trim_start().starts_with('r') && next.contains("= c")),
        "the receive is copied leaf by leaf into a second set of locals:\n{run}"
    );
}

/// Four live 50-leaf `nobox` values plus a call is the shape that does not
/// load. It must now be a Rock diagnostic with a source location, naming the
/// limit, the emitter's count, and the `nobox` value that cannot be boxed for
/// it -- not a `loadfile` failure after a successful compile.
#[test]
fn four_live_fifty_leaf_values_are_a_spanned_compile_error() {
    let err = try_emit_module(&fixture(50, 4))
        .expect_err("four live 50-leaf values must not emit: LuaJIT cannot load the result");
    let message = &err.message;
    assert!(message.contains("`run`"), "no Rock function named:\n{message}");
    assert!(message.contains("LuaJIT"), "no mention of the limit's owner:\n{message}");
    assert!(
        message.contains("LJ_MAX_LOCVAR") || message.contains("LJ_MAX_SLOTS"),
        "no LuaJIT limit named:\n{message}"
    );
    assert!(message.contains("nobox"), "no nobox contributor:\n{message}");
    assert!(
        message.contains("cannot be boxed"),
        "the remedy does not say a nobox value cannot be boxed:\n{message}"
    );
    let label = err.labels.first().expect("a spanned label");
    assert_eq!(label.span.file.as_str(), "<test>", "the rejection has no source location: {err:?}");
    assert_eq!(
        err.location.file.as_str(),
        "<test>",
        "the rejection's primary location is not in the source: {err:?}"
    );
    // And the Lua it would have emitted really is unloadable, so the rejection
    // is not the compiler being timid.
    let lua = emit_module_without_frame_limits(&fixture(50, 4));
    let load = luajit_loads(&lua, "wide_slot_budget_4x50").expect_err("must not load");
    assert!(
        load.contains("more than 200 local variables") || load.contains("too complex"),
        "unexpected LuaJIT parse error: {load}"
    );
}

// --- boundary agreement ----------------------------------------------------
//
// The model is a LOWER bound of LuaJIT's count, so it may miss an overflow but
// must never invent one. Only the bundled LuaJIT can settle that, and only on
// the Lua the emitter would actually have written -- which is why each point
// emits twice: once through the production path (whose `Err` IS the verdict)
// and once with the check off, to get the text `loadfile` judges.

/// One point of a boundary family. Agreement is demanded EXACTLY, in both
/// directions. The model is a lower bound in general -- it may miss an overflow
/// in a shape it does not count -- but on these families it is exact by
/// construction, and pinning it here is the whole point: a threshold that
/// drifted one slot in the lenient direction would still reject SOMETHING in a
/// wide enough window, so "the flip happened somewhere" is not a test.
///
/// Returns the shared verdict.
fn verdicts_agree(tag: &str, src: &str) -> bool {
    let compiled = try_emit_module(src);
    let lua = emit_module_without_frame_limits(src);
    let load = luajit_loads(&lua, tag);
    match (&compiled, &load) {
        (Ok(_), Ok(())) => true,
        (Err(_), Err(_)) => false,
        (Ok(_), Err(e)) => panic!(
            "{tag}: MISSED OVERFLOW -- the compiler accepted a program LuaJIT cannot load: {e}"
        ),
        (Err(e), Ok(())) => panic!(
            "{tag}: FALSE REJECTION -- the compiler refused a program LuaJIT loads.\n{}",
            e.message
        ),
    }
}

/// Walk `lo..=hi` of a family and require the boundary to sit inside the
/// window: a window with only one verdict pins nothing.
fn walk(name: &str, lo: usize, hi: usize, mk: impl Fn(usize) -> String) {
    let verdicts: Vec<bool> =
        (lo..=hi).map(|w| verdicts_agree(&format!("{name}_{w}"), &mk(w))).collect();
    assert!(
        verdicts.iter().any(|ok| *ok),
        "{name}: every member of {lo}..={hi} was rejected -- the window is past the boundary"
    );
    assert!(
        verdicts.iter().any(|ok| !*ok),
        "{name}: every member of {lo}..={hi} was accepted -- the window is below the boundary"
    );
    // Monotone by construction: once the frame is too wide, widening it further
    // cannot make it load again.
    let flip = verdicts.iter().position(|ok| !*ok).expect("a rejected member");
    assert!(
        verdicts[flip..].iter().all(|ok| !*ok),
        "{name}: verdicts are not monotone across {lo}..={hi}: {verdicts:?}"
    );
}

/// The declarations of a `leaves`-wide `nobox` value and the helpers that
/// produce and read one.
/// The reader binds the complete value to retain full transport for the boundary walkers.
fn wide_decls(leaves: usize) -> String {
    let fields: Vec<String> = (0..leaves).map(|k| format!("f{k}: float")).collect();
    let inits: Vec<String> = (0..leaves).map(|k| format!("f{k}: x + {k}.0")).collect();
    format!(
        "nobox value W {{ {} }}\n\
         fn mk(x: float) -> W {{ return W {{ {} }} }}\n\
         fn use_w(value: W, k: int) -> float {{ let w = value; return w.f0 + w.f{} + float(k) }}\n\
         fn one(x: float, k: int) -> float {{ return x * float(k) + 1.0 }}\n",
        fields.join(", "),
        inits.join(", "),
        leaves - 1,
    )
}

/// `pad` scalar locals live across one wide receive and one wide call. `pad`
/// steps the frame by exactly one slot and one local, which is what makes an
/// off-by-one in either direction visible.
fn padded_receive(leaves: usize, pad: usize) -> String {
    let mut src = wide_decls(leaves);
    src.push_str("fn run(x: float) -> float {\n");
    for i in 0..pad {
        src.push_str(&format!("    let a{i} = one(x, {i})\n"));
    }
    src.push_str("    let v = mk(x)\n    let total = use_w(v, 0)\n");
    for i in 0..pad {
        src.push_str(&format!("    total += a{i}\n"));
    }
    src.push_str("    return total\n}\nfn main() { print(\"{run(1.0)}\") }\n");
    src
}

/// The same wide receive and call, inside a counted `for`. The loop adds its
/// four hidden control locals and its own body-scoped register line, so a model
/// that forgot either would disagree with LuaJIT here and nowhere else.
fn padded_receive_in_loop(leaves: usize, pad: usize) -> String {
    let mut src = wide_decls(leaves);
    src.push_str("fn run(x: float) -> float {\n");
    for i in 0..pad {
        src.push_str(&format!("    let a{i} = one(x, {i})\n"));
    }
    src.push_str(
        "    let total = 0.0\n    for i in 0..3 {\n        let v = mk(x + float(i))\n        \
         total += use_w(v, i)\n    }\n",
    );
    for i in 0..pad {
        src.push_str(&format!("    total += a{i}\n"));
    }
    src.push_str("    return total\n}\nfn main() { print(\"{run(1.0)}\") }\n");
    src
}

/// A wide producer written directly into a wide consumer's argument list, which
/// the emitter splices as `use_w(mk(x), 0)` rather than materializing the
/// leaves into locals first.
fn nested_wide_call(leaves: usize, pad: usize) -> String {
    let mut src = wide_decls(leaves);
    src.push_str("fn run(x: float) -> float {\n");
    for i in 0..pad {
        src.push_str(&format!("    let a{i} = one(x, {i})\n"));
    }
    src.push_str("    let total = use_w(mk(x), 0)\n");
    for i in 0..pad {
        src.push_str(&format!("    total += a{i}\n"));
    }
    src.push_str("    return total\n}\nfn main() { print(\"{run(1.0)}\") }\n");
    src
}

/// The wide value live inside a closure body, which is emitted as its own Lua
/// function with its own frame.
fn wide_in_closure(leaves: usize, pad: usize) -> String {
    let mut src = wide_decls(leaves);
    src.push_str("fn run(x: float) -> float {\n    let f = (y: float) => {\n");
    for i in 0..pad {
        src.push_str(&format!("        let a{i} = one(y, {i})\n"));
    }
    src.push_str("        let v = mk(y)\n        let total = use_w(v, 0)\n");
    for i in 0..pad {
        src.push_str(&format!("        total += a{i}\n"));
    }
    src.push_str("        return total\n    }\n    return f(x)\n}\n");
    src.push_str("fn main() { print(\"{run(1.0)}\") }\n");
    src
}

/// `pad` live locals and nothing wide: this family walks the LOCALS cap rather
/// than the slot cap, one local per step. The accumulator goes through a call
/// so the `+=` chain is not folded into one parenthesized expression, which
/// would hit LuaJIT's separate 200-syntax-level cap first.
fn flat_locals(pad: usize) -> String {
    let mut src = String::from("fn one(x: float, k: int) -> float { return x * float(k) + 1.0 }\n");
    src.push_str("fn run(x: float) -> float {\n");
    for i in 0..pad {
        src.push_str(&format!("    let a{i} = one(x, {i})\n"));
    }
    src.push_str("    let total = 0.0\n");
    for i in 0..pad {
        src.push_str(&format!("    total = one(total + a{i}, {i})\n"));
    }
    src.push_str("    return total\n}\nfn main() { print(\"{run(1.0)}\") }\n");
    src
}

// The slot windows below straddle 250 exactly: the first member models 248
// slots and the last 251, so an off-by-one in either direction moves a verdict.

#[test]
fn a_wide_receive_agrees_with_luajit_across_the_boundary() {
    walk("padded_receive", 144, 147, |pad| padded_receive(50, pad));
}

#[test]
fn a_wide_call_in_a_loop_agrees_with_luajit_across_the_boundary() {
    walk("in_loop", 134, 137, |pad| padded_receive_in_loop(50, pad));
}

#[test]
fn a_nested_wide_call_agrees_with_luajit_across_the_boundary() {
    walk("nested", 144, 147, |pad| nested_wide_call(50, pad));
}

#[test]
fn a_wide_value_in_a_closure_agrees_with_luajit_across_the_boundary() {
    walk("closure", 144, 147, |pad| wide_in_closure(50, pad));
}

/// The locals cap: 199, 200, 201 and 202 active locals, one per step.
#[test]
fn the_local_variable_cap_agrees_with_luajit_across_the_boundary() {
    walk("flat_locals", 198, 201, flat_locals);
}

/// The retained width witness: three live 50-leaf values load, four do not.
#[test]
fn the_held_wide_value_witness_agrees_with_luajit() {
    walk("held_values", 3, 4, |held| fixture(50, held));
}

/// The rejection must reach the INCREMENTAL re-emit path too, not only a fresh
/// full build. the host watch command re-emits the edited body and reassembles; a body
/// edit that pushes a function over the frame limit must fail the reload with
/// the Rock diagnostic rather than publish Lua the engine cannot parse
///.
#[test]
fn the_frame_limit_rejection_reaches_the_incremental_path() {
    let fits = padded_receive(50, 100);
    let over = padded_receive(50, 160);
    let ws1 = ws_with_stdlib(&fits);
    let mid = skull::user_module_id(&ws1).expect("user module");
    let (_full, mut cache) = skull::mir_emit::emit_full(&ws1, mid).expect("emit_full fits");

    let ws2 = ws_with_stdlib(&over);
    let run = run_body_id(&ws2);
    let err = skull::mir_emit::emit_incremental(&ws2, &mut cache, &[run])
        .err()
        .expect("the widened body must fail the incremental re-emit");
    assert!(
        err.message.contains("too wide for LuaJIT"),
        "the incremental path reported something else: {}",
        err.message
    );
}

/// The `(ModuleId, BodyOwner)` reinferred id of the fixture's `run`.
fn run_body_id(
    ws: &bone_semantic::inference::Workspace,
) -> (bone_semantic::prelude::ModuleId, bone_semantic::hir::BodyOwner) {
    for module in ws.modules.iter() {
        for (fid, f) in module.functions.iter() {
            if f.name.as_str() == "run" {
                return (module.id, bone_semantic::hir::BodyOwner::Function(fid));
            }
        }
    }
    panic!("no fn named run");
}
