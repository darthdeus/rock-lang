use crate::common::{
    emit_module, function_body, run_main, try_emit_module, try_emit_with_stdlib_module,
};

const NOBOX_NULLABLE_FIELD: &str = r#"
nobox value NV2 { x: float, y: float }
struct Txt { pos: NV2?, t: float }

fn spawn(p: NV2?) -> Txt { return Txt { pos: p, t: 1.0 } }
fn spawn_literal() -> Txt { return Txt { pos: NV2 { x: 9.0, y: 8.0 }, t: 2.0 } }

fn read(c: Txt) -> float {
    if c.pos != null { return c.pos.x }
    return -1.0
}

fn clear(c: Txt) -> float {
    c.pos = null
    return read(c)
}
fn set(c: Txt, v: NV2) -> float {
    c.pos = v
    return read(c)
}

fn main() {
    let a = spawn(NV2 { x: 3.0, y: 4.0 })
    let b = spawn(null)
    print(read(a))
    print(read(b))
    print(read(spawn_literal()))
    print(clear(spawn(NV2 { x: 5.0, y: 6.0 })))
    print(set(spawn(null), NV2 { x: 7.0, y: 0.0 }))
}
"#;

#[test]
fn a_nullable_nobox_field_is_stored_as_presence_and_leaves() {
    let lua = emit_module(NOBOX_NULLABLE_FIELD);
    for name in ["spawn", "spawn_literal", "read", "clear", "set"] {
        let body = function_body(&lua, name);
        assert!(
            !body.contains("NV2_ct("),
            "a nullable nobox field materialized in `{name}`:\n{body}"
        );
    }
    let body = function_body(&lua, "spawn");
    assert!(
        body.contains("setmetatable({c1, c2, c0,"),
        "the nullable field did not flatten into its parent's leaves:\n{body}"
    );
    assert_eq!(run_main(&lua), "3\n-1\n9\n-1\n7");
}

const NOBOX_COLUMN_WIDEN: &str = r#"
nobox value NV2 { x: float, y: float }

fn via_get(xs: [NV2]) -> NV2? { return xs.get(0) }
fn via_pop(xs: [NV2]) -> NV2? { return xs.pop() }

fn main() {
    let xs = [NV2 { x: 1.0, y: 2.0 }]
    let a = via_get(xs)
    let b = via_pop(xs)
    print("{a?.x ?? -1.0} {b?.y ?? -1.0}")
}
"#;

#[test]
fn a_column_read_widens_to_nullable_without_materializing() {
    let lua = emit_module(NOBOX_COLUMN_WIDEN);
    for name in ["via_get", "via_pop"] {
        let body = function_body(&lua, name);
        assert!(
            !body.contains("NV2_ct("),
            "{name} materialized a no-box value:\n{body}"
        );
    }
    assert_eq!(run_main(&lua), "1 2");
}

#[test]
fn nobox_escape_sinks_reject_with_specific_diagnostics() {
    for (name, declaration, body, main, fragments, argument) in [
        (
            "inline Lua sink",
            r#"@nomangle inline(lua) fn foreign_sink(v: NV2) = "return""#,
            "fn bad(v: NV2) {\n    foreign_sink(v)\n}",
            "bad(NV2 { x: 1.0, y: 2.0 })",
            ["nobox", "foreign_sink"],
            None,
        ),
        (
            "tuple interpolation",
            "",
            "fn bad(v: NV2) -> int {\n    let t = (v, 1.0)\n    print(\"{t}\")\n    return 0\n}",
            "print(bad(NV2 { x: 1.0, y: 2.0 }))",
            ["nobox", "NV2"],
            None,
        ),
        (
            "column inspect",
            "",
            "fn bad() {\n    let xs: [NV2] = [NV2 { x: 1.0, y: 2.0 }]\n    let _ = inspect(xs)\n}",
            "bad()",
            ["nobox", "inspect"],
            None,
        ),
        (
            "Lua-array bridge",
            "",
            "fn bad() {\n    let xs: [NV2] = [NV2 { x: 1.0, y: 2.0 }]\n    let _ = __rock_vec_to_lua_array(xs)\n}",
            "bad()",
            ["nobox", "Lua array"],
            None,
        ),
        (
            "any sink",
            "fn erase(v: any) {}",
            "fn bad(v: NV2) {\n    erase(v)\n}",
            "",
            ["nobox", "erase"],
            Some("v"),
        ),
        (
            "unspecialized generic sink",
            "extern(lua) fn generic_sink<T>(v: T)",
            "fn bad(v: NV2) {\n    generic_sink(v)\n}",
            "",
            ["nobox", "generic_sink"],
            None,
        ),
        (
            "extern sink",
            "extern(lua) fn foreign(v: NV2)",
            "fn bad(v: NV2) {\n    foreign(v)\n}",
            "",
            ["nobox", "foreign"],
            None,
        ),
    ] {
        let source = format!(
            "nobox value NV2 {{ x: float, y: float }}\n{declaration}\n{body}\nfn main() {{\n{main}\n}}\n"
        );
        let err = try_emit_module(&source).expect_err(name);
        for fragment in fragments {
            assert!(
                err.message.contains(fragment),
                "{name}: expected diagnostic fragment `{fragment}`:\n{err:?}"
            );
        }
        if let Some(argument) = argument {
            let span = err.labels.first().expect("sink error has a label").span;
            let range = span.offset_range();
            assert_eq!(&source[range.0..range.1], argument, "{name}");
        }
    }
}

const NOBOX_GLOBAL: &str = r#"
nobox value NV2 { x: float, y: float }

global SPOT: NV2 = NV2 { x: 1.0, y: 2.0 }

fn read() -> float {
    return SPOT.x + SPOT.y
}

fn main() {
    print("{read()}")
    SPOT.x = 10.0
    print("{read()}")
    SPOT = NV2 { x: 100.0, y: 200.0 }
    print("{read()}")
}
"#;

#[test]
fn global_of_a_nobox_value_stores_leaves() {
    let lua = emit_module(NOBOX_GLOBAL);
    for f in ["read", "main"] {
        let body = function_body(&lua, f);
        assert!(
            !body.contains("NV2_ct("),
            "`{f}` materialized the nobox global:\n{body}"
        );
    }
    assert_eq!(run_main(&lua), "3\n12\n300");
}

const NOBOX_STDLIB_SINK_LIB: &str = r#"
nobox value SV2 { x: float, y: float }

fn stdlib_bad(v: SV2) -> int {
    let t = (v, 1.0)
    print("{t}")
    return 0
}

fn stdlib_entry() {
    print(stdlib_bad(SV2 { x: 1.0, y: 2.0 }))
}
"#;

const NOBOX_STDLIB_SINK_USER: &str = r#"
fn main() {
    stdlib_entry()
}
"#;

#[test]
fn a_stdlib_body_does_not_get_leniency_for_a_nobox_violation() {
    let err = try_emit_with_stdlib_module(NOBOX_STDLIB_SINK_LIB, NOBOX_STDLIB_SINK_USER)
        .expect_err("a nobox violation in a stdlib body must reject, not silently skip");
    assert!(
        err.message.contains("nobox") && err.message.contains("SV2"),
        "{err:?}"
    );
}

const WIDE_NOBOX_TAIL_FORWARD: &str = r#"
nobox value Wide {
    a: int, b: int, c: int, d: int,
    e: int, f: int, g: int, h: int,
}

fn make_wide(seed: int) -> Wide {
    return Wide {
        a: seed, b: seed + 1, c: seed + 2, d: seed + 3,
        e: seed + 4, f: seed + 5, g: seed + 6, h: seed + 7,
    }
}

fn forward_wide(seed: int) -> Wide {
    return make_wide(seed)
}

fn forward_wide_indirect(f: (int) -> Wide, seed: int) -> Wide {
    return f(seed)
}

@inline inline(lua) fn observe(value: int) = "if value == -2147483648 then error('unreachable') end"

fn inspect_then_forward(seed: int) -> Wide {
    let w = make_wide(seed)
    observe(w.h)
    return w
}

fn consume_wide(prefix: int, w: Wide) -> int {
    return prefix + w.a + w.b + w.c + w.d + w.e + w.f + w.g + w.h
}

fn forward_into_call(seed: int) -> int {
    return consume_wide(100, make_wide(seed))
}

fn forward_indirect_into_call(f: (int) -> Wide, seed: int) -> int {
    return consume_wide(100, f(seed))
}

fn inspect_then_consume(seed: int) -> int {
    let w = make_wide(seed)
    observe(w.h)
    return consume_wide(100, w)
}

fn report_wide(w: Wide) { print("{w.a}") }

fn consume_then_return(seed: int) -> Wide {
    let w = make_wide(seed)
    report_wide(w)
    return w
}

fn main() {
    let w = forward_wide(10)
    print("{w.a + w.b + w.c + w.d + w.e + w.f + w.g + w.h}")
    let i = forward_wide_indirect(make_wide, 10)
    print("{i.a + i.b + i.c + i.d + i.e + i.f + i.g + i.h}")
    let c = inspect_then_forward(10)
    print("{c.a + c.b + c.c + c.d + c.e + c.f + c.g + c.h}")
    print("{forward_into_call(10)}")
    print("{forward_indirect_into_call(make_wide, 10)}")
    print("{inspect_then_consume(10)}")
    let r = consume_then_return(10)
    print("{r.a + r.b + r.c + r.d + r.e + r.f + r.g + r.h}")
}
"#;

#[test]
fn a_wide_nobox_tail_forward_returns_the_scalar_wire_directly() {
    let lua = emit_module(WIDE_NOBOX_TAIL_FORWARD);
    let body = function_body(&lua, "forward_wide");
    let callee = crate::common::bound_callee_name(&lua, "forward_wide", "make_wide_int");
    assert!(
        body.lines()
            .any(|line| line.contains(&format!("do return {callee}("))),
        "wide scalar return was captured and copied instead of tail-forwarded:\n{body}"
    );
    let indirect = function_body(&lua, "forward_wide_indirect");
    assert!(
        indirect
            .lines()
            .any(|line| line.contains("do return ") && line.contains('(')),
        "indirect scalar return was captured and copied instead of tail-forwarded:\n{indirect}"
    );
    let inspected = function_body(&lua, "inspect_then_forward");
    assert!(
        inspected
            .lines()
            .any(|line| line.matches(',').count() >= 7 && line.contains(" = ")),
        "a result observed before return was incorrectly tail-forwarded:\n{inspected}"
    );
    let call_arg = function_body(&lua, "forward_into_call");
    let consumer =
        crate::common::bound_callee_name(&lua, "forward_into_call", "consume_wide_int_Wide");
    let producer = crate::common::bound_callee_name(&lua, "forward_into_call", "make_wide_int");
    assert!(
        call_arg
            .lines()
            .any(|line| line.contains(&format!("{consumer}("))
                && line.contains(&format!("{producer}("))),
        "an unobserved scalar wire was captured only to become trailing call arguments:\n{call_arg}"
    );
    let indirect_call_arg = function_body(&lua, "forward_indirect_into_call");
    let consumer = crate::common::bound_callee_name(
        &lua,
        "forward_indirect_into_call",
        "consume_wide_int_Wide",
    );
    assert!(
        indirect_call_arg.lines().any(|line| {
            line.contains(&format!("{consumer}(")) && line.matches('(').count() >= 2
        }),
        "an indirect scalar wire was captured only to become trailing call arguments:\n{indirect_call_arg}"
    );
    let inspected_call_arg = function_body(&lua, "inspect_then_consume");
    assert!(
        inspected_call_arg
            .lines()
            .any(|line| line.matches(',').count() >= 7 && line.contains(" = ")),
        "a result observed before becoming call arguments was incorrectly forwarded:\n{inspected_call_arg}"
    );
    assert_eq!(run_main(&lua), "108\n108\n108\n208\n208\n208\n10\n108");
}

const NOBOX_OPAQUE_TOPLEVEL_FN_BY_VALUE: &str = r#"
nobox value NV2 { x: float, y: float }

inline(lua) fn opaque(v: NV2) -> float = """return v.x + v.y"""

fn apply(f: (NV2) -> float, v: NV2) -> float {
    return f(v)
}

fn main() {
    let f = opaque
    print("{apply(f, NV2 { x: 2.0, y: 3.0 })}")
}
"#;

#[test]
fn an_opaque_top_level_nobox_fn_value_still_rejects() {
    let err = try_emit_module(NOBOX_OPAQUE_TOPLEVEL_FN_BY_VALUE)
        .expect_err("an opaque body cannot adopt the type-derived nobox ABI");
    assert!(
        err.message.contains("opaque") && err.message.contains("fixed ABI"),
        "{err:?}"
    );
}

const NOBOX_IN_GC_VALUE_SURFACE: &str = r#"
nobox value V2 { x: float, y: float }
value Row { label: string, pos: V2 }
struct Holder { row: Row }
global G: Row
fn mk(l: string, x: float) -> Row { return Row { label: l, pos: V2 { x: x, y: 2.0 } } }
fn main() {
    let h = Holder { row: mk("a", 1.0) }
    print("{h.row}")
    h.row.pos.x = 5.0
    print("{h.row.pos.x} {h.row == mk("a", 5.0)} {h.row == mk("a", 6.0)}")
    let rows = [mk("b", 3.0), mk("c", 4.0)]
    rows.push(mk("d", 7.0))
    rows[1].pos.y = 8.0
    let total = 0.0
    for r in rows { total += r.pos.x + r.pos.y }
    print("{total} {rows[1]} {rows.len()}")
    G = mk("g", 8.0)
    G.pos.y = 9.0
    print("{G} {G.pos.y} {to_string(rows[2])}")
}
"#;

#[test]
fn a_stored_gc_tier_value_with_a_nobox_field_matches_its_unmarked_twin() {
    let marked = emit_module(NOBOX_IN_GC_VALUE_SURFACE);
    let plain = emit_module(&NOBOX_IN_GC_VALUE_SURFACE.replacen("nobox value V2", "value V2", 1));
    for name in ["mk", "main"] {
        let body = function_body(&marked, name);
        assert!(
            !body.contains("V2_ct("),
            "`{name}` boxed the nobox field:\n{body}"
        );
    }
    let expected = "Row { label: \"a\", pos: V2 { x: 1, y: 2 } }\n\
                    5 true false\n\
                    26 Row { label: \"c\", pos: V2 { x: 4, y: 8 } } 3\n\
                    Row { label: \"g\", pos: V2 { x: 8, y: 9 } } 9 Row { label: \"d\", pos: V2 { x: 7, y: 2 } }";
    assert_eq!(run_main(&plain), expected);
    assert_eq!(run_main(&marked), expected);
}

const NOBOX_IN_GC_VALUE_NULLABLE_NESTED: &str = r#"
nobox value V2 { x: float, y: float }
nobox value Rect { lo: V2, hi: V2 }
value Row { label: string, pos: V2, at: V2?, r: Rect }
value Gen<T> { t: T, pos: V2 }
struct Holder { row: Row, g: Gen<string> }
enum Slot { Empty, Has(Row) }
global G: Row
fn mk(l: string, x: float) -> Row {
    return Row { label: l, pos: V2 { x: x, y: 2.0 }, at: null, r: Rect { lo: V2 { x: 1.0, y: 2.0 }, hi: V2 { x: 3.0, y: 4.0 } } }
}
fn mk_at(l: string, x: float) -> Row {
    return Row { label: l, pos: V2 { x: x, y: 2.0 }, at: V2 { x: 1.5, y: 2.5 }, r: Rect { lo: V2 { x: 1.0, y: 2.0 }, hi: V2 { x: 3.0, y: 4.0 } } }
}
fn area(r: Rect) -> float { return (r.hi.x - r.lo.x) * (r.hi.y - r.lo.y) }
fn ax(r: Row) -> float {
    if let a = r.at { return a.x }
    return -1.0
}
fn main() {
    let h = Holder { row: mk("a", 1.0), g: Gen { t: "gt", pos: V2 { x: 6.0, y: 7.0 } } }
    h.row.pos = V2 { x: 10.0, y: 11.0 }
    h.row.at = V2 { x: 5.0, y: 6.0 }
    h.row.r.hi.x = 13.0
    print("{h.row}")
    print("{area(h.row.r)} {ax(h.row)} {h.g.t} {h.g.pos.y}")
    h.row.at = null
    print("{ax(h.row)} {h.row == mk("a", 10.0)}")
    let slots = [Slot::Has(mk("s", 4.0)), Slot::Empty, Slot::Has(mk_at("t", 5.0))]
    for s in slots {
        match s {
            Slot::Has(r) => print("has {r.label} {r.pos.x} {r.r.lo.y} {ax(r)}"),
            Slot::Empty => print("empty"),
        }
    }
    G = mk("g", 8.0)
    G.at = V2 { x: 1.5, y: 2.5 }
    print("{G == mk_at("g", 8.0)} {G == mk("g", 8.0)}")
    G.at = null
    print("{G == mk("g", 8.0)}")
}
"#;

#[test]
fn a_stored_gc_tier_value_keeps_nullable_and_nested_nobox_fields_as_leaves() {
    let lua = emit_module(NOBOX_IN_GC_VALUE_NULLABLE_NESTED);
    for name in ["mk", "mk_at", "main"] {
        let body = function_body(&lua, name);
        assert!(
            !body.contains("V2_ct(") && !body.contains("Rect_ct("),
            "`{name}` boxed a nobox field:\n{body}"
        );
    }
    assert_eq!(
        run_main(&lua),
        "Row { label: \"a\", pos: V2 { x: 10, y: 11 }, at: V2 { x: 5, y: 6 }, \
         r: Rect { lo: V2 { x: 1, y: 2 }, hi: V2 { x: 13, y: 4 } } }\n\
         24 5 gt 7\n\
         -1 false\n\
         has s 4 2 -1\n\
         empty\n\
         has t 5 2 1.5\n\
         true false\n\
         true"
    );
}

const NOBOX_TUPLE_COPY: &str = r#"
nobox value NV2 { x: float, y: float }
global XS: [(NV2, int)] = []
global YS: [(NV2, int)] = []

fn copy_direct(i: int) { YS.push(XS[i]) }
fn copy_through_a_name(i: int) {
    let t = XS[i]
    YS.push(t)
}
fn copy_each() { for t in XS { YS.push(t) } }
@inline fn stored_at(i: int) -> (NV2, int) { return XS[i] }
fn copy_through_an_inlined_return(i: int) { YS.push(stored_at(i)) }
fn copy_through_get(i: int) {
    if let t = XS.get(i) { YS.push(t) }
}
fn copy_through_coalesce(i: int) { YS.push(XS.get(i + 1) ?? XS[i]) }
fn copy_joined(i: int) {
    let t = if i > 0 { XS[i - 1] } else { XS[i] }
    YS.push(t)
}

value UV2 { x: float, y: float }
global UX: [(UV2, int)] = []
global UY: [(UV2, int)] = []
@inline fn stored_at_unmarked(i: int) -> (UV2, int) { return UX[i] }
fn copy_through_an_inlined_return_unmarked(i: int) { UY.push(stored_at_unmarked(i)) }
fn copy_joined_unmarked(i: int) {
    let t = if i > 0 { UX[i - 1] } else { UX[i] }
    UY.push(t)
}

fn main() {
    XS.push((NV2 { x: 1.0, y: 2.0 }, 3))
    UX.push((UV2 { x: 1.0, y: 2.0 }, 3))
    copy_direct(0)
    copy_through_a_name(0)
    copy_each()
    copy_through_an_inlined_return(0)
    copy_through_get(0)
    copy_through_coalesce(0)
    copy_joined(0)
    copy_through_an_inlined_return_unmarked(0)
    copy_joined_unmarked(0)
    let (v, n) = UY[0]
    print(v.x)
    print(v.y)
    print(n)
}
"#;

#[test]
fn copying_a_stored_tuple_element_allocates_nothing() {
    let lua = emit_module(NOBOX_TUPLE_COPY);
    for name in [
        "copy_direct",
        "copy_through_a_name",
        "copy_each",
        "copy_through_an_inlined_return",
        "copy_through_get",
        "copy_through_coalesce",
        "copy_joined",
        "copy_through_an_inlined_return_unmarked",
        "copy_joined_unmarked",
    ] {
        let body = function_body(&lua, name);
        assert!(
            !body.contains("({") && !body.contains("NV2_ct("),
            "`{name}` packed a copy of a stored tuple:\n{body}"
        );
        assert!(
            !body.contains("[1]"),
            "`{name}` read a slot of a tuple it only copies:\n{body}"
        );
    }
    assert_eq!(run_main(&lua), "1\n2\n3");
}
