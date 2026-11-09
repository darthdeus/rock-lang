//! unused-fn reachability lint over bone HIR. Same harness as unused_var:
//! `semantic_pass(&mut [parse(src)])` -> `Workspace` (with inference).

use bone_lints::{UnusedFn, filter_suppressed, unused_fns};
use bone_semantic::prelude::*;

fn parse(src: &str) -> skull_parser::Ast {
    skull_parser::parse_module_clean(src, Ustr::from("<unused_fn>")).expect("parse")
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
fn reachable_fns_and_roots_are_not_flagged() {
    let cases: &[&str] = &[
        // used is called from main
        "fn main() {\n    used()\n}\nfn used() {}",
        // b reached through a
        "fn main() {\n    a()\n}\nfn a() {\n    b()\n}\nfn b() {}",
        // `cb` is referenced as a value (never directly called) -> a callback.
        // Missing this false-flags every callback in the codebase.
        "fn take(f: fn() -> ()) {}\n\
               fn main() {\n    take(cb)\n}\nfn cb() {}",
        // main is always a root
        "fn main() {}",
        // @test fn is a root
        "@test\nfn checks_it() {}",
        // extern fn is called by name from FFI
        "extern(lua) fn c_thing() -> int",
        // @nomangle fn is called by original name from inline lua
        "@nomangle\nfn called_from_lua() {}",
        // A fn referenced only through a GLOBAL initializer (here via a lambda in a
        // struct field) is live -- the global runs at load. bone lowers this to a
        // BodyOwner::Global body, which the walk must treat as a reachability
        // source, not just Function bodies.
        "struct Reg {\n    close: fn() -> (),\n}\n\
               global REGS: [Reg] = [\n    Reg { close: () => shutdown() },\n]\n\
               fn shutdown() {}",
        // `iter`/`next` are reached ONLY through `for x in (R {..})`, which bone
        // resolves via `for_iter_protocol` -- a separate map from
        // resolved_overload_calls, and the names are not textual at the loop. A
        // reachability walk that misses this false-flags every custom iterator.
        "struct R {\n    lo: int,\n    hi: int,\n}\n\
               struct RIter {\n    cur: int,\n    hi: int,\n}\n\
               fn iter(r: R) -> RIter {\n    return RIter { cur: r.lo, hi: r.hi }\n}\n\
               fn next(it: RIter) -> (bool, int) {\n    \
               if it.cur >= it.hi {\n        return (false, 0)\n    }\n    \
               let v = it.cur\n    it.cur = it.cur + 1\n    return (true, v)\n}\n\
               @test\nfn uses_it() {\n    let t = 0\n    \
               for x in (R { lo: 1, hi: 4 }) {\n        t += x\n    }\n}",
        // custom iterator reached through an implicit coercion must not flag
        "struct R {\n    lo: int,\n    hi: int,\n}\n\
               struct RIter {\n    cur: int,\n    hi: int,\n}\n\
               fn iter(r: R) -> RIter {\n    return RIter { cur: r.lo, hi: r.hi }\n}\n\
               fn next(it: RIter) -> (bool, int) {\n    if it.cur >= it.hi {\n        return (false, 0)\n    }\n    let v = it.cur\n    it.cur += 1\n    return (true, v)\n}\n\
               fn sum_it<I: Iterator<int>>(it: I) -> int {\n    let total = 0\n    for v in it { total += v }\n    return total\n}\n\
               @test\nfn uses_it() {\n    (R { lo: 1, hi: 4 }).sum_it()\n}",
        // `a + b` on a user struct resolves to the `__op_add` overload. The `+` is
        // desugared to a `__op_add` callee before lowering, so it is reached both
        // through the name-scan and through resolved_overload_calls.
        "struct V {\n    x: int,\n}\n\
               fn __op_add(a: V, b: V) -> V {\n    V { x: a.x + b.x }\n}\n\
               fn main() -> V {\n    V { x: 1 } + V { x: 2 }\n}",
        // A user `deco`'s machinery invokes the target, so it is entered from
        // outside the call graph -- unlike the implementation annotations above.
        "deco init track {\n}\n\nfn main() {}\n@track\nfn hooked() {}",
        // The parser derives `__op_add`/`__op_lt`/... for every newtype over a
        // primitive. The user never wrote them and cannot delete them, so an
        // unreached one is noise with no available action. Their only HIR mark
        // today is `@pure @inline`, which the split above stopped rooting on.
        "newtype Handle(int)\n\nfn main() {\n    let h = Handle(1)\n}",
    ];
    for src in cases {
        assert!(names(src).is_empty(), "{src}: {:?}", names(src));
    }
}

#[test]
fn allow_comment_above_suppresses() {
    let src = "fn main() {}\n// rock: allow(unused_fn)\nfn dead() {}";
    assert_eq!(
        names(src),
        vec!["dead".to_string()],
        "raw finding still exists"
    );
    let ws = semantic_pass(&mut [parse(src)]).expect("typechecks");
    assert!(
        filter_suppressed(unused_fns(&ws), src).is_empty(),
        "comment above silences the dead fn"
    );
}

#[test]
fn implementation_annotations_are_not_roots() {
    let cases: &[(&str, &str)] = &[
        // `@pure` describes how the fn compiles, not how it is entered. Rooting on
        // "has any annotation" would silently exempt every `@pure`/`@inline` helper.
        (
            "pure_alone_is_not_a_root",
            "fn main() {}\n@pure\nfn dead(x: int) -> int {\n    return x\n}",
        ),
        (
            "inline_alone_is_not_a_root",
            "fn main() {}\n@inline inline(lua) fn dead() = \"\"",
        ),
    ];
    for (name, src) in cases {
        assert_eq!(names(src), ["dead"], "{name}");
    }
}
