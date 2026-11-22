use crate::common::{emit_module, function_body, run_luajit};

#[test]
fn a_local_record_read_preserves_loop_bounds_until_publication() {
    let lua = emit_module(
        r#"
@lua_named struct Report { offset: int }
@nomangle fn fill_report(xs: [int], offset: int) -> Report {
    let report = Report { offset }
    for i in 0..xs.len() { xs[i] = i + report.offset }
    return report
}
"#,
    );
    let body = function_body(&lua, "fill_report");
    assert!(!body.contains("__vec_checked_set"));
    assert_eq!(
        run_luajit(&format!(
            r#"{lua}
local xs = {{[0] = 3, 0,0,0}}
local report = fill_report(xs, 7)
assert(report.offset == 7)
assert(xs[1] == 7 and xs[2] == 8 and xs[3] == 9)
print('ok')
"#
        )),
        "ok"
    );
}

// Keyed constructor entries land in a Lua table's hash part, where every slot
// read costs a hash lookup and two trace constants instead of an array load.
#[test]
fn a_record_literal_with_absent_embedded_leaves_fills_every_slot_in_order() {
    let lua = emit_module(
        r#"
struct Spot { x: int, y: int }
struct Nav { embed from: Spot?, embed next: Spot?, decision: int }
struct Holder { name: string, embed nav: Nav?, flag: bool, extra: int? }
fn make_bare(name: string) -> Holder {
    Holder { name, nav: null, flag: true, extra: null }
}
fn make_full(name: string, x: int) -> Holder {
    Holder { name, nav: Nav { from: null, next: Spot { x, y: 2 }, decision: 3 }, flag: false, extra: x }
}
fn describe(h: Holder) -> string {
    "{h.name} {h.nav?.next?.x} {h.nav?.decision} {h.flag} {h.extra}"
}
@nomangle fn holder_report() -> string {
    "{describe(make_bare("a"))}|{describe(make_full("b", 5))}"
}
"#,
    );
    for name in ["make_bare", "make_full"] {
        let body = function_body(&lua, name);
        assert!(!body.contains("[1] ="), "{name} keys its slots:\n{body}");
    }
    assert_eq!(
        run_luajit(&format!(
            r#"{lua}
print(holder_report())
"#
        )),
        "a nil nil true nil|b 5 3 false 5"
    );
}

// A flat enum inside an inline value keeps one slot per payload leaf; a
// named leaf costs a string constant and a hash lookup on every trace.
#[test]
fn a_flat_enum_inside_an_inline_value_reads_its_leaves_by_slot() {
    let lua = emit_module(
        r#"
enum Target { Nobody, Mob(int, int), Spot(int) }
nobox value Lock { target: Target, weight: int }
struct Brain { mode: int, lock: Lock? = null }
fn aim(b: Brain, a: int, c: int) { b.lock = Lock { target: Target::Mob(a, c), weight: a + c } }
fn describe(b: Brain) -> string {
    if let lock = b.lock {
        return match lock.target {
            Target::Nobody => "nobody {lock.weight}",
            Target::Mob(a, c) => "mob {a} {c} {lock.weight}",
            Target::Spot(s) => "spot {s} {lock.weight}",
        }
    }
    return "none"
}
@nomangle fn brain_report() -> string {
    let b = Brain { mode: 1 }
    let before = describe(b)
    aim(b, 4, 5)
    let after = describe(b)
    b.lock = null
    "{before}|{after}|{describe(b)}"
}
"#,
    );
    for name in ["aim", "describe"] {
        let body = function_body(&lua, name);
        assert!(
            !body.contains("lock.target"),
            "{name} names a leaf:\n{body}"
        );
    }
    assert_eq!(
        run_luajit(&format!("{lua}\nprint(brain_report())\n")),
        "none|mob 4 5 9|none"
    );
}
