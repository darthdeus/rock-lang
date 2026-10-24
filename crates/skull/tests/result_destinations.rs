use crate::common::{emit_module, run_luajit};

#[test]
fn destination_returns_keep_effect_order_and_defer_snapshots() {
    let lua = emit_module(r#"
struct Trace { n: int }
nobox value Pair { x: int, y: int }
fn tick(t: Trace, n: int) -> int { t.n = t.n * 10 + n; n }
fn pick(t: Trace, flag: bool) -> (int, int) {
    return if flag {
        let first = tick(t, 1)
        (first, tick(t, 2))
    } else {
        if t.n == 0 { return (tick(t, 3), tick(t, 4)) }
        (tick(t, 5), tick(t, 6))
    }
}
fn cleanup(t: Trace, flag: bool) -> int {
    defer { t.n = t.n * 10 + 9 }
    if flag { t.n } else { 7 }
}
fn inner_cleanup(t: Trace) -> int {
    return { defer { t.n = t.n * 10 + 8 }; t.n }
}
fn maybe(flag: bool) -> int? { if flag { null } else { 3 } }
fn truth(flag: bool) -> bool { if flag { false } else { true } }
fn tuple_option(flag: bool) -> (int, Pair)? {
    if flag { (7, Pair { x: 8, y: 9 }) } else { null }
}
fn main() {
    let t = Trace { n: 0 }
    let (a, b) = pick(t, true)
    print(a); print(b); print(t.n)
    t.n = 0
    let (c, d) = pick(t, false)
    print(c); print(d); print(t.n)
    print(cleanup(t, true)); print(t.n)
    print(inner_cleanup(t)); print(t.n)
    print(maybe(true) == null); print(maybe(false) ?? 0)
    print(truth(true)); print(truth(false))
    print(tuple_option(false) == null)
    if let value = tuple_option(true) {
        print(value.0); print(value.1.x); print(value.1.y)
    }
}
"#);
    assert_eq!(run_luajit(&format!("{lua}\nmain()\n"), "result_destinations"), "1\n2\n12\n3\n4\n34\n34\n349\n349\n3498\ntrue\n3\nfalse\ntrue\ntrue\n7\n8\n9");
}
