use crate::common::{emit_module, function_body, run_main};

fn declares_box(body: &str) -> bool {
    body.lines().any(|l| {
        let t = l.trim();
        let t = t.strip_prefix("local ").unwrap_or(t);
        t.strip_prefix(['c', 'r'])
            .and_then(|r| r.split_once(" = "))
            .is_some_and(|(n, rhs)| {
                !n.is_empty()
                    && n.bytes().all(|b| b.is_ascii_digit())
                    && (rhs == "{}" || (rhs.starts_with("{v = ") && rhs.ends_with('}')))
            })
    })
}

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
    assert!(body.contains("setmetatable({ __slot = __rock_code_slot_ref("));
    assert!(!body.contains("__fn"));
    assert!(!body.contains("function("));
    assert!(!body.contains("return function"));
    assert!(!declares_box(body));
}

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
    assert!(body.contains("setmetatable({ __slot = __rock_code_slot_ref("));
    assert!(!body.contains("__fn"));
    assert!(!body.contains("return function"));
    let out = run_main(&lua);
    assert_eq!(out, "1\n2\n3");
}

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
    assert!(declares_box(body));
    assert!(body.contains(".v = "));
    assert!(body.contains("setmetatable({ __slot = __rock_code_slot_ref("));
    assert!(!body.contains("__fn"));
    assert!(!body.contains("return function"));
}

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
    assert!(!declares_box(body));
    assert!(!body.contains(".v = "));
    assert!(body.contains("setmetatable({ __slot = __rock_code_slot_ref("));
    assert!(!body.contains("__fn"));
    let out = run_main(&lua);
    assert_eq!(out, "3\n5\n7");
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
    assert!(!declares_box(body));
    assert!(!body.contains(".v = "));
    assert!(body.contains("setmetatable({ __slot = __rock_code_slot_ref("));
    let out = run_main(&lua);
    assert_eq!(out, "1\n2\n3");
}

#[test]
fn lambda_slot_keys_do_not_spell_the_source_path() {
    let src = r#"
global KEEP: fn() -> int = () => 0
fn make(x: int) -> fn() -> int {
    fn helper() -> int { return 2 }
    return () => x + helper()
}
fn main() { KEEP = make(1); print(KEEP()) }
"#;
    let lua = crate::common::try_emit_module_with_file(
        src,
        "/checkout/game/main.rock",
        Default::default(),
    )
    .expect("emit");
    let keys: Vec<&str> = lua
        .split("__rock_code_slot_ref(\"")
        .skip(1)
        .filter_map(|rest| rest.split('"').next())
        .filter(|key| key.starts_with("lambda:") || key.starts_with("nested:"))
        .collect();
    assert!(!keys.is_empty());
    for key in keys {
        let site = key.rsplit_once('@').expect("site").1;
        assert!(!site.contains("/checkout/"));
    }
}
