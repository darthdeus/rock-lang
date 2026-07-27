mod common;
use common::{emit_module, function_body, run_luajit, try_emit_module};

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

#[test]
fn nobox_interpolation_does_not_materialize() {
    let lua = emit_module(NOBOX_WHOLE_VALUE_OPERATORS);
    let body = function_body(&lua, "render");
    assert!(!body.contains("NV2_ct("), "nobox interpolation materialized its operand:\n{body}");
}

const NOBOX_INDIRECT_SINK: &str = r#"
nobox value NV2 { x: float, y: float }

fn consume(v: NV2) {}

fn bad(v: NV2, sink: fn(NV2)) {
    sink(v)
}

fn main() {
    bad(NV2 { x: 1.0, y: 2.0 }, consume)
}
"#;

#[test]
fn post_transform_gate_rejects_an_implicit_nobox_box() {
    let err = try_emit_module(NOBOX_INDIRECT_SINK)
        .expect_err("an indirect call must not implicitly materialize a nobox value");
    assert!(err.message.contains("nobox") && err.message.contains("NV2"), "{err:?}");
}

const NOBOX_COLUMN_SORT: &str = r#"
nobox value NV2 { x: float, y: float }

fn bad() {
    let xs: [NV2] = [
        NV2 { x: 2.0, y: 0.0 },
        NV2 { x: 1.0, y: 0.0 },
    ]
    xs.sort(v => v.x)
}

fn main() {
    bad()
}
"#;

#[test]
fn column_sort_rejects_implicit_nobox_element_materialization() {
    let err = try_emit_module(NOBOX_COLUMN_SORT)
        .expect_err("column sort must not materialize nobox elements for its key function");
    assert!(
        err.message.contains("nobox") && err.message.contains("sort"),
        "{err:?}"
    );
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
