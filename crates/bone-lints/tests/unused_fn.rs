//! unused-fn reachability lint over bone HIR. Same harness as unused_var:
//! `semantic_pass(&mut [parse(src)])` -> `Workspace` (with inference).

use bone_lints::{filter_suppressed, unused_fns, UnusedFn};
use bone_semantic::prelude::*;

fn parse(src: &str) -> skull_parser::Ast {
    skull_parser::parse_module(src, Ustr::from("<unused_fn>")).expect("parse")
}

fn findings(src: &str) -> Vec<UnusedFn> {
    let ws = semantic_pass(&mut [parse(src)]).expect("clean program typechecks");
    unused_fns(&ws)
}

fn names(src: &str) -> Vec<String> {
    let mut ns: Vec<String> = findings(src).iter().map(|f| f.name.to_string()).collect();
    ns.sort();
    ns
}

#[test]
fn flags_a_dead_fn() {
    // `dead` has no caller and no root marker -> unreachable.
    let src = "fn main() {\n    used()\n}\nfn used() {}\nfn dead() {}";
    assert_eq!(names(src), vec!["dead".to_string()]);
}

#[test]
fn a_called_fn_is_not_flagged() {
    let src = "fn main() {\n    used()\n}\nfn used() {}";
    assert!(names(src).is_empty(), "used is called from main");
}

#[test]
fn transitively_reachable_is_not_flagged() {
    let src = "fn main() {\n    a()\n}\nfn a() {\n    b()\n}\nfn b() {}";
    assert!(names(src).is_empty(), "b reached through a");
}

#[test]
fn fn_passed_as_value_is_not_flagged() {
    // `cb` is referenced as a value (never directly called) -> a callback.
    // Missing this false-flags every callback in the codebase.
    let src = "fn take(f: fn() -> ()) {}\n\
               fn main() {\n    take(cb)\n}\nfn cb() {}";
    assert!(names(src).is_empty(), "cb is passed as a value, must not flag: got {:?}", names(src));
}

#[test]
fn main_is_a_root() {
    let src = "fn main() {}";
    assert!(names(src).is_empty(), "main is always a root");
}

#[test]
fn test_annotated_fn_is_a_root() {
    let src = "@test\nfn checks_it() {}";
    assert!(names(src).is_empty(), "@test fn is a root");
}

#[test]
fn extern_fn_is_a_root() {
    let src = "extern fn c_thing() -> int";
    assert!(names(src).is_empty(), "extern fn is called by name from FFI");
}

#[test]
fn nomangle_fn_is_a_root() {
    let src = "@nomangle\nfn called_from_lua() {}";
    assert!(names(src).is_empty(), "@nomangle fn is called by original name from inline lua");
}

#[test]
fn allow_comment_above_suppresses() {
    let src = "fn main() {}\n// rock: allow(unused_fn)\nfn dead() {}";
    assert_eq!(names(src), vec!["dead".to_string()], "raw finding still exists");
    let ws = semantic_pass(&mut [parse(src)]).expect("typechecks");
    assert!(
        filter_suppressed(unused_fns(&ws), src).is_empty(),
        "comment above silences the dead fn"
    );
}

#[test]
fn custom_for_loop_iterator_is_not_flagged() {
    // `iter`/`next` are reached ONLY through `for x in (R {..})`, which bone
    // resolves via `for_iter_protocol` -- a separate map from
    // resolved_overload_calls, and the names are not textual at the loop. A
    // reachability walk that misses this false-flags every custom iterator.
    let src = "struct R {\n    lo: int,\n    hi: int,\n}\n\
               struct RIter {\n    cur: int,\n    hi: int,\n}\n\
               fn iter(r: R) -> RIter {\n    return RIter { cur: r.lo, hi: r.hi }\n}\n\
               fn next(it: RIter) -> (bool, int) {\n    \
               if it.cur >= it.hi {\n        return (false, 0)\n    }\n    \
               let v = it.cur\n    it.cur = it.cur + 1\n    return (true, v)\n}\n\
               @test\nfn uses_it() {\n    let t = 0\n    \
               for x in (R { lo: 1, hi: 4 }) {\n        t += x\n    }\n}";
    assert!(
        names(src).is_empty(),
        "custom iterator iter/next reached via for-loop must not flag: got {:?}",
        names(src)
    );
}

#[test]
fn operator_overload_target_is_not_flagged() {
    // `a + b` on a user struct resolves to the `__op_add` overload. The `+` is
    // desugared to a `__op_add` callee before lowering, so it is reached both
    // through the name-scan and through resolved_overload_calls.
    let src = "struct V {\n    x: int,\n}\n\
               fn __op_add(a: V, b: V) -> V {\n    V { x: a.x + b.x }\n}\n\
               fn main() -> V {\n    V { x: 1 } + V { x: 2 }\n}";
    assert!(
        names(src).is_empty(),
        "the + overload is reached via operator resolution: got {:?}",
        names(src)
    );
}
