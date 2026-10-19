//! A `[T]` of a WIDE `nobox` value struct (more leaves than
//! `MAX_COLUMN_LEAVES`) stores its leaves row-major in one `_data` table, the
//! same strided layout wide flat enums use, instead of one column table per
//! leaf.
//!
//! Why: every column is its own table, so a push or a whole-element read of a
//! 50-leaf element resolves 50 string keys and bounds-checks 50 arrays. On a
//! LuaJIT trace that costs about three IR constants and ten instructions per
//! leaf, and a hot caller's root trace hits the 500-constant limit ("trace too
//! long") inside the push.
//! Row-major storage touches one table, and the push walks it with a running
//! index, so a leaf costs no new constant.
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

const RESERVE_MAIN: &str = r#"
inline(lua) fn begin_reserve_watch() = "_G.__stride_reservations = {}"
inline(lua) fn check_reserve(xs: [Ev], count: int) = """
local calls = _G.__stride_reservations
assert(#calls == count, 'expected one backing reservation, got ' .. #calls)
if count > 0 then
    local h = rawget(xs, '@vec')
    assert(calls[1].size == h.cap * h.stride, 'reservation must cover declared leaves')
    assert(calls[1].data == xs._data, 'appends must use the reserved backing')
end
"""
inline(lua) fn remember_backing(xs: [Ev]) = "_G.__stride_backing = xs._data"
inline(lua) fn check_backing(xs: [Ev]) = "assert(xs._data == _G.__stride_backing)"

fn input(n: int) -> Ev {
    return Ev { kind: n, pay: Pay::Text("row{n}"), who: Who::Named("who{n}"),
                label: "label{n}", lethal: false, at: P2 { x: n.to_float(), y: -n.to_float() },
                follow: () => n.to_float(),
                space: Id { value: n }, t: 0.5 }
}

fn check_rows(xs: [Ev]) {
    for i in 0..xs.len() {
        let expected = input(i)
        assert(describe(xs[i]) == describe(expected))
        assert(xs[i].at.y == expected.at.y)
    }
}

fn main() {
    let xs: [Ev] = vec_new_with_capacity(64)
    assert(xs.len() == 0 && xs.capacity() == 64)
    begin_reserve_watch()
    xs.push(input(0))
    check_reserve(xs, 1)
    xs.push(input(1))
    check_rows(xs)
    let partial = xs.clone()
    begin_reserve_watch()
    for i in 2..64 { partial.push(input(i)) }
    check_reserve(partial, 1)
    check_rows(partial)
    assert(partial.len() == 64 && partial.capacity() == 64)
    remember_backing(partial)
    partial.clear()
    assert(partial.len() == 0 && partial.capacity() == 64)
    begin_reserve_watch()
    for i in 0..64 { partial.push(input(i)) }
    check_reserve(partial, 0)
    check_backing(partial)
    check_rows(partial)
    partial.push(input(64))
    assert(partial.len() == 65 && partial.capacity() >= partial.len())
    check_reserve(partial, 0)
    check_rows(partial)
    let zero: [Ev] = vec_new_with_capacity(0)
    begin_reserve_watch()
    for i in 0..65 { zero.push(input(i)) }
    check_reserve(zero, 0)
    assert(zero.len() == 65 && zero.capacity() >= zero.len())
    check_rows(zero)
    print("reserved")
}
"#;

#[test]
fn strided_first_fill_reserves_once_and_refill_reuses_backing() {
    let lua = try_emit_module(&program(RESERVE_MAIN)).expect("emits");
    let script = format!(r#"
local original = require('table.new')
package.loaded['table.new'] = function(narr, nhash)
    local data = original(narr, nhash)
    if _G.__stride_reservations then
        table.insert(_G.__stride_reservations, {{size = narr, data = data}})
    end
    return data
end
local ok, err = xpcall(function()
{lua}
main()
end, debug.traceback)
package.loaded['table.new'] = original
_G.__stride_reservations, _G.__stride_backing = nil, nil
assert(require('table.new') == original)
assert(ok, err)
"#);
    assert_eq!(run_luajit(&script, "strided_reservation"), "reserved");
    let failing = script
        .replace("main()\nend, debug.traceback)", "main()\nerror('intentional-cleanup')\nend, debug.traceback)")
        .replace("assert(ok, err)", "assert(not ok and err:find('intentional%-cleanup'), err)");
    assert_eq!(run_luajit(&failing, "strided_reservation_cleanup"), "reserved");
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
    let values: [Holder] = vec_new_with_capacity(4)
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

const NESTED_WRITE: &str = r#"
nobox value Q2 { x: float, y: float }
nobox value Wide { kind: int, at: Q2, a: int, b: int, c: int }
nobox value Narrow { kind: int, at: Q2 }
struct Obj { hp: int }
nobox value Holds { o: Obj, a: int, b: int, c: int, d: int }

fn main() {
    let ws: [Wide] = []
    ws.push(Wide { kind: 1, at: Q2 { x: 1.0, y: 2.0 }, a: 3, b: 4, c: 5 })
    ws.push(Wide { kind: 6, at: Q2 { x: 7.0, y: 8.0 }, a: 9, b: 10, c: 11 })
    ws[1].at.x = 70.0
    ws[0].at.y += 0.5
    let ns: [Narrow] = []
    ns.push(Narrow { kind: 1, at: Q2 { x: 1.0, y: 2.0 } })
    ns[0].at.y = 20.0
    let hs: [Holds] = []
    hs.push(Holds { o: Obj { hp: 1 }, a: 2, b: 3, c: 4, d: 5 })
    hs[0].o.hp = 50
    let w0 = ws[0]
    let w1 = ws[1]
    print("{w0.kind},{w0.at.x},{w0.at.y},{w0.a}|{w1.kind},{w1.at.x},{w1.at.y},{w1.c}|{ns[0].at.x},{ns[0].at.y}|{hs[0].o.hp}")
}
"#;

#[test]
fn a_nested_leaf_of_a_flat_stored_element_is_one_slot_write() {
    let lua = try_emit_module(NESTED_WRITE).expect("emits");
    assert!(
        lua.contains(r#"repr = "strided""#) && lua.contains(r#"repr = "columns""#),
        "fixture must exercise both flat tiers"
    );
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "nobox_nested_leaf_write"),
        "1,1,2.5,3|6,70,8,11|1,20|50"
    );
}

const EMPTY_EDGES_MAIN: &str = r#"
fn edges_strided() -> string {
    let xs: [Ev] = []
    let a = xs.pop() == null
    xs.push(ev(1, Pay::Nothing))
    let b = xs.pop().unwrap().kind
    let c = xs.pop() == null
    let d = xs.remove_at(0) == null
    let e = xs.swap_remove(0) == null
    xs.push(ev(2, Pay::Text("again")))
    return "{a},{b},{c},{d},{e},{xs.len()},{xs[0].kind}"
}

fn edges_boxed() -> string {
    let xs: [int] = []
    let a = xs.pop() == null
    xs.push(1)
    let b = xs.pop().unwrap()
    let c = xs.pop() == null
    let d = xs.remove_at(0) == null
    let e = xs.swap_remove(0) == null
    xs.push(2)
    return "{a},{b},{c},{d},{e},{xs.len()},{xs[0]}"
}

fn main() {
    print("{edges_strided()}|{edges_boxed()}")
}
"#;

#[test]
fn an_emptied_strided_vec_answers_like_a_boxed_one() {
    let lua = try_emit_module(&program(EMPTY_EDGES_MAIN)).expect("emits");
    assert!(lua.contains(r#"repr = "strided""#), "fixture must exercise strided storage");
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "nobox_wide_strided_empty_edges"),
        "true,1,true,true,true,1,2|true,1,true,true,true,1,2"
    );
}
