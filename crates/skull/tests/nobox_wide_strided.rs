//! A `[T]` of a WIDE `nobox` value struct (more leaves than
//! `MAX_COLUMN_LEAVES`) stores its leaves row-major in one `_data` table, the
//! same strided layout wide flat enums use, instead of one column table per
//! leaf.
//!
//! Why: every column is its own table, so a push or a whole-element read of a
//! 50-leaf element resolves 50 string keys and bounds-checks 50 arrays. On a
//! LuaJIT trace that costs about three IR constants and ten instructions per
//! leaf, and a hot caller's root trace hits the 500-constant limit ("trace too
//! long") inside the push (`docs/mag-presentation-boundary-plan.md`, slice 4).
//! Row-major storage touches one table, and the push walks it with a running
//! index, so a leaf costs no new constant. `docs/nobox-wide-trace-plan.md`
//! carries the measurements.
//!
//! Unlike the numeric flat-enum tier, these leaves may be bools, strings,
//! closures and nil (an inactive enum payload slot, an absent nullable), so the
//! backing stores every leaf RAW: `false` stays `false` and nil stays nil.

use crate::common::{function_body, run_luajit, try_emit_module};

const EVENT: &str = r#"
nobox value P2 { x: float, y: float }
nobox value Id { value: int }

enum Pay {
    Nothing,
    Dmg(float),
    Door(P2, int, bool),
    Text(string),
}

enum Who { Nobody, Named(string) }

nobox value Ev {
    kind: int,
    pay: Pay,
    who: Who,
    label: string = "",
    lethal: bool = false,
    at: P2 = P2 { x: 0.0, y: 0.0 },
    follow: (() -> float)? = null,
    space: Id? = null,
    t: float,
}

struct World { evs: [Ev] }

fn emit(w: World, e: Ev) {
    w.evs.push(e)
}

fn pay_score(p: Pay) -> string {
    return match p {
        Pay::Nothing => "n",
        Pay::Dmg(v) => "d{v}",
        Pay::Door(at, id, keyed) => "o{at.x},{at.y},{id},{keyed}",
        Pay::Text(s) => "t{s}",
    }
}

fn who_score(w: Who) -> string {
    return match w {
        Who::Nobody => "-",
        Who::Named(s) => s,
    }
}

fn describe(e: Ev) -> string {
    let follow = "nf"
    if let f = e.follow { follow = "f{f()}" }
    let space = "ns"
    if let s = e.space { space = "s{s.value}" }
    return "{e.kind}:{pay_score(e.pay)}:{who_score(e.who)}:{e.label}:{e.lethal}:{e.at.x}:{follow}:{space}:{e.t}"
}

fn ev(kind: int, pay: Pay) -> Ev {
    return Ev { kind, pay, who: Who::Nobody, t: 0.5 }
}
"#;

fn program(main: &str) -> String {
    format!("{EVENT}\n{main}")
}

const PUSH_MAIN: &str = r#"
fn main() {
    let w = World { evs: [] }
    emit(w, Ev { kind: 1, pay: Pay::Door(P2 { x: 1.0, y: 2.0 }, 7, false), who: Who::Named("bob"),
                 label: "hit", lethal: true, follow: () => 4.0, space: Id { value: 3 }, t: 9.0 })
    emit(w, ev(2, Pay::Dmg(5.0)))
    emit(w, ev(3, Pay::Text("hi")))
    emit(w, ev(4, Pay::Nothing))
    let out = ""
    for e in w.evs {
        out = out + describe(e) + "|"
    }
    print(out)
}
"#;

#[test]
fn wide_nobox_vec_declares_strided_storage() {
    let lua = try_emit_module(&program(PUSH_MAIN))
        .expect("a wide nobox element must stay flat in a Vec");
    let main = function_body(&lua, "main");
    assert!(
        main.contains(r#"repr = "strided""#),
        "a wide nobox Vec must use the row-major strided backing:\n{main}"
    );
    assert!(
        !main.contains(r#"["pay.__tag"] = {}"#),
        "a wide nobox Vec must not build one table per leaf:\n{main}"
    );
}

#[test]
fn wide_nobox_push_writes_one_backing_with_a_running_index() {
    let lua = try_emit_module(&program(PUSH_MAIN)).expect("emits");
    let push = function_body(&lua, "emit");
    assert_eq!(
        push.matches("._data").count(),
        1,
        "the push must load the one backing table once:\n{push}"
    );
    assert!(
        !push.contains(r#"["pay.__tag"]"#) && !push.contains(".lethal"),
        "the push must not address per-leaf column tables:\n{push}"
    );
    // A leaf's slot is the previous leaf's slot plus one, so no leaf past the
    // first spends a distinct numeric constant on its offset.
    for offset in 2..=14 {
        assert!(
            !push.contains(&format!("+ ({offset})")),
            "leaf offset {offset} is spelled as its own constant:\n{push}"
        );
    }
}

#[test]
fn wide_nobox_elements_round_trip_every_leaf_kind() {
    let lua = try_emit_module(&program(PUSH_MAIN)).expect("emits");
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "nobox_wide_strided_round_trip"),
        "1:o1,2,7,false:bob:hit:true:0:f4:s3:9|\
         2:d5:-::false:0:nf:ns:0.5|\
         3:thi:-::false:0:nf:ns:0.5|\
         4:n:-::false:0:nf:ns:0.5|"
    );
}

const SURFACE_MAIN: &str = r#"
fn kinds(xs: [Ev]) -> string {
    let out = ""
    for e in xs { out = out + "{e.kind}" }
    return out
}

fn main() {
    let xs: [Ev] = [ev(1, Pay::Dmg(1.0)), ev(2, Pay::Text("b")), ev(3, Pay::Nothing)]
    xs[1] = ev(5, Pay::Door(P2 { x: 3.0, y: 4.0 }, 1, true))
    xs[0].lethal = true
    xs[0].label = "set"
    xs.insert_at(1, ev(9, Pay::Text("ins")))
    let removed = xs.remove_at(2).unwrap()
    let swapped = xs.swap_remove(0).unwrap()
    xs.push(ev(6, Pay::Nothing))
    let popped = xs.pop().unwrap()
    let copy = xs.clone()
    copy.push(ev(7, Pay::Dmg(2.0)))
    let spread: [Ev] = [...copy, ev(8, Pay::Nothing)]
    copy.sort((e: Ev) => -e.kind)
    let same = xs == xs.clone()
    let differ = xs == copy
    xs.clear()
    xs.push(ev(4, Pay::Text("after")))
    print("{describe(removed)}|{describe(swapped)}|{popped.kind}|{kinds(copy)}|{kinds(spread)}|{same},{differ}|{describe(xs[0])}|{xs.len()}")
}
"#;

#[test]
fn wide_nobox_strided_vec_keeps_the_mutating_surface() {
    let lua = try_emit_module(&program(SURFACE_MAIN)).expect("emits");
    assert!(lua.contains(r#"repr = "strided""#), "fixture must exercise strided storage");
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "nobox_wide_strided_surface"),
        "5:o3,4,1,true:-::false:0:nf:ns:0.5|\
         1:d1:-:set:true:0:nf:ns:0.5|\
         6|973|3978|true,false|\
         4:tafter:-::false:0:nf:ns:0.5|1"
    );
}

const RELEASE_MAIN: &str = r#"
nobox value Holder { items: [int]?, a: int, b: int, c: int, d: int }

inline(lua) fn watch(items: [int]) = """
_G.__wide_strided_weak = setmetatable({items}, {__mode = 'v'})
"""

inline(lua) fn released() -> bool = """
collectgarbage('collect')
collectgarbage('collect')
return _G.__wide_strided_weak[1] == nil
"""

fn seed(values: [Holder]) {
    let items = [7]
    watch(items)
    values.push(Holder { items, a: 1, b: 2, c: 3, d: 4 })
}

fn discard(values: [Holder]) { let _ = values.pop() }

fn main() {
    let values: [Holder] = []
    seed(values)
    let held = !released()
    discard(values)
    let after_pop = released()
    seed(values)
    values.clear()
    let after_clear = released()
    print("{held},{after_pop},{after_clear}")
}
"#;

#[test]
fn wide_nobox_strided_vec_releases_removed_references() {
    let lua = try_emit_module(RELEASE_MAIN).expect("emits");
    assert!(lua.contains(r#"repr = "strided""#), "fixture must exercise strided storage");
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "nobox_wide_strided_release"),
        "true,true,true"
    );
}

#[test]
fn inspecting_a_wide_nobox_vec_still_rejects_implicit_materialization() {
    let src = program(
        r#"
fn main() {
    let xs: [Ev] = [ev(1, Pay::Nothing)]
    print(inspect(xs))
}
"#,
    );
    let err = try_emit_module(&src).expect_err("inspect would materialize every element");
    assert!(
        err.to_string().contains("nobox"),
        "the rejection must name the nobox guarantee: {err}"
    );
}

const NARROW: &str = r#"
nobox value Four { a: int, b: int, c: int, d: int }
fn main() {
    let xs: [Four] = []
    xs.push(Four { a: 1, b: 2, c: 3, d: 4 })
    print("{xs[0].d}")
}
"#;

#[test]
fn a_nobox_element_at_the_threshold_keeps_columns() {
    let lua = try_emit_module(NARROW).expect("emits");
    let main = function_body(&lua, "main");
    assert!(
        main.contains(r#"repr = "columns""#) && !main.contains(r#"repr = "strided""#),
        "a four-leaf nobox element stays in columns:\n{main}"
    );
    assert_eq!(run_luajit(&format!("{lua}\nmain()\n"), "nobox_narrow_columns"), "4");
}
