use crate::common::{
    body_of_synthetic, emit_module, function_body, run_main, try_emit_module,
    try_emit_with_stdlib_module,
};

const NOBOX_FIELD: &str = r#"
nobox value NV2 { x: float, y: float }
struct Player { pos: NV2, hp: int }

fn make_player(x: float, y: float) -> Player {
    return Player { pos: NV2 { x: x, y: y }, hp: 10 }
}

fn main() {
    let _ = make_player(1.0, 2.0)
}
"#;

#[test]
fn nobox_field_is_stored_as_parent_leaves() {
    let lua = emit_module(NOBOX_FIELD);
    let body = function_body(&lua, "make_player");
    assert!(
        body.contains(r#"["pos.x"]"#) && body.contains(r#"["pos.y"]"#),
        "nobox field did not flatten into its parent:\n{body}"
    );
    assert!(
        !body.contains("NV2_ct("),
        "nobox field materialized a cdata value:\n{body}"
    );
}

const NOBOX_EXPLICIT_MATERIALIZE: &str = r#"
nobox value NV2 { x: float, y: float }

@nomangle inline(lua) fn foreign_sink(v: NV2) = "return"

fn explicit(v: NV2) {
    foreign_sink(materialize(v))
}

fn main() {
    explicit(NV2 { x: 1.0, y: 2.0 })
}
"#;

#[test]
fn explicit_materialize_authorizes_exactly_one_box() {
    let lua = emit_module(NOBOX_EXPLICIT_MATERIALIZE);
    let body = function_body(&lua, "explicit");
    assert_eq!(
        body.matches("NV2_ct(").count(),
        1,
        "materialize must introduce exactly one deliberate box:\n{body}"
    );
}

const NOBOX_NULLABLE_FIELD: &str = r#"
nobox value NV2 { x: float, y: float }
struct Txt { pos: NV2?, t: float }

// The write half: an OPAQUE nullable value into the field (the literal and the
// `null` cases already threaded).
fn spawn(p: NV2?) -> Txt { return Txt { pos: p, t: 1.0 } }
fn spawn_literal() -> Txt { return Txt { pos: NV2 { x: 9.0, y: 8.0 }, t: 2.0 } }

// The read half: presence test, then a read through the narrowed field.
fn read(c: Txt) -> float {
    if c.pos != null { return c.pos.x }
    return -1.0
}

// Mutation through the field, both directions.
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

/// Shape (b): a `T?` `nobox` value in a struct FIELD, stored as presence plus
/// leaves rather than a box. Presence lives at the field's own undotted key,
/// which flattening leaves free, so an absent child is a nil presence over nil
/// leaves and no consumer needs a box to test against.
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
        body.contains(r#"["pos.x"]"#) && body.contains(r#"["pos.y"]"#),
        "the nullable field did not flatten into its parent's leaves:\n{body}"
    );
    assert_eq!(run_main(&lua), "3\n-1\n9\n-1\n7");
}

const NOBOX_TWO_SINKS_IN_ONE_BODY: &str = r#"
nobox value NV2 { x: float, y: float }

fn mk(x: float, y: float) -> NV2 { return NV2 { x: x, y: y } }

// Two INDEPENDENT sinks of the same class, on distinct lines, in one body.
// Same permanent sink as `NOBOX_INDIRECT_SINK` -- read the note there.
fn bad() -> int {
    let t = (mk(1.0, 2.0), 1.0)
    print("{t}")
    let u = (mk(3.0, 4.0), 2.0)
    print("{u}")
    return 0
}

fn main() {
    print(bad())
}
"#;

/// Two INDEPENDENT violations in one body are both reported. The verifier used
/// to return on the first, so a body with two sinks looked like a body with
/// one -- which is how `queue_effect_uniforms`'s `pass_uniforms` gap stayed
/// invisible behind an earlier rejection, and how any "N sinks left" count read
/// as a total when it was a floor.
#[test]
fn every_nobox_violation_in_one_body_is_reported() {
    let err =
        try_emit_module(NOBOX_TWO_SINKS_IN_ONE_BODY).expect_err("two nobox sinks must reject");
    let all: Vec<&str> = std::iter::once(err.message.as_str())
        .chain(err.siblings.iter().map(|s| s.message.as_str()))
        .collect();
    let sinks = all
        .iter()
        .filter(|m| m.contains("survived lowering"))
        .count();
    assert!(
        sinks >= 2,
        "expected both sinks reported, got {sinks}: {all:#?}"
    );
    let lines: std::collections::HashSet<usize> = std::iter::once(&err)
        .chain(err.siblings.iter())
        .filter(|e| e.message.contains("survived lowering"))
        .map(|e| e.location.line)
        .collect();
    assert!(
        lines.len() >= 2,
        "both sinks reported at the same line, so one span is standing in for two: {lines:?}"
    );
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

const NOBOX_CLOSURE_CAPTURE: &str = r#"
nobox value NV2 { x: float, y: float }

fn capture(v: NV2) -> fn() -> float {
    return () => v.x + v.y
}

fn main() {
    let f = capture(NV2 { x: 1.0, y: 2.0 })
    let _ = f()
}
"#;

#[test]
fn closure_capture_stores_nobox_leaves() {
    let lua = emit_module(NOBOX_CLOSURE_CAPTURE);
    // `capture` plus the lambda hoisted out of it. Read by NAME rather than as
    // the text between two headers: the bound callee shape moves an enclosing
    // definition into a factory, so the two are not adjacent in the chunk.
    let names = crate::common::definitions_matching(&lua, "capture");
    assert!(!names.is_empty(), "no `capture` definition in:\n{lua}");
    let bodies: String = names
        .iter()
        .map(|name| crate::common::function_body(&lua, name))
        .collect();
    assert!(
        !bodies.contains("NV2_ct("),
        "closure capture materialized its nobox value:\n{bodies}"
    );
}

const NOBOX_COLLECTION: &str = r#"
nobox value NV2 { x: float, y: float }

fn sum() -> float {
    let xs: [NV2] = [NV2 { x: 1.0, y: 2.0 }]
    xs.push(NV2 { x: 3.0, y: 4.0 })
    let v = xs[1]
    return v.x + v.y
}

fn main() {
    let _ = sum()
}
"#;

#[test]
fn collection_storage_keeps_nobox_elements_in_columns() {
    let lua = emit_module(NOBOX_COLLECTION);
    let body = function_body(&lua, "sum");
    assert!(
        crate::common::has_vec_header(body, "columns", Some("NV2")),
        "nobox collection did not use columns:\n{body}"
    );
    assert!(
        !body.contains("NV2_ct("),
        "nobox collection materialized an element:\n{body}"
    );
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

// Re-pointed for the same reason as `NOBOX_INDIRECT_SINK`, and to the same
// permanent sink -- see the note there before changing either.
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

const NOBOX_NESTED_FN_INFERRED_RETURN: &str = r#"
nobox value NV2 { x: float, y: float }

fn outer(k: float) -> float {
    fn make(v: float) {
        NV2 { x: v, y: v + 1.0 }
    }
    let p = make(k)
    return p.x + p.y
}

fn main() { print("{outer(1.0)}") }
"#;

#[test]
fn a_nested_fn_with_an_inferred_nobox_return_threads_leaves() {
    // An inferred return is as much a part of the signature as a written one:
    // reading only the optional annotation drops the callee to the boxed ABI.
    let lua = emit_module(NOBOX_NESTED_FN_INFERRED_RETURN);
    let body = body_of_synthetic(&lua, "_make_");
    assert!(
        !body.contains("NV2_ct("),
        "an inferred nobox return materialized:\n{body}"
    );
    assert_eq!(run_main(&lua), "3");
}

const NOBOX_FN_TYPE_AGREES: &str = r#"
nobox value NV2 { x: float, y: float }

fn indirect(f: (NV2) -> float, v: NV2) -> float { return f(v) }

fn both_ways(p: NV2) -> float {
    fn sum(v: NV2) -> float { return v.x + v.y }
    // The SAME fn, called directly and through the fn value, in one body.
    return sum(p) + indirect(sum, p)
}

fn main() {
    print(both_ways(NV2 { x: 1.0, y: 2.0 }))
    print(indirect(w => w.x + w.y, NV2 { x: 1.0, y: 2.0 }))
}
"#;

/// The load-bearing invariant of the fn-value ABI: a DIRECT call, a call through
/// the fn VALUE, and the definition all pass the same arity, because all three
/// read the leaf count off the same fn type. `direct` is called both ways in one
/// program, so a disagreement is a wrong-arity call rather than a subtle
/// mis-shape -- and passing the SAME fn both ways is the case a callee-identity
/// ABI cannot get right.
#[test]
fn direct_and_indirect_calls_of_one_fn_agree_on_arity() {
    let lua = emit_module(NOBOX_FN_TYPE_AGREES);
    let body = function_body(&lua, "indirect");
    assert!(
        body.contains("c0(c1, c2)"),
        "the indirect call must pass two leaves, not one box:\n{body}"
    );
    assert_eq!(run_main(&lua), "6\n3");
}

const NOBOX_TOPLEVEL_FN_BY_VALUE: &str = r#"
nobox value NV2 { x: float, y: float }
value Boxed { x: float }

fn combine(n: float, p: NV2, b: Boxed) -> float {
    return n + p.x + p.y + b.x
}

fn apply(f: (float, NV2, Boxed) -> float, n: float, p: NV2, b: Boxed) -> float {
    return f(n, p, b)
}

fn main() {
    let p = NV2 { x: 2.0, y: 3.0 }
    let b = Boxed { x: 4.0 }
    print("{combine(1.0, p, b)} {apply(combine, 5.0, p, b)}")
}
"#;

#[test]
fn a_top_level_fn_value_uses_the_type_derived_nobox_abi() {
    let lua = emit_module(NOBOX_TOPLEVEL_FN_BY_VALUE);
    let body = function_body(&lua, "combine");
    let header = body.lines().next().expect("function header");
    let params = header
        .split_once('(')
        .and_then(|(_, rest)| rest.split_once(')'))
        .map(|(params, _)| params.split(',').count())
        .expect("function params");
    assert_eq!(
        params, 4,
        "float + two nobox leaves + one boxed value:\n{header}"
    );
    assert!(
        !body.contains("NV2_ct("),
        "the top-level fn value materialized NV2:\n{body}"
    );
    assert_eq!(run_main(&lua), "10 14");
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

const NOBOX_REJECTION_SITE: &str = r#"
nobox value NV2 { x: float, y: float }

@nomangle inline(lua) fn sink(v: NV2) = "return"

fn last(ps: [NV2]) -> float {
    let total = 0.0
    for p in ps { total = total + p.x }
    sink(NV2 { x: total, y: 0.0 })
    return total
}

fn main() { let _ = last([]) }
"#;

#[test]
fn a_nobox_rejection_anchors_on_the_offending_expression() {
    let err =
        try_emit_module(NOBOX_REJECTION_SITE).expect_err("a boxed sink materializes the value");
    // Assert on the byte offset, not `line`/`col`: the three coordinate systems
    // for spans disagree on their base, and the offset is
    // the one that needs no conversion. The `fn` header is the anchor this used
    // to report, and a whole-fn anchor cannot tell two violations in one body
    // apart -- which is how one sink hid another inside `draw_fractures`.
    let start = NOBOX_REJECTION_SITE
        .find("    sink(NV2")
        .expect("fixture line");
    let end = start
        + NOBOX_REJECTION_SITE[start..]
            .find('\n')
            .expect("fixture line ends");
    assert!(
        (start..=end).contains(&err.location.offset),
        "rejection did not anchor on the call that boxes: {err:?}"
    );
}

const NOBOX_IN_GC_VALUE_DECLS: &str = r#"
nobox value Id { value: int }
nobox value Grid { x: int, y: int }
value Map { space: Id, size: Grid, tiles: [int] }
struct World { map: Map, n: int }
enum Slot { Empty, Has(Map) }
fn total(m: Map) -> int { return m.space.value + m.size.x + m.size.y + m.tiles.len() }
"#;

fn nobox_in_gc_value_src(site: &str, main: &str) -> String {
    format!("{NOBOX_IN_GC_VALUE_DECLS}{site}\nfn main() {{ {main} }}\n")
}

// Positions that carry the outer value as leaves: a flat-enum payload and a
// scalar-ABI argument. Neither builds the `Map_vt` box, so neither boxes a
// nobox field.
const NOBOX_IN_GC_VALUE_LEAF_SITES: [(&str, &str, &str, &str); 2] = [
    (
        "enum payload",
        "fn site(s: int) -> Slot { return Slot::Has(Map { space: Id { value: s }, size: Grid { x: 1, y: 1 }, tiles: [] }) }",
        "match site(7) { Slot::Has(m) => print(m.space.value + m.size.x), Slot::Empty => print(-1) }",
        "8",
    ),
    (
        "call argument",
        "fn site(s: int) -> int { return total(Map { space: Id { value: s }, size: Grid { x: 1, y: 1 }, tiles: [9] }) }",
        "print(site(7))",
        "10",
    ),
];

#[test]
fn a_gc_tier_value_threaded_as_leaves_keeps_its_nobox_fields_unboxed() {
    for (what, site, main, expected) in NOBOX_IN_GC_VALUE_LEAF_SITES {
        let lua = match try_emit_module(&nobox_in_gc_value_src(site, main)) {
            Ok(lua) => lua,
            Err(e) => panic!("the {what} site must emit: {e:?}"),
        };
        let body = function_body(&lua, "site");
        assert!(
            !body.contains("Id_ct(") && !body.contains("Grid_ct(") && !body.contains("Map_vt("),
            "the {what} site boxed:\n{body}"
        );
        assert_eq!(run_main(&lua), expected, "the {what} site");
    }
}

// Positions that store the outer value in a table slot, so its `Map_vt` box
// is real. The box keys each `nobox` field's leaves by dotted path
// (`size.x`), the layout a plain struct and a leaf-table value already use, so
// storing it builds no `Id_ct`/`Grid_ct`.
const NOBOX_IN_GC_VALUE_BOXED_SITES: [(&str, &str, &str, &str); 3] = [
    (
        "struct literal field",
        "fn site(s: int) -> World { return World { map: Map { space: Id { value: s }, size: Grid { x: 2, y: 3 }, tiles: [4] }, n: 1 } }",
        "let w = site(7)\n w.map.size.x = 11\n print(\"{w.map.space.value} {w.map.size.x} {w.map.size.y} {total(w.map)}\")",
        "7 11 3 22",
    ),
    (
        "Vec element",
        "fn site(s: int) -> [Map] { return [Map { space: Id { value: s }, size: Grid { x: 1, y: 1 }, tiles: [] }] }",
        "let v = site(5)\n v[0].size.y = 12\n v.push(v[0])\n print(\"{v[1].space.value} {v[1].size.y} {v.len()} {v[0] == v[1]}\")",
        "5 12 2 true",
    ),
    (
        "global",
        "global G: Map\nfn site(s: int) { G = Map { space: Id { value: s }, size: Grid { x: 4, y: 5 }, tiles: [1, 2] } }",
        "site(9)\n G.size.y = 6\n print(\"{G.space.value} {G.size.x} {G.size.y} {total(G)}\")",
        "9 4 6 21",
    ),
];

#[test]
fn a_stored_gc_tier_value_keeps_its_nobox_fields_as_leaves() {
    for (what, site, main, expected) in NOBOX_IN_GC_VALUE_BOXED_SITES {
        let lua = match try_emit_module(&nobox_in_gc_value_src(site, main)) {
            Ok(lua) => lua,
            Err(e) => panic!("the {what} site must emit: {e:?}"),
        };
        let body = function_body(&lua, "site");
        assert!(
            !body.contains("Id_ct(") && !body.contains("Grid_ct("),
            "the {what} site boxed a nobox field:\n{body}"
        );
        assert_eq!(run_main(&lua), expected, "the {what} site");
    }
}

// The observable surface of a stored GC-tier value with a `nobox` field --
// field write through a row, `==`, structural `tostring`, a Vec of them -- is
// the unmarked twin's, which boxes `V2` as an ordinary value. Both must print
// the same text; the marked one must not build a `V2` box to do it.
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
    let marked = match try_emit_module(NOBOX_IN_GC_VALUE_SURFACE) {
        Ok(lua) => lua,
        Err(e) => panic!("a stored GC-tier value with a nobox field must emit: {e:?}"),
    };
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

// Nullable, nested and generic-parent `nobox` fields of a stored GC-tier value:
// whole-field and nested-leaf writes, an absent tag written back to `null`, an
// enum payload, and `==` over the presence tag. The unmarked twin is no oracle
// for `==` here: it compares a `V2?` field by reference.
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
    let lua = match try_emit_module(NOBOX_IN_GC_VALUE_NULLABLE_NESTED) {
        Ok(lua) => lua,
        Err(e) => panic!("nullable and nested nobox fields of a GC-tier value must emit: {e:?}"),
    };
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

// A leaf read through a `nobox` field of an UNBOXED GC-tier value (a scalar-ABI
// param) projects the leaf off the value's Group. The `_vt` box keys that leaf
// as `lock.place.x`, but building the box just to read the key back is an
// allocation the leaf-threaded value never needed (application's
// `hostile_target_evaluate` reads `input.lock.target` this way per mob).
const NOBOX_IN_GC_VALUE_PARAM_LEAF_READ: &str = r#"
nobox value P { x: int, y: int }
nobox value Id { a: int, b: int }
enum Who { Player, Mob(Id) }
nobox value Cand { target: Who, place: P, ok: bool }
nobox value Decision { target: Who, place: P }
enum Eval { Found(Decision, bool), Nothing(bool) }
value Snap { label: string, lock: Cand, rest: [int] }
fn pick(s: Snap) -> Eval {
    if s.lock.ok { return Eval::Found(Decision { target: s.lock.target, place: s.lock.place }, false) }
    return Eval::Nothing(true)
}
fn main() {
    let e = pick(Snap { label: "s", lock: Cand { target: Who::Mob(Id { a: 5, b: 6 }), place: P { x: 3, y: 4 }, ok: true }, rest: [] })
    match e {
        Eval::Found(d, _) => match d.target {
            Who::Mob(id) => print(id.b + d.place.x + d.place.y),
            Who::Player => print(0),
        },
        Eval::Nothing(_) => print(-1),
    }
}
"#;

#[test]
fn a_leaf_read_through_a_nobox_field_of_a_gc_tier_param_does_not_box() {
    let lua = emit_module(NOBOX_IN_GC_VALUE_PARAM_LEAF_READ);
    let body = function_body(&lua, "pick");
    assert!(
        !body.contains("Snap_vt("),
        "`pick` boxed its param to read a leaf:\n{body}"
    );
    assert_eq!(run_main(&lua), "13");
}

// Functional update of a GC-tier value whose base is a local: the fields the
// literal does not name come off the base by the box's layout, so a `nobox`
// field is read by its leaves (application's `CarriedItemView { ..view, id }`).
const NOBOX_IN_GC_VALUE_FUNCTIONAL_UPDATE: &str = r#"
nobox value C { r: float, g: float }
enum Kind { Plain, Tagged(string) }
value View { id: int, kind: Kind, name: string, color: C }
fn mk(k: Kind) -> View { return View { id: 1, kind: k, name: "n", color: C { r: 0.5, g: 0.25 } } }
fn use_it(v: View) -> float { return v.color.r + v.color.g + v.id }
fn main() {
    let view = mk(Kind::Tagged("t"))
    print(use_it(View { ..view, id: 3 }))
}
"#;

#[test]
fn a_functional_update_of_a_gc_tier_value_reads_nobox_fields_by_leaf() {
    let lua = emit_module(NOBOX_IN_GC_VALUE_FUNCTIONAL_UPDATE);
    let body = function_body(&lua, "main");
    assert!(
        !body.contains("View_vt("),
        "the update boxed its base:\n{body}"
    );
    assert_eq!(run_main(&lua), "3.75");
}

// The same read off a BOXED GC-tier local (reassigned in a loop, so it lives in
// one cell): the enum inside the `nobox` field is keyed by its leaves
// (`lock.target.__tag`, ...), so reading the whole enum off `lock.target`
// found nil. A hostile-target lock read in an application had this shape.
const NOBOX_IN_GC_VALUE_BOXED_LOCAL_ENUM_READ: &str = r#"
nobox value P { x: int, y: int }
nobox value Id { a: int, b: int }
enum Who { Player, Mob(Id) }
nobox value Cand { target: Who, place: P, ok: bool }
value Snap { label: string, lock: Cand, rest: [int] }
fn vis(c: Cand, v: bool) -> Cand { return Cand { target: c.target, place: c.place, ok: v } }
fn look(s0: Snap) -> int {
    let s = s0
    for _ in 0..4 {
        let id = match s.lock.target {
            Who::Player => return -1,
            Who::Mob(id) => id,
        }
        if s.lock.ok { return id.b + s.lock.place.y }
        let lock = vis(s.lock, true)
        s = Snap { ..s, lock }
    }
    return -2
}
fn main() {
    print(look(Snap { label: "s", lock: Cand { target: Who::Mob(Id { a: 5, b: 6 }), place: P { x: 3, y: 4 }, ok: false }, rest: [] }))
}
"#;

#[test]
fn an_enum_in_a_nobox_field_of_a_boxed_gc_tier_local_reads_by_leaf() {
    let lua = emit_module(NOBOX_IN_GC_VALUE_BOXED_LOCAL_ENUM_READ);
    assert_eq!(run_main(&lua), "10");
    let body = function_body(&lua, "look");
    assert!(
        !body.contains("Snap_vt("),
        "`look` boxed its local to read a field:\n{body}"
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

/// Copying a stored tuple element shares its immutable box -- directly, through
/// an unwritten name, as a loop element, through an inlined return or a
/// nullable return (`get`, `??`), or out of a join whose arms are all stored --
/// rather than packing a new table, and reads none of its slots.
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
