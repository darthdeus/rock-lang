use crate::common::{emit_module, function_body, run_luajit, run_main, try_emit_module};

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
fn wide_nobox_vec_stores_each_leaf_in_strided_rows() {
    let lua = emit_module(&program(PUSH_MAIN));
    let main = function_body(&lua, "main");
    assert!(crate::common::has_vec_header(main, "strided", None));
    assert!(!main.contains(r#"["pay.__tag"] = {}"#));
    assert_eq!(
        run_main(&lua),
        "1:o1,2,7,false:bob:hit:true:0:f4:s3:9|\
         2:d5:-::false:0:nf:ns:0.5|\
         3:thi:-::false:0:nf:ns:0.5|\
         4:n:-::false:0:nf:ns:0.5|"
    );
}

#[test]
fn compact_nobox_push_stores_slots_with_one_shared_base() {
    let fields = (0..12)
        .map(|i| format!("f{i}: int"))
        .collect::<Vec<_>>()
        .join(", ");
    let values = (0..12)
        .map(|i| format!("f{i}: {i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let src = format!(
        r#"
nobox value Row {{ {fields} }}
fn emit(xs: [Row], row: Row) {{ xs.push(row) }}
fn main() {{ let xs: [Row] = []; emit(xs, Row {{ {values} }}) }}
"#
    );
    let lua = emit_module(&src);
    let push = function_body(&lua, "emit");
    assert_eq!(push.matches("._data").count(), 1);
    assert_eq!(push.matches(".len").count(), 2);
    assert_eq!(push.matches(" * ").count(), 1);
    let slot_stores: Vec<_> = push.lines().filter(|line| line.contains("] = ")).collect();
    assert!(slot_stores.len() > 8);
    let mut shared_base = None;
    for (offset, line) in slot_stores.iter().enumerate() {
        let (slot, _) = line.trim().split_once("] = ").expect("slot store");
        let (base, physical) = slot.split_once(" + ").unwrap_or_else(|| {
            panic!("each slot must add its offset directly to the shared base: {push}")
        });
        assert_eq!(*shared_base.get_or_insert(base), base);
        assert_eq!(physical, format!("({}))", offset + 1));
    }
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
    let lua = emit_module(&program(RESERVE_MAIN));
    let script = format!(
        r#"
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
"#
    );
    assert_eq!(run_luajit(&script), "reserved");
    let failing = script
        .replace(
            "main()\nend, debug.traceback)",
            "main()\nerror('intentional-cleanup')\nend, debug.traceback)",
        )
        .replace(
            "assert(ok, err)",
            "assert(not ok and err:find('intentional%-cleanup'), err)",
        );
    assert_eq!(run_luajit(&failing), "reserved");
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
    assert!(err.to_string().contains("nobox"));
}

const NARROW: &str = r#"
nobox value Four { a: int, b: int, c: int, d: int }
fn main() {
    let xs: [Four] = []
    xs.push(Four { a: 1, b: 2, c: 3, d: 4 })
    print("{xs[0].d}")
}
"#;

const GENERIC_WIDE: &str = r#"
nobox value Wide<T> { a: int, item: T, c: int, d: int, flag: bool }
value Pair { x: int, y: int }
global STORAGE: [int] = []
fn append<T>(xs: [Wide<T>], item: T) {
    xs.push(Wide { a: 1, item, c: 3, d: 4, flag: false })
}
inline(lua) fn row_width(xs: any) -> int = "return xs['@vec'].repr == 'strided' and xs['@vec'].stride or -1"
fn main() {
    let ints: [Wide<int>] = []
    let pairs: [Wide<Pair>] = []
    let arrays: [Wide<[int]>] = []
    let absent: [Wide<int?>] = []
    append(ints, 7)
    append(pairs, Pair { x: 11, y: 12 })
    append(arrays, [21])
    append(absent, null)
    print("{row_width(ints)} {row_width(pairs)} {row_width(arrays)} {row_width(absent)}")
    arrays[0].item.push(22)
    print("{ints[0].item} {pairs[0].item.x} {pairs[0].item.y} {arrays[0].item.len()} {absent[0].item ?? -1} {ints[0].flag}")
}
"#;

#[test]
fn wide_generic_rows_keep_one_slot_per_type_argument() {
    let lua = emit_module(GENERIC_WIDE);
    assert_eq!(
        run_luajit(&format!(
            "{lua}\nmain()\nlocal row = __ROCK_STRIDED_ROWS.Wide\nassert(row.a == 1 and row.item == 2 and row.flag == 5)\n"
        )),
        "5 5 5 5\n7 11 12 2 -1 false",
    );
}

#[test]
fn a_nobox_element_at_the_threshold_keeps_columns() {
    let lua = emit_module(NARROW);
    let main = function_body(&lua, "main");
    assert!(
        crate::common::has_vec_header(main, "columns", None)
            && !crate::common::has_vec_header(main, "strided", None)
    );
    assert_eq!(run_main(&lua), "4");
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
    let lua = emit_module(NESTED_WRITE);
    assert!(
        crate::common::has_vec_header(&lua, "strided", None)
            && crate::common::has_vec_header(&lua, "columns", None)
    );
    assert_eq!(run_main(&lua), "1,1,2.5,3|6,70,8,11|1,20|50");
}

#[test]
fn row_push_growth_and_iteration_preserve_all_leaves() {
    for width in [1, 2, 8, 12, 13, 24] {
        let fields = (0..width)
            .map(|i| format!("f{i}: int"))
            .collect::<Vec<_>>()
            .join(", ");
        let values = (0..width)
            .map(|i| format!("f{i}: n + {i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let checks = (0..width)
            .map(|i| format!("assert(xs[n].f{i} == n + {i})"))
            .collect::<Vec<_>>()
            .join("\n");
        let src = format!(
            r#"
nobox value Row {{ {fields} }}
fn row(n: int) -> Row {{ return Row {{ {values} }} }}
fn main() {{
    let xs: [Row] = vec_new_with_capacity(1)
    for n in 0..65 {{ xs.push(row(n)) }}
    let visited = 0
    for r in xs {{
        xs.push(row(65 + visited))
        visited += 1
        assert(r.f0 == visited - 1)
    }}
    assert(visited == 65 && xs.len() == 130)
    for n in 0..130 {{ {checks} }}
    xs.clear()
    xs.push(row(7))
    assert(xs.len() == 1 && xs[0].f0 == 7)
    print("ok")
}}
"#
        );
        let lua = emit_module(&src);
        assert_eq!(run_main(&lua), "ok");
    }
}

#[test]
fn strided_row_store_bounds_the_distinct_offset_budget() {
    for width in [12, 13] {
        let fields = (0..width)
            .map(|i| format!("f{i}: int"))
            .collect::<Vec<_>>()
            .join(", ");
        let values = (0..width)
            .map(|i| format!("f{i}: i"))
            .collect::<Vec<_>>()
            .join(", ");
        let src = format!(
            r#"
nobox value Row {{ {fields} }}
fn append(xs: [Row], i: int) {{ xs.push(Row {{ {values} }}) }}
fn main() {{ let xs: [Row] = []; append(xs, 1) }}
"#
        );
        let lua = emit_module(&src);
        let push = function_body(&lua, "append");
        assert_eq!(push.matches(".len").count(), 2);
        if width == 12 {
            assert!(push.contains(" + (12)"));
        } else {
            for offset in 2..=width {
                assert!(!push.contains(&format!(" + ({offset})")));
            }
        }
    }
}
