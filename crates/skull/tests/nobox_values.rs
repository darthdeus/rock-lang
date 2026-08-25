use crate::common::{
    body_of_synthetic, emit_module, function_body, run_luajit, try_emit_module,
    try_emit_with_stdlib_module,
};

fn global_session_slots(lua: &str, name: &str) -> Vec<u32> {
    let marker = format!("identity=\"global:<test>::{name}\"");
    let entry = lua
        .split(&marker)
        .nth(1)
        .unwrap_or_else(|| panic!("missing StateSchema entry for {name}"));
    let slots = entry
        .split("session_keys={")
        .nth(1)
        .expect("StateSchema entry has Session keys")
        .split('}')
        .next()
        .expect("StateSchema Session keys close");
    slots
        .split(',')
        .map(|slot| slot.parse::<u32>().expect("Lua global Session slot is numeric"))
        .collect()
}

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

const NOBOX_FIELD_BIND: &str = r#"
nobox value NV2 { x: float, y: float }
struct Player { pos: NV2 }

fn sum_pos(p: Player) -> float {
    let v = p.pos
    return v.x + v.y
}

fn main() {
    let _ = sum_pos(Player { pos: NV2 { x: 1.0, y: 2.0 } })
}
"#;

#[test]
fn binding_a_nobox_field_does_not_materialize_it() {
    let lua = emit_module(NOBOX_FIELD_BIND);
    let body = function_body(&lua, "sum_pos");
    assert!(
        !body.contains("NV2_ct("),
        "binding a nobox field materialized a cdata value:\n{body}"
    );
}

const NOBOX_INLINE_LUA_SINK: &str = r#"
nobox value NV2 { x: float, y: float }

@nomangle inline(lua) fn foreign_sink(v: NV2) = "return"

fn bad(v: NV2) {
    foreign_sink(v)
}

fn main() {
    bad(NV2 { x: 1.0, y: 2.0 })
}
"#;

#[test]
fn nobox_value_rejects_an_inline_lua_sink() {
    let err = try_emit_module(NOBOX_INLINE_LUA_SINK)
        .expect_err("an implicit inline(lua) materialization must reject");
    assert!(err.message.contains("nobox") && err.message.contains("foreign_sink"), "{err:?}");
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

const NOBOX_WHOLE_VALUE_OPERATORS: &str = r#"
nobox value NV2 { x: float, y: float }

fn same(a: NV2, b: NV2) -> bool {
    return a == b
}

fn absent(v: NV2?) -> bool {
    return v == null
}

fn render(v: NV2) -> string {
    return "{v}"
}

fn main() {}
"#;

#[test]
fn nobox_equality_compares_leaves_without_materializing() {
    let lua = emit_module(NOBOX_WHOLE_VALUE_OPERATORS);
    let body = function_body(&lua, "same");
    assert!(!body.contains("NV2_ct("), "nobox equality materialized its operands:\n{body}");
}

const NOBOX_MIXED_NULLABLE_EQUALITY: &str = r#"
nobox value NV2 { x: float, y: float }

fn matches(value: NV2?, expected: NV2) -> bool {
    return value == expected && expected == value
}

fn main() {
    let expected = NV2 { x: 1.0, y: 2.0 }
    print(matches(expected, expected))
    print(!matches(null, expected))
    print(!matches(NV2 { x: 1.0, y: 3.0 }, expected))
}
"#;

#[test]
fn nullable_nobox_equality_with_a_non_nullable_operand_compares_presence_and_leaves() {
    let lua = emit_module(NOBOX_MIXED_NULLABLE_EQUALITY);
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "nobox_mixed_nullable_equality"),
        "true\ntrue\ntrue"
    );
}

#[test]
fn nobox_null_check_does_not_materialize() {
    let lua = emit_module(NOBOX_WHOLE_VALUE_OPERATORS);
    let body = function_body(&lua, "absent");
    assert!(!body.contains("NV2_ct("), "nobox null check materialized its operand:\n{body}");
}

const NOBOX_NULLABLE_CALL: &str = r#"
nobox value NV2 { x: float, y: float }

fn absent(v: NV2?) -> bool {
    return v == null
}

fn relay(v: NV2?) -> NV2? {
    return v
}

fn check() -> bool {
    return absent(relay(null)) && !absent(relay(NV2 { x: 1.0, y: 2.0 }))
}

fn main() {
    print(check())
}
"#;

#[test]
fn nullable_nobox_call_threads_presence_and_leaves() {
    let lua = emit_module(NOBOX_NULLABLE_CALL);
    for name in ["absent", "relay", "check"] {
        let body = function_body(&lua, name);
        assert!(
            !body.contains("NV2_ct("),
            "nullable nobox call materialized in `{name}`:\n{body}"
        );
    }
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "nobox_nullable"),
        "true"
    );
}

const NOBOX_NULLABLE_LOCAL: &str = r#"
nobox value NV2 { x: float, y: float }

fn render(flag: bool) -> string {
    let v: NV2? = null
    if flag {
        v = NV2 { x: 1.0, y: 2.0 }
    }
    return "{v}"
}

fn main() {
    print(render(false))
    print(render(true))
}
"#;

#[test]
fn nullable_nobox_local_keeps_presence_in_leaf_cells() {
    let lua = emit_module(NOBOX_NULLABLE_LOCAL);
    let body = function_body(&lua, "render");
    assert!(
        !body.contains("NV2_ct("),
        "nullable nobox local materialized:\n{body}"
    );
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "nobox_nullable_local"),
        "nil\nNV2 { x = 1, y = 2 }"
    );
}

const NOBOX_FIELD_WIDEN: &str = r#"
nobox value NV2 { x: float, y: float }
struct Player { pos: NV2, hp: int }

fn absent(v: NV2? = null) -> bool { return v == null }
fn sum(v: NV2? = null) -> float {
    if v == null { return -1.0 }
    let u = v.unwrap()
    return u.x + u.y
}

// The shape: a value loaded from a struct FIELD, widened to `T?`. A LITERAL
// ctor and a CALL RESULT at the same param already thread as leaves.
fn from_field(p: Player) -> bool { return absent(p.pos) }
fn sum_field(p: Player) -> float { return sum(p.pos) }

// The same widening into a nullable LOCAL rather than a param.
fn to_local(p: Player) -> float {
    let q: NV2? = null
    q = p.pos
    if q == null { return -1.0 }
    return q.unwrap().x
}

fn main() {
    let p = Player { pos: NV2 { x: 1.0, y: 2.0 }, hp: 1 }
    print(absent(p.pos))
    print(sum_field(p))
    print(to_local(p))
}
"#;

#[test]
fn a_field_load_widens_to_nullable_without_materializing() {
    let lua = emit_module(NOBOX_FIELD_WIDEN);
    for name in ["from_field", "sum_field", "to_local"] {
        let body = function_body(&lua, name);
        assert!(
            !body.contains("NV2_ct("),
            "a field load widened to `T?` materialized in `{name}`:\n{body}"
        );
    }
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "nobox_field_widen"),
        "false\n3\n1"
    );
}

const NOBOX_NULLABLE_FIELD: &str = r#"
nobox value NV2 { x: float, y: float }
struct Txt { pos: NV2?, t: float }

// The write half: an OPAQUE nullable value into the field (the literal and the
// `null` cases already threaded).
fn spawn(p: NV2?) -> Txt { return Txt { pos: p, t: 1.0 } }
fn spawn_literal() -> Txt { return Txt { pos: NV2 { x: 9.0, y: 8.0 }, t: 2.0 } }

// The read half: presence test, then unwrap.
fn read(c: Txt) -> float {
    if c.pos != null { return c.pos.unwrap().x }
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
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "nobox_nullable_field"),
        "3\n-1\n9\n-1\n7"
    );
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
/// one -- which is how `bitgun_queue_heat_shimmer`'s `pass_uniforms` gap stayed
/// invisible behind an earlier rejection, and how any "N sinks left" count read
/// as a total when it was a floor.
#[test]
fn every_nobox_violation_in_one_body_is_reported() {
    let err = try_emit_module(NOBOX_TWO_SINKS_IN_ONE_BODY)
        .expect_err("two nobox sinks must reject");
    let all: Vec<&str> = std::iter::once(err.message.as_str())
        .chain(err.siblings.iter().map(|s| s.message.as_str()))
        .collect();
    let sinks = all.iter().filter(|m| m.contains("survived lowering")).count();
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

const NOBOX_NESTED_FIELD_WIDEN: &str = r#"
nobox value NV2 { x: float, y: float }
nobox value Nest { a: NV2, s: float }
struct Deep { n: Nest }

fn mk_nest() -> Nest { return Nest { a: NV2 { x: 4.0, y: 5.0 }, s: 6.0 } }
fn nested_absent(v: Nest? = null) -> bool { return v == null }
fn from_nested_field(d: Deep) -> bool { return nested_absent(d.n) }

fn main() {
    let n = mk_nest()
    let d = Deep { n: n }
    print(from_nested_field(d))
}
"#;

/// The two-level twin of [`a_field_load_widens_to_nullable_without_materializing`]:
/// a `nobox` child holding a `nobox` field OF ITS OWN. The rejection landed on
/// the CTOR (`Deep { n: n }`) after `from_nested_field` inlines, and was never a
/// hole in the widening -- the same nest rejected with no widening anywhere,
/// because the splice walks asked how `Nest` stores `a` instead of what leaves
/// it contributes.
#[test]
fn a_nested_nobox_child_widens_without_materializing() {
    let lua = emit_module(NOBOX_NESTED_FIELD_WIDEN);
    for name in ["mk_nest", "nested_absent", "from_nested_field"] {
        let body = function_body(&lua, name);
        assert!(
            !body.contains("NV2_ct("),
            "a two-level nobox nest materialized in `{name}`:\n{body}"
        );
    }
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "nobox_nested_field_widen"),
        "false"
    );
}

const NOBOX_NESTED_PLAIN_OWNER_FIELD_READ: &str = r#"
nobox value Id { value: int }
nobox value Grid { x: int, y: int }
nobox value Place { space: Id, grid: Grid }
struct Owner { place: Place }

fn id_equal(a: Id, b: Id) -> bool { return a.value == b.value }

fn contains(owners: Arena<Owner>, space: Id, grid: Grid) -> bool {
    for (_, owner) in owners {
        if id_equal(owner.place.space, space) && owner.place.grid == grid {
            return true
        }
    }
    return false
}

fn main() {
    let owner = Owner {
        place: Place { space: Id { value: 11 }, grid: Grid { x: 13, y: 17 } }
    }
    let owners: Arena<Owner> = arena_new()
    owners.insert(owner)
    print(contains(owners, Id { value: 11 }, Grid { x: 13, y: 17 }))
}
"#;

#[test]
fn a_plain_owner_reads_every_leaf_of_a_nested_nobox_field() {
    let lua = emit_module(NOBOX_NESTED_PLAIN_OWNER_FIELD_READ);
    let body = function_body(&lua, "contains");
    for field in ["place.space.value", "place.grid.x", "place.grid.y"] {
        assert!(
            body.contains(&format!(r#"["{field}"]"#)),
            "nested nobox field read stopped before `{field}`:\n{body}"
        );
    }
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "nobox_nested_plain_owner_read"),
        "true"
    );
}

const NOBOX_COLUMN_WIDEN: &str = r#"
nobox value NV2 { x: float, y: float }

// CONTROL: a whole element read out of a COLUMN, not widened. Emits as leaf
// reads, and must keep doing so.
fn ctrl(xs: [NV2]) -> NV2 { return xs[0] }

// The shape: the same column read, widened to `T?`. The first two inline their
// stdlib body into this one; the rest mint the value inside their own
// specialization, where the call site is not in view.
fn via_get(xs: [NV2]) -> NV2? { return xs.get(0) }
fn via_vec_get(xs: [NV2]) -> NV2? { return vec_get(xs, 1) }
fn via_pop(xs: [NV2]) -> NV2? { return xs.pop() }
fn via_swap_remove(xs: [NV2]) -> NV2? { return xs.swap_remove(0) }
fn via_remove_at(xs: [NV2]) -> NV2? { return xs.remove_at(0) }
fn via_sample(xs: [NV2]) -> NV2? { return xs.sample() }

fn show(v: NV2?) -> float {
    if v == null { return -1.0 }
    let u = v.unwrap()
    return u.x * 10.0 + u.y
}

fn main() {
    let xs = [NV2 { x: 1.0, y: 2.0 }, NV2 { x: 3.0, y: 4.0 }, NV2 { x: 5.0, y: 6.0 }]
    print(ctrl(xs).x)
    print(show(via_get(xs)))
    print(show(via_vec_get(xs)))
    print(show(xs.get(9)))
    print(show(via_pop(xs)))
    print(show(via_swap_remove(xs)))
    print(show(via_remove_at(xs)))
    print(show(via_pop(xs)))
    print(show(via_sample(xs)))
    print(show(via_sample([NV2 { x: 7.0, y: 8.0 }])))
}
"#;

/// Shape (a) with a COLUMN source instead of a field source: a whole element
/// read out of a `[T]` column, widened to `T?`, is a Group over the element's
/// leaf columns plus presence -- never a box. Six stdlib entry points share it
/// (`vec_get`, `Vec.get`, `pop`, `remove_at`, `swap_remove`, `sample`), and the
/// last four are not `@inline`, so the box was minted inside their own
/// specialization.
#[test]
fn a_column_read_widens_to_nullable_without_materializing() {
    let lua = emit_module(NOBOX_COLUMN_WIDEN);
    for name in [
        "ctrl",
        "via_get",
        "via_vec_get",
        "via_pop",
        "via_swap_remove",
        "via_remove_at",
        "via_sample",
    ] {
        let body = function_body(&lua, name);
        assert!(
            !body.contains("NV2_ct("),
            "a column read widened to `T?` materialized in `{name}`:\n{body}"
        );
    }
    // The four non-`@inline` entry points mint the value inside their own
    // specialized copy, which carries the `_spec_<key>` suffix.
    for needle in [
        "pop_arr_T_ret_T_opt_spec",
        "swap_remove_arr_T_int_ret_T_opt_spec",
        "remove_at_arr_T_int_ret_T_opt_spec",
        "sample_arr_T_ret_T_opt_spec",
    ] {
        let body = body_of_synthetic(&lua, needle);
        assert!(
            !body.contains("NV2_ct("),
            "the `{needle}` specialization materialized the element it returns:\n{body}"
        );
    }
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "nobox_column_widen"),
        "1\n12\n34\n-1\n56\n12\n34\n-1\n-1\n78"
    );
}

#[test]
fn nobox_interpolation_does_not_materialize() {
    let lua = emit_module(NOBOX_WHOLE_VALUE_OPERATORS);
    let body = function_body(&lua, "render");
    assert!(!body.contains("NV2_ct("), "nobox interpolation materialized its operand:\n{body}");
}

// A shape that still materializes, so the gate below stays a gate. Twice
// re-pointed now: an indirect `sink(v)` call started threading leaves
// (`fn_value_abi`), then a tuple pushed into a collection started riding the box
// as leaves. Both were OPEN GAPS when chosen, which is what made them rot.
//
// Interpolating a whole tuple is chosen because it is PERMANENT -- asking for one
// runtime-typed value is F1 erasure, a justified box meant to stay a rejection
// (`tests/errors/nobox_container_gaps.rock.err`). Do not re-point this at a sink
// the case matrix lists as live.
const NOBOX_INDIRECT_SINK: &str = r#"
nobox value NV2 { x: float, y: float }

fn bad(v: NV2) -> int {
    let t = (v, 1.0)
    print("{t}")
    return 0
}

fn main() {
    print(bad(NV2 { x: 1.0, y: 2.0 }))
}
"#;

#[test]
fn post_transform_gate_rejects_an_implicit_nobox_box() {
    let err = try_emit_module(NOBOX_INDIRECT_SINK)
        .expect_err("an indirect call must not implicitly materialize a nobox value");
    assert!(err.message.contains("nobox") && err.message.contains("NV2"), "{err:?}");
}

const NOBOX_COLUMN_INSPECT: &str = r#"
nobox value NV2 { x: float, y: float }

fn bad() {
    let xs: [NV2] = [NV2 { x: 1.0, y: 2.0 }]
    let _ = inspect(xs)
}

fn main() {
    bad()
}
"#;

#[test]
fn column_inspect_rejects_implicit_nobox_element_materialization() {
    let err = try_emit_module(NOBOX_COLUMN_INSPECT)
        .expect_err("inspect must not materialize nobox column elements");
    assert!(
        err.message.contains("nobox") && err.message.contains("inspect"),
        "{err:?}"
    );
}

const NOBOX_COLUMN_LUA_ARRAY: &str = r#"
nobox value NV2 { x: float, y: float }

fn bad() {
    let xs: [NV2] = [NV2 { x: 1.0, y: 2.0 }]
    let _ = __rock_vec_to_lua_array(xs)
}

fn main() {
    bad()
}
"#;

#[test]
fn column_lua_array_bridge_rejects_implicit_nobox_element_materialization() {
    let err = try_emit_module(NOBOX_COLUMN_LUA_ARRAY)
        .expect_err("the Lua-array bridge must not materialize nobox column elements");
    assert!(
        err.message.contains("nobox") && err.message.contains("Lua array"),
        "{err:?}"
    );
}

const NOBOX_CALL_RETURN: &str = r#"
nobox value NV2 { x: float, y: float }

fn bounce(v: NV2) -> NV2 {
    return v
}

fn use(v: NV2) -> float {
    let next = bounce(v)
    return next.x + next.y
}

fn main() {
    let _ = use(NV2 { x: 1.0, y: 2.0 })
}
"#;

#[test]
fn direct_call_and_return_thread_nobox_leaves() {
    let lua = emit_module(NOBOX_CALL_RETURN);
    for name in ["bounce", "use"] {
        let body = function_body(&lua, name);
        assert!(
            !body.contains("NV2_ct("),
            "`{name}` materialized a nobox call or return:\n{body}"
        );
    }
}

const NOBOX_ANY_SINK: &str = r#"
nobox value NV2 { x: float, y: float }

fn erase(v: any) {}

fn bad(v: NV2) {
    erase(v)
}

fn main() {}
"#;

#[test]
fn any_sink_rejects_at_the_argument() {
    let err = try_emit_module(NOBOX_ANY_SINK).expect_err("an any sink must reject");
    assert!(err.message.contains("nobox") && err.message.contains("erase"), "{err:?}");
    let span = err.labels.first().expect("sink error has a label").span;
    let range = span.offset_range;
    assert_eq!(&NOBOX_ANY_SINK[range.0..range.1], "v");
}

const NOBOX_GENERIC_SINK: &str = r#"
nobox value NV2 { x: float, y: float }

extern fn generic_sink<T>(v: T)

fn bad(v: NV2) {
    generic_sink(v)
}

fn main() {}
"#;

#[test]
fn unspecialized_generic_sink_rejects() {
    let err =
        try_emit_module(NOBOX_GENERIC_SINK).expect_err("an unspecialized generic sink must reject");
    assert!(
        err.message.contains("nobox") && err.message.contains("generic_sink"),
        "{err:?}"
    );
}

const NOBOX_EXTERN_SINK: &str = r#"
nobox value NV2 { x: float, y: float }

extern fn foreign(v: NV2)

fn bad(v: NV2) {
    foreign(v)
}

fn main() {}
"#;

#[test]
fn extern_sink_rejects() {
    let err = try_emit_module(NOBOX_EXTERN_SINK).expect_err("an extern sink must reject");
    assert!(
        err.message.contains("nobox") && err.message.contains("foreign"),
        "{err:?}"
    );
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
    let start = lua.find("function capture").expect("capture definition");
    let end = lua[start..]
        .find("\nfunction main")
        .map(|offset| start + offset)
        .expect("main follows capture and its hoist");
    let bodies = &lua[start..end];
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
        body.contains(r#"["@vec"] = {repr = "columns", elem = "NV2""#),
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
    let init = function_body(&lua, "__skull_ginit_SPOT");
    assert!(
        init.contains("do return (1.0), (2.0) end"),
        "the initializer did not return leaves:\n{init}"
    );
    let slots = global_session_slots(&lua, "SPOT");
    assert_eq!(slots.len(), 2, "the nobox global must own two Session leaves");
    assert!(
        slots.iter().all(|slot| lua.contains(&format!("__rock_active_session.__rock_state[{slot}]"))),
        "the global leaves were not bound through the active Session:\n{lua}"
    );
    for f in ["read", "main"] {
        let body = function_body(&lua, f);
        assert!(
            !body.contains("NV2_ct("),
            "`{f}` materialized the nobox global:\n{body}"
        );
    }
}

#[test]
fn nobox_global_reads_and_writes_leaves_at_runtime() {
    let lua = emit_module(NOBOX_GLOBAL);
    let out = run_luajit(&format!("{lua}\nmain()\n"), "nobox_global");
    assert_eq!(out, "3\n12\n300");
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
    assert!(err.message.contains("nobox") && err.message.contains("SV2"), "{err:?}");
}

const NOBOX_GLOBAL_INIT_ORDER: &str = r#"
nobox value NV2 { x: float, y: float }

global DERIVED: NV2 = NV2 { x: BASE.x + 10.0, y: BASE.y + 10.0 }
global BASE: NV2 = NV2 { x: 1.0, y: 2.0 }

fn main() {
    print("{DERIVED.x} {DERIVED.y}")
}
"#;

#[test]
fn a_nobox_global_read_orders_its_dependent_initializer() {
    let lua = emit_module(NOBOX_GLOBAL_INIT_ORDER);
    let base_slot = global_session_slots(&lua, "BASE")[0];
    let derived_slot = global_session_slots(&lua, "DERIVED")[0];
    let base = lua
        .find(&format!("if not __rock_state_has({base_slot})"))
        .expect("BASE Session init line");
    let derived = lua
        .find(&format!("if not __rock_state_has({derived_slot})"))
        .expect("DERIVED Session init line");
    assert!(base < derived, "DERIVED initialized before the BASE it reads:\n{lua}");
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "nobox_global_order"),
        "11 12"
    );
}

const NOBOX_NULLABLE_GLOBAL: &str = r#"
nobox value NV2 { x: float, y: float }

global MAYBE: NV2? = NV2 { x: 3.0, y: 4.0 }

fn probe() -> float {
    if MAYBE != null {
        return MAYBE.x + MAYBE.y
    }
    return -1.0
}

fn main() {
    print("{probe()}")
    MAYBE = null
    print("{probe()}")
    MAYBE = NV2 { x: 1.0, y: 1.0 }
    print("{probe()}")
}
"#;

#[test]
fn a_refined_nullable_nobox_global_addresses_the_right_leaves() {
    // A refined read's expr type is `NV2`, whose 2-leaf shape would put `x` on
    // the presence leaf. Layout follows the DECLARED type, so it must not.
    let lua = emit_module(NOBOX_NULLABLE_GLOBAL);
    assert_eq!(
        global_session_slots(&lua, "MAYBE").len(),
        3,
        "a nullable nobox global must reserve Session presence/data leaves"
    );
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "nobox_global_nullable"),
        "7\n-1\n2"
    );
}

const NOBOX_NESTED_FN: &str = r#"
nobox value NV2 { x: float, y: float }

fn outer(p: NV2, k: float) -> float {
    fn scale(v: NV2, by: float) -> NV2 {
        return NV2 { x: v.x * by, y: v.y * by }
    }
    fn sum(v: NV2) -> float { return v.x + v.y }
    let s = scale(p, k)
    return sum(s) + sum(scale(s, 2.0))
}

fn main() {
    print("{outer(NV2 { x: 1.0, y: 2.0 }, 3.0)}")
}
"#;

#[test]
fn a_nested_fn_threads_nobox_leaves_through_params_and_return() {
    let lua = emit_module(NOBOX_NESTED_FN);
    let bodies = [
        function_body(&lua, "outer"),
        body_of_synthetic(&lua, "_scale_"),
        body_of_synthetic(&lua, "_sum_"),
    ];
    for body in bodies {
        assert!(
            !body.contains("NV2_ct("),
            "a nobox value materialized at a nested-fn boundary:\n{body}"
        );
    }
    assert_eq!(run_luajit(&format!("{lua}\nmain()\n"), "nobox_nested_fn"), "27");
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
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "nobox_nested_fn_inferred"),
        "3"
    );
}

const VALUE_NESTED_FN_BY_VALUE: &str = r#"
value V2 { x: float, y: float }

fn apply(f: fn(V2) -> float, v: V2) -> float { return f(v) }

fn outer(p: V2) -> float {
    fn sum(v: V2) -> float { return v.x + v.y }
    return sum(p) + apply(sum, p)
}

fn main() {
    print("{outer(V2 { x: 1.0, y: 2.0 })}")
}
"#;

#[test]
fn a_nested_fn_taken_by_value_keeps_one_boxed_abi_at_every_call() {
    // The scalar ABI is all-or-nothing per callee: a nested fn reachable as a
    // fn VALUE cannot splat, so its direct calls must not splat either.
    let lua = emit_module(VALUE_NESTED_FN_BY_VALUE);
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "value_nested_fn_by_value"),
        "6"
    );
}

const NOBOX_SCALAR_ABI_FFI: &str = r#"
nobox value NV2 { x: float, y: float }

@scalar_abi inline(lua) fn ffi_take(p: NV2, k: float) -> float = "return p__x + p__y + k"
@scalar_abi inline(lua) fn ffi_make() -> NV2 = "return 3.0, 4.0"

fn main() {
    let v = ffi_make()
    print("{ffi_take(v, 1.0)} {ffi_take(k: 1.0, p: v)}")
}
"#;

/// `@scalar_abi` is how an `inline(lua)` FFI wrapper takes a nobox value without
/// materializing it -- the alternative at every call site is `materialize(v)`.
/// The whole imgui surface (`stone/engine/ui/imgui.rock`) rests on this working
/// in BOTH directions, and on named args reaching the right leaf group.
#[test]
fn a_scalar_abi_ffi_wrapper_threads_nobox_leaves_both_ways() {
    let lua = emit_module(NOBOX_SCALAR_ABI_FFI);
    let body = function_body(&lua, "main");
    assert!(
        !body.contains("NV2_ct("),
        "a scalar-ABI FFI boundary materialized a nobox value:\n{body}"
    );
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "nobox_scalar_abi_ffi"),
        "8 8"
    );
}

const NOBOX_NESTED_FN_BY_VALUE: &str = r#"
nobox value NV2 { x: float, y: float }

fn apply(f: fn(NV2) -> float, v: NV2) -> float { return f(v) }

fn outer(p: NV2) -> float {
    fn sum(v: NV2) -> float { return v.x + v.y }
    return apply(sum, p)
}

// The RETURN half of the same ABI: a lambda handing back a nobox value.
fn arc(r: float) -> (float) -> NV2 {
    t => NV2 { x: t * r, y: t * 2.0 }
}

fn main() {
    print(outer(NV2 { x: 1.0, y: 2.0 }))
    let stroke = arc(2.0)
    let q = stroke(3.0)
    print(q.x + q.y)
}
"#;

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
        body.contains("r0(r1, r2)"),
        "the indirect call must pass two leaves, not one box:\n{body}"
    );
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "nobox_fn_type_agrees"),
        "6\n3"
    );
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
    assert_eq!(params, 4, "float + two nobox leaves + one boxed value:\n{header}");
    assert!(!body.contains("NV2_ct("), "the top-level fn value materialized NV2:\n{body}");
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "nobox_toplevel_fn_value"),
        "10 14"
    );
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

const GPU_ONLY_TOPLEVEL_FN_BY_VALUE: &str = r#"
@intrinsic @gpu_only inline(lua) fn gpu_only(v: int) -> int = """error("GPU only")"""

fn apply(f: (int) -> int, v: int) -> int {
    return f(v)
}

fn main() {
    print("{apply(gpu_only, 3)}")
}
"#;

#[test]
fn a_gpu_only_fn_cannot_bypass_the_cpu_restriction_as_a_value() {
    let err = try_emit_module(GPU_ONLY_TOPLEVEL_FN_BY_VALUE)
        .expect_err("a GPU-only function value must reject in CPU code");
    assert!(
        err.message.contains("gpu_only")
            && err.message.contains("GPU-only")
            && err.message.contains("fn value"),
        "{err:?}"
    );
}

/// A `nobox` value crosses an INDIRECT call as leaves, both directions: a nested
/// fn taken by value receives them, and a lambda returns them through the
/// multi-return wire.
///
/// The ABI is read off the fn TYPE at both ends and nowhere else -- an indirect
/// call site cannot see its callee, so definition and call site agree only by
/// reading the same signature. `nested_fn_abi` used to drop a fn to the boxed
/// ABI the moment it was taken by value, which is precisely when the two ends
/// have to agree without seeing each other.
#[test]
fn a_nobox_value_crosses_a_fn_value_call_as_leaves() {
    let lua = emit_module(NOBOX_NESTED_FN_BY_VALUE);
    for name in ["apply", "outer", "arc"] {
        let body = function_body(&lua, name);
        assert!(
            !body.contains("NV2_ct("),
            "a fn-value boundary materialized in `{name}`:\n{body}"
        );
    }
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "nobox_fn_value"),
        "3\n12"
    );
}

const NOBOX_GLOBAL_PLACES: &str = r#"
nobox value NV2 { x: float, y: float }
nobox value Outer { a: NV2, n: float }

global O: Outer = Outer { a: NV2 { x: 1.0, y: 2.0 }, n: 3.0 }
global P: NV2 = NV2 { x: 0.0, y: 0.0 }

fn len2(v: NV2) -> float { return v.x * v.x + v.y * v.y }

fn main() {
    O.a = NV2 { x: 10.0, y: 20.0 }
    O.a.y = 99.0
    P.x += 5.0
    P.x++
    print("{O.a.x} {O.a.y} {O.n} {P.x} {P.y} {len2(O.a)}")
    let copy = O.a
    O.a.x = 0.0
    print("{copy.x} {O.a.x}")
}
"#;

#[test]
fn nobox_global_places_reach_the_right_leaves() {
    let lua = emit_module(NOBOX_GLOBAL_PLACES);
    let body = function_body(&lua, "main");
    for ct in ["NV2_ct(", "Outer_ct("] {
        assert!(!body.contains(ct), "a nobox global place materialized via {ct}:\n{body}");
    }
    // Nested whole-value write, nested leaf write, leaf compound assign, leaf
    // inc/dec, whole-value read into a call, and the copy-on-read rule.
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "nobox_global_places"),
        "10 99 3 6 0 9901\n10 0"
    );
}

const NOBOX_THROUGH_GENERIC_NULLABLE: &str = r#"
nobox value NV2 { x: float, y: float }
value BV2 { x: float, y: float }

fn nobox_sum(v: NV2?) -> float {
    let u = unwrap(v)
    return u.x + u.y
}

fn boxed_sum(v: BV2?) -> float {
    let u = unwrap(v)
    return u.x + u.y
}

fn main() {
    print("{nobox_sum(NV2 { x: 1.0, y: 2.0 })} {boxed_sum(BV2 { x: 10.0, y: 20.0 })}")
}
"#;

#[test]
fn a_generic_nullable_param_threads_a_nobox_argument_as_leaves() {
    let lua = emit_module(NOBOX_THROUGH_GENERIC_NULLABLE);
    let body = function_body(&lua, "nobox_sum");
    assert!(
        !body.contains("NV2_ct("),
        "a nobox value crossing `T?` materialized a cdata:\n{body}"
    );
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "nobox_generic_nullable"),
        "3 30"
    );
}

#[test]
fn a_generic_nullable_param_keeps_the_single_cdata_abi_for_an_unmarked_value() {
    let lua = emit_module(NOBOX_THROUGH_GENERIC_NULLABLE);
    let head = function_body(&lua, "boxed_sum").lines().next().expect("header").to_string();
    // cdata has no spare field for a nil discriminator, so an UNMARKED nullable
    // value struct stays one boxed param -- only `nobox` buys the presence leaf.
    assert!(
        head.ends_with("(c0)"),
        "an unmarked nullable value struct left the single-cdata ABI: {head}"
    );
}

const NOBOX_NULLABLE_BINDINGS: &str = r#"
nobox value NV2 { x: float, y: float }

fn coalesce(p: NV2?) -> float {
    let q = p ?? NV2 { x: 5.0, y: 6.0 }
    return q.x + q.y
}

fn if_let(p: NV2?) -> float {
    if let q = p { return q.x + q.y }
    return -1.0
}

fn main() {
    print("{coalesce(NV2 { x: 1.0, y: 2.0 })} {coalesce(null)} {if_let(NV2 { x: 3.0, y: 4.0 })} {if_let(null)}")
}
"#;

#[test]
fn null_coalescing_a_nobox_nullable_stays_on_leaves() {
    let lua = emit_module(NOBOX_NULLABLE_BINDINGS);
    let body = function_body(&lua, "coalesce");
    assert!(!body.contains("NV2_ct("), "`??` materialized a nobox cdata:\n{body}");
}

#[test]
fn if_let_binding_a_nobox_nullable_stays_on_leaves() {
    let lua = emit_module(NOBOX_NULLABLE_BINDINGS);
    let body = function_body(&lua, "if_let");
    assert!(!body.contains("NV2_ct("), "`if let` materialized a nobox cdata:\n{body}");
}

#[test]
fn nobox_nullable_bindings_evaluate_correctly() {
    let lua = emit_module(NOBOX_NULLABLE_BINDINGS);
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "nobox_nullable_bindings"),
        "3 11 7 -1"
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
    let err = try_emit_module(NOBOX_REJECTION_SITE)
        .expect_err("a boxed sink materializes the value");
    // Assert on the byte offset, not `line`/`col`: the three coordinate systems
    // in `docs/span-position-units.md` disagree on their base, and the offset is
    // the one that needs no conversion. The `fn` header is the anchor this used
    // to report, and a whole-fn anchor cannot tell two violations in one body
    // apart -- which is how one sink hid another inside `draw_fractures`.
    let start = NOBOX_REJECTION_SITE.find("    sink(NV2").expect("fixture line");
    let end = start + NOBOX_REJECTION_SITE[start..].find('\n').expect("fixture line ends");
    assert!(
        (start..=end).contains(&err.location.offset),
        "rejection did not anchor on the call that boxes: {err:?}"
    );
}

const PLAIN_VALUE_TUPLE_RETURN: &str = r#"
value V2 { x: float, y: float }

fn split(v: V2) -> (bool, V2) {
    return (true, v)
}

fn main() {
    let (ok, w) = split(V2 { x: 1.0, y: 2.0 })
    print("{ok} {w.x} {w.y}")
}
"#;

// The asymmetry guard for the nobox tuple-return ABI: an UNMARKED value struct
// keeps its single boxed tuple slot. Marked is a guarantee that changes the
// wire; unmarked is an optimization that must not silently re-shape a return an
// `@scalar_abi inline(lua)` body (`imgui_slider_float2`) hand-writes.
#[test]
fn a_non_nobox_value_in_a_tuple_return_keeps_the_boxed_abi() {
    let lua = emit_module(PLAIN_VALUE_TUPLE_RETURN);
    // Assert the WIRE, not just the presence of a box: a splatting return that
    // packs and re-splits still contains a `V2_ct(`, so only the caller's dest
    // count distinguishes the two ABIs.
    assert_eq!(
        multi_return_dests(&lua, "split"),
        2,
        "an unmarked value lost its single boxed tuple slot:\n{lua}"
    );
    assert_eq!(run_luajit(&format!("{lua}\nmain()\n"), "plain_value_tuple_return"), "true 1 2");
}

/// The `a, b = <fn>(..)` destination count at a multi-return call site -- the
/// tuple ABI's arity as the CALLER sees it.
fn multi_return_dests(lua: &str, fn_name: &str) -> usize {
    let call = format!("= {fn_name}_");
    let line = lua
        .lines()
        .find(|l| l.contains(&call))
        .unwrap_or_else(|| panic!("no multi-return call of `{fn_name}` in:\n{lua}"));
    line.split(" = ").next().expect("an assignment lhs").split(',').count()
}

const NOBOX_TUPLE_RETURN: &str = r#"
nobox value NV2 { x: float, y: float }

fn split(v: NV2) -> (bool, NV2) {
    return (true, v)
}

fn main() {
    let (ok, w) = split(NV2 { x: 1.0, y: 2.0 })
    print("{ok} {w.x} {w.y}")
}
"#;

#[test]
fn a_nobox_value_in_a_tuple_return_carries_its_leaves() {
    let lua = emit_module(NOBOX_TUPLE_RETURN);
    let body = function_body(&lua, "split");
    assert!(
        !body.contains("NV2_ct("),
        "a nobox tuple element materialized:\n{body}"
    );
    assert_eq!(
        multi_return_dests(&lua, "split"),
        3,
        "the flag plus two leaves must come back as three values:\n{lua}"
    );
    assert_eq!(run_luajit(&format!("{lua}\nmain()\n"), "nobox_tuple_return"), "true 1 2");
}

const NOBOX_PHI_CONTROLS: &str = r#"
nobox value NV2 { x: float, y: float }
enum Choice { A, B, C }

fn pick_two(right: bool) -> NV2 {
    let p = if right { NV2 { x: 3.0, y: 4.0 } } else { NV2 { x: 1.0, y: 2.0 } }
    return p
}

fn pick_flat(c: Choice) -> NV2 {
    let p = match c {
        Choice::A => NV2 { x: 5.0, y: 6.0 },
        Choice::B => NV2 { x: 7.0, y: 8.0 },
        Choice::C => NV2 { x: 9.0, y: 10.0 },
    }
    return p
}

fn main() {
    let a = pick_two(true)
    let b = pick_flat(Choice::B)
    print("{a.x} {a.y} {b.x} {b.y}")
}
"#;

#[test]
fn nobox_phi_controls_stay_unboxed() {
    let lua = emit_module(NOBOX_PHI_CONTROLS);
    for name in ["pick_two", "pick_flat"] {
        let body = function_body(&lua, name);
        assert!(!body.contains("NV2_ct("), "`{name}` materialized its result:\n{body}");
    }
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "nobox_phi_controls"),
        "3 4 7 8"
    );
}

const NOBOX_PHI_AGGREGATES: &str = r#"
nobox value NV2 { x: float, y: float }
enum Choice { A, B, C }

fn pick_nested(n: int) -> NV2 {
    let p = if n == 0 {
        NV2 { x: 1.0, y: 2.0 }
    } else if n == 1 {
        NV2 { x: 3.0, y: 4.0 }
    } else {
        NV2 { x: 5.0, y: 6.0 }
    }
    return p
}

fn pick_pair(c: Choice) -> (NV2, NV2) {
    return match c {
        Choice::A => (NV2 { x: 1.0, y: 2.0 }, NV2 { x: 3.0, y: 4.0 }),
        Choice::B => (NV2 { x: 7.0, y: 8.0 }, NV2 { x: 9.0, y: 10.0 }),
        Choice::C => (NV2 { x: 11.0, y: 12.0 }, NV2 { x: 13.0, y: 14.0 }),
    }
}

fn main() {
    let p = pick_nested(1)
    let (a, b) = pick_pair(Choice::B)
    print("{p.x} {p.y} {a.x} {a.y} {b.x} {b.y}")
}
"#;

#[test]
fn nobox_phi_joins_keep_aggregate_reprs() {
    let lua = emit_module(NOBOX_PHI_AGGREGATES);
    for name in ["pick_nested", "pick_pair"] {
        let body = function_body(&lua, name);
        assert!(!body.contains("NV2_ct("), "`{name}` materialized a nobox value:\n{body}");
    }
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "nobox_phi_aggregates"),
        "3 4 7 8 9 10"
    );
}

const NOBOX_OPAQUE_TUPLE_RETURN: &str = r#"
nobox value NV2 { x: float, y: float }

@nomangle inline(lua) fn foreign_pair() -> (bool, NV2) = """return true, NV2_ct(1.0, 2.0)"""

fn use_it() -> float {
    let (ok, v) = foreign_pair()
    if !ok { return -1.0 }
    return v.x + v.y
}

fn main() {
    print("{use_it()}")
}
"#;

// The return-position twin of `nobox_value_rejects_an_inline_lua_sink`: the
// hand-written body writes its own `return`, so it cannot carry leaves. Without
// the rejection the call site read a leaf the body never returned and the
// program faulted at runtime.
#[test]
fn nobox_tuple_return_rejects_an_opaque_callee() {
    let err = try_emit_module(NOBOX_OPAQUE_TUPLE_RETURN)
        .expect_err("an inline(lua) tuple return cannot carry nobox leaves");
    assert!(
        err.message.contains("nobox") && err.message.contains("foreign_pair"),
        "{err:?}"
    );
}

const NOBOX_SCALAR_ABI_TUPLE_RETURN: &str = r#"
nobox value NV2 { x: float, y: float }

@scalar_abi @nomangle inline(lua) fn foreign_pair() -> (bool, NV2) = """return true, 1.0, 2.0"""

fn use_it() -> float {
    let (ok, v) = foreign_pair()
    if !ok { return -1.0 }
    return v.x + v.y
}

fn main() {
    print("{use_it()}")
}
"#;

#[test]
fn a_scalar_abi_inline_lua_tuple_return_carries_leaves() {
    let lua = emit_module(NOBOX_SCALAR_ABI_TUPLE_RETURN);
    let body = function_body(&lua, "use_it");
    assert!(
        !body.contains("NV2_ct("),
        "an opted-in inline(lua) tuple return still boxed:\n{body}"
    );
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "nobox_scalar_abi_tuple_return"),
        "3"
    );
}

const NOBOX_LOOP_ELEMENT: &str = r#"
nobox value NV2 { x: float, y: float }

fn last_x(ps: [NV2]) -> float {
    let seen: NV2? = null
    for p in ps { seen = p }
    if let q = seen { return q.x }
    return -1.0
}

fn main() {
    print("{last_x([NV2 { x: 1.0, y: 2.0 }, NV2 { x: 3.0, y: 4.0 }])} {last_x([])}")
}
"#;

// A `for` over a Vec goes through the user-iterator protocol, whose `next`
// returns `(bool, T)` -- so the loop element only stays unboxed if that tuple
// return carries T's leaves.
#[test]
fn a_loop_element_binds_a_nobox_value_by_leaves() {
    let lua = emit_module(NOBOX_LOOP_ELEMENT);
    let body = function_body(&lua, "last_x");
    assert!(
        !body.contains("NV2_ct("),
        "the iterator protocol boxed a nobox element:\n{body}"
    );
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "nobox_loop_element"),
        "3 -1"
    );
}
