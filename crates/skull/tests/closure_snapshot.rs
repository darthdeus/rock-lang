//! Codegen-shape contract for how a capturing lambda snapshots its captures into
//! an env record. The pre-P2.6 IIFE
//! by-value snapshot is gone: every capture is a record FIELD instead. A
//! single-assignment capture is snapshotted BY VALUE (the field holds the value);
//! a mutable capture is BOXED and the field holds the shared box table, so a later
//! write is observed by reference. Neither renders an inline `function(` closure
//! at the creation site. Record mechanics in depth: `env_record_hoist.rs`.
use crate::common::{emit_module, function_body, run_luajit};

/// True if `body` declares a box cell -- a `local c<N> = {}` line (the box-init
/// shape). Matched precisely rather than via the bare `= {}` substring, which an
/// empty vec/map literal or a prelude table also produces.
fn declares_box(body: &str) -> bool {
    body.lines().any(|l| {
        // The empty box table literal `{}`. The shared closure-conversion pass
        // materializes it at the owner scope entry, and regalloc may route it
        // through a temp (`r<N> = {}` then stored into the box cell) or assign the
        // cell directly; pre-cutover codegen wrote it inline as `local c<N> = {}`.
        // The caller's `.v = ` assertion confirms it is a box (not an empty
        // map/tuple literal).
        let t = l.trim();
        let t = t.strip_prefix("local ").unwrap_or(t);
        t.strip_prefix(['c', 'r'])
            .and_then(|r| r.strip_suffix(" = {}"))
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    })
}

// A closure created ONCE (no loop), capturing single-assignment cells a, b --
// the `sprite_mesh_submit` shape: `let a = ..; let b = ..; defer(() => use(a,b))`.
// `defer` STORES the closure in a global so it escapes (can't be inlined away),
// matching `queue_custom_draw` storing the draw fn in a batch for flush time.
const NONLOOP_CAPTURE: &str = r#"
@nomangle inline(lua) fn use_two(a: int, b: int) = "_G.__sink = (a or 0) + (b or 0)"

fn submit(x: int) -> fn() {
    let a = x + 1
    let b = x + 2
    return () => { use_two(a, b) }
}

fn main() { let f = submit(3) f() }
"#;

#[test]
fn nonloop_capture_hoists_to_by_value_record() {
    let lua = emit_module(NONLOOP_CAPTURE);
    let body = function_body(&lua, "submit");
    assert!(
        body.contains("setmetatable({ __slot = __rock_code_slot_ref("),
        "the capture hoists to an env record, not an inline closure:\n{body}"
    );
    assert!(!body.contains("__fn"), "a Rock closure stores a CodeRef, not a frozen body:\n{body}");
    assert!(
        !body.contains("function("),
        "no inline closure at the creation site:\n{body}"
    );
    assert!(
        !body.contains("return function"),
        "no by-value snapshot IIFE (deleted in P2.6):\n{body}"
    );
    // The single-assignment captures are by-value fields -- no box declared.
    assert!(!declares_box(&body), "single-assignment captures are by value, not boxed:\n{body}");
}

// A closure created INSIDE a loop, capturing a cell assigned inside the loop --
// each iteration's env record must snapshot ITS value (per-iteration), which the
// by-value field does directly (the field reads the loop-body local each time).
const LOOP_CAPTURE: &str = r#"
@nomangle inline(lua) fn use_one(a: int) = "_G.__sink = (a or 0)"

fn submit_loop(n: int) -> [fn() -> int] {
    let fns: [fn() -> int] = []
    for i in 0..n {
        let a = i + 1
        fns.push(() => a)
    }
    return fns
}

fn main() {
    let fns = submit_loop(3)
    for f in fns { print(f()) }
}
"#;

#[test]
fn loop_capture_snapshots_per_iteration_via_record() {
    let lua = emit_module(LOOP_CAPTURE);
    let body = function_body(&lua, "submit_loop");
    assert!(
        body.contains("setmetatable({ __slot = __rock_code_slot_ref("),
        "a loop-created capture hoists to a per-iteration env record:\n{body}"
    );
    assert!(!body.contains("__fn"), "a Rock closure stores a CodeRef, not a frozen body:\n{body}");
    assert!(!body.contains("return function"), "no by-value snapshot IIFE (deleted in P2.6):\n{body}");
    let out = run_luajit(&format!("{lua}\nmain()\n"), "loopcap");
    assert_eq!(out, "1\n2\n3", "each record must snapshot its iteration's value; got:\n{out}");
}

// A captured cell mutated after capture (multi-store) lowers to a BOX
// (`local c<N> = {}`, every store/load through `.v`); the record field holds the
// shared box table (by reference), so the closure observes the later write. NOT
// snapshotted by value.
const MUTATED_CAPTURE: &str = r#"
@nomangle inline(lua) fn use_one(a: int) = "_G.__sink = (a or 0)"

fn capture_box() -> fn() {
    let n = 0
    let g = () => { use_one(n) }
    n = 5
    return g
}

fn main() { let h = capture_box() h() }
"#;

#[test]
fn mutated_capture_lowers_to_box_threaded_into_record() {
    let lua = emit_module(MUTATED_CAPTURE);
    let body = function_body(&lua, "capture_box");
    assert!(declares_box(&body), "a mutable capture must declare a box `local c<N> = {{}}`:\n{body}");
    assert!(body.contains(".v = "), "a mutable capture stores through `.v`:\n{body}");
    assert!(
        body.contains("setmetatable({ __slot = __rock_code_slot_ref("),
        "the box is threaded into an env record:\n{body}"
    );
    assert!(!body.contains("__fn"), "a Rock closure stores a CodeRef, not a frozen body:\n{body}");
    assert!(!body.contains("return function"), "a boxed capture gets no by-value snapshot IIFE:\n{body}");
}

// A VALUE struct bound per iteration by a `for` and captured. Scalarization
// splits the binding into leaf cells, and every leaf must inherit the BINDING's
// lifetime -- one snapshot per iteration, no box. SROA's alias-unification
// (`Store(p, Load(cursor_dest))` treated as the same object) is correct for a
// reference struct but hands a value binding the loop cursor's own storage,
// which the exhausted path nils: every closure then reads that nil.
const PER_ITER_VALUE_CAPTURE: &str = r#"
value Pt2 { x: float, y: float }

fn main() {
    let pts = [Pt2 { x: 1.0, y: 2.0 }, Pt2 { x: 2.0, y: 3.0 }, Pt2 { x: 3.0, y: 4.0 }]
    let fns: [fn() -> float] = []
    for p in pts { fns.push(() => p.x + p.y) }
    for f in fns { print(f()) }
}
"#;

#[test]
fn per_iteration_value_capture_snapshots_flat_leaves() {
    let lua = emit_module(PER_ITER_VALUE_CAPTURE);
    let body = function_body(&lua, "main");
    assert!(!declares_box(&body), "a per-iteration value capture must not box its leaves:\n{body}");
    assert!(!body.contains(".v = "), "no leaf is stored through a box:\n{body}");
    assert!(
        body.contains("setmetatable({ __slot = __rock_code_slot_ref("),
        "the leaves snapshot into a per-iteration env record:\n{body}"
    );
    assert!(!body.contains("__fn"), "a Rock closure stores a CodeRef, not a frozen body:\n{body}");
    let out = run_luajit(&format!("{lua}\nmain()\n"), "perit");
    assert_eq!(out, "3\n5\n7", "each closure reads ITS iteration's leaves; got:\n{out}");
}

const PER_ITER_ENUM_CAPTURE: &str = r#"
enum InnerChoice { Number(int) }
enum Choice { Pick(InnerChoice), Other(InnerChoice) }

fn capture_choices(choices: [Choice]) -> [fn() -> int] {
    let fns: [fn() -> int] = []
    for choice in choices {
        fns.push(() => match choice {
            Choice::Pick(InnerChoice::Number(n)) => n,
            Choice::Other(InnerChoice::Number(n)) => n,
        })
    }
    return fns
}

fn main() {
    let fns = capture_choices([
        Choice::Pick(InnerChoice::Number(1)),
        Choice::Other(InnerChoice::Number(2)),
        Choice::Pick(InnerChoice::Number(3)),
    ])
    for f in fns { print(f()) }
}
"#;

#[test]
fn per_iteration_enum_capture_snapshots_flat_leaves() {
    let lua = emit_module(PER_ITER_ENUM_CAPTURE);
    let body = function_body(&lua, "capture_choices");
    assert!(!declares_box(&body), "a per-iteration enum capture must not box its leaves:\n{body}");
    assert!(!body.contains(".v = "), "no enum leaf is stored through a box:\n{body}");
    assert!(
        body.contains("setmetatable({ __slot = __rock_code_slot_ref("),
        "the enum leaves snapshot into a per-iteration env record:\n{body}"
    );
    let out = run_luajit(&format!("{lua}\nmain()\n"), "perit-enum");
    assert_eq!(out, "1\n2\n3", "each closure reads its own enum value; got:\n{out}");
}
