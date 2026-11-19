use crate::common::{
    body_id, emit_module, emit_module_without_frame_limits, function_body, luajit_loads, run_main,
    try_emit_module, ws_with_stdlib,
};

#[test]
fn retained_optional_loader_results_fit_the_local_budget() {
    let fields = (0..8)
        .map(|i| format!("item{i}: Item"))
        .collect::<Vec<_>>()
        .join(", ");
    let scalars = (0..20)
        .map(|i| format!("let s{i} = scalar(seed, {i})\n"))
        .collect::<String>();
    let loads = (0..8)
        .map(|i| format!("let item{i} = load(seed + {i}) ?? return null\n"))
        .collect::<String>();
    let sum = (0..20)
        .map(|i| format!("s{i}"))
        .collect::<Vec<_>>()
        .join(" + ");
    let items = (0..8)
        .map(|i| format!("item{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let source = format!(
        r#"
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
"#
    );
    let lua = emit_module(&source);
    assert_eq!(run_main(&lua), "210");
}

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
    let lua = emit_module(&fixture(50, 3));
    assert_eq!(run_main(&lua), "159");
}

#[test]
fn a_wide_multi_return_lands_in_the_locals_that_hold_the_value() {
    let lua = emit_module(&fixture(50, 2));
    let run = function_body(&lua, "run");
    let callee = crate::common::bound_callee_name(&lua, "run", "mk_float");
    let receive = run
        .lines()
        .position(|line| line.contains(&format!("= {callee}(")))
        .unwrap_or_else(|| panic!("no receive of mk in:\n{run}"));
    let next = run.lines().nth(receive + 1).unwrap_or("");
    assert!(!(next.trim_start().starts_with('r') && next.contains("= c")));
}

#[test]
fn four_live_fifty_leaf_values_are_a_spanned_compile_error() {
    let err = try_emit_module(&fixture(50, 4))
        .expect_err("four live 50-leaf values must not emit: LuaJIT cannot load the result");
    let message = &err.message;
    assert!(message.contains("`@/<test>::run`"));
    assert!(message.contains("LuaJIT"));
    assert!(message.contains("LJ_MAX_LOCVAR") || message.contains("LJ_MAX_SLOTS"));
    assert!(message.contains("nobox"));
    assert!(message.contains("cannot be boxed"));
    let label = err.labels.first().expect("a spanned label");
    assert_eq!(label.span.file.as_str(), "<test>");
    assert_eq!(err.location.file.as_str(), "<test>");
    let lua = emit_module_without_frame_limits(&fixture(50, 4));
    let load = luajit_loads(&lua).expect_err("must not load");
    assert!(load.contains("more than 200 local variables") || load.contains("too complex"));
}

fn verdicts_agree(tag: &str, src: &str) -> bool {
    let compiled = try_emit_module(src);
    let lua = emit_module_without_frame_limits(src);
    let load = luajit_loads(&lua);
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

fn walk(name: &str, lo: usize, hi: usize, mk: impl Fn(usize) -> String) {
    let verdicts: Vec<bool> = (lo..=hi)
        .map(|w| verdicts_agree(&format!("{name}_{w}"), &mk(w)))
        .collect();
    assert!(verdicts.iter().any(|ok| *ok));
    assert!(verdicts.iter().any(|ok| !*ok));
    let flip = verdicts
        .iter()
        .position(|ok| !*ok)
        .expect("a rejected member");
    assert!(verdicts[flip..].iter().all(|ok| !*ok));
}

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

const MAIN: &str = "fn main() { print(\"{run(1.0)}\") }\n";

fn padded(indent: &str, arg: &str, pad: usize, middle: &str) -> String {
    let mut body = String::new();
    for i in 0..pad {
        body.push_str(&format!("{indent}let a{i} = one({arg}, {i})\n"));
    }
    body.push_str(middle);
    for i in 0..pad {
        body.push_str(&format!("{indent}total += a{i}\n"));
    }
    body.push_str(&format!("{indent}return total\n"));
    body
}

fn padded_run(pad: usize, middle: &str) -> String {
    format!(
        "{}fn run(x: float) -> float {{\n{}}}\n{MAIN}",
        wide_decls(50),
        padded("    ", "x", pad, middle)
    )
}

fn padded_receive(pad: usize) -> String {
    padded_run(pad, "    let v = mk(x)\n    let total = use_w(v, 0)\n")
}

fn padded_receive_in_loop(pad: usize) -> String {
    padded_run(
        pad,
        "    let total = 0.0\n    for i in 0..3 {\n        let v = mk(x + float(i))\n        \
         total += use_w(v, i)\n    }\n",
    )
}

fn nested_wide_call(pad: usize) -> String {
    padded_run(pad, "    let total = use_w(mk(x), 0)\n")
}

fn wide_in_closure(pad: usize) -> String {
    format!(
        "{}fn run(x: float) -> float {{\n    let f = (y: float) => {{\n{}    }}\n    return f(x)\n}}\n{MAIN}",
        wide_decls(50),
        padded(
            "        ",
            "y",
            pad,
            "        let v = mk(y)\n        let total = use_w(v, 0)\n"
        )
    )
}

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

#[test]
fn frame_limit_verdicts_agree_with_luajit_across_each_boundary() {
    type Fixture = fn(usize) -> String;
    let cases: [(&str, usize, usize, Fixture); 6] = [
        ("padded_receive", 144, 147, padded_receive),
        ("in_loop", 136, 139, padded_receive_in_loop),
        ("nested", 144, 147, nested_wide_call),
        ("closure", 144, 147, wide_in_closure),
        ("flat_locals", 198, 201, flat_locals),
        ("held_values", 3, 4, |held| fixture(50, held)),
    ];
    for (name, lo, hi, mk) in cases {
        walk(name, lo, hi, mk);
    }
}

#[test]
fn the_frame_limit_rejection_reaches_the_incremental_path() {
    let fits = padded_receive(100);
    let over = padded_receive(160);
    let ws1 = ws_with_stdlib(&fits);
    let mid = skull::user_module_id(&ws1).expect("user module");
    let (_full, mut cache) = skull::mir_emit::emit_full(&ws1, mid).expect("emit_full fits");

    let ws2 = ws_with_stdlib(&over);
    let run = body_id(&ws2, "run");
    let err = skull::mir_emit::emit_incremental(&ws2, &mut cache, &[run])
        .err()
        .expect("the widened body must fail the incremental re-emit");
    assert!(err.message.contains("too wide for LuaJIT"));
}
