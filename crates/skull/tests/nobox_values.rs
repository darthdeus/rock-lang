mod common;
use common::{
    body_of_synthetic, emit_module, function_body, run_luajit, try_emit_module,
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
    assert!(
        lua.contains("__nbg_SPOT_0, __nbg_SPOT_1 = __skull_ginit_SPOT()"),
        "the global was not bound leaf-wise:\n{lua}"
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

const NOBOX_STDLIB_SINK_LIB: &str = r#"
nobox value SV2 { x: float, y: float }

fn consume(v: SV2) {}

fn stdlib_bad(v: SV2, sink: fn(SV2)) {
    sink(v)
}

fn stdlib_entry() {
    stdlib_bad(SV2 { x: 1.0, y: 2.0 }, consume)
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
    let base = lua.find("__nbg_BASE_0, __nbg_BASE_1 =").expect("BASE init line");
    let derived = lua.find("__nbg_DERIVED_0, __nbg_DERIVED_1 =").expect("DERIVED init line");
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
    assert!(
        lua.contains("__nbg_MAYBE_0, __nbg_MAYBE_1, __nbg_MAYBE_2 ="),
        "a nullable nobox global did not reserve a presence leaf:\n{lua}"
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

fn main() {}
"#;

#[test]
fn a_nested_fn_taken_by_value_rejects_a_nobox_param() {
    let err = try_emit_module(NOBOX_NESTED_FN_BY_VALUE)
        .expect_err("an indirectly-reachable nested fn must not implicitly box a nobox param");
    assert!(err.message.contains("nobox"), "{err:?}");
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

fn last(ps: [NV2]) -> float {
    let seen: NV2? = null
    for p in ps { seen = p }
    if let q = seen { return q.x }
    return 0.0
}

fn main() { let _ = last([]) }
"#;

#[test]
fn a_nobox_rejection_anchors_on_the_offending_expression() {
    let err = try_emit_module(NOBOX_REJECTION_SITE)
        .expect_err("a whole-value use of a loop element boxes it");
    // Assert on the byte offset, not `line`/`col`: the three coordinate systems
    // in `docs/span-position-units.md` disagree on their base, and the offset is
    // the one that needs no conversion. The `fn` header is the anchor this used
    // to report, and a whole-fn anchor cannot tell two violations in one body
    // apart -- which is how one sink hid another inside `draw_fractures`.
    let start = NOBOX_REJECTION_SITE.find("    for p in ps").expect("fixture line");
    let end = start + NOBOX_REJECTION_SITE[start..].find('\n').expect("fixture line ends");
    assert!(
        (start..=end).contains(&err.location.offset),
        "rejection did not anchor on the assignment that boxes: {err:?}"
    );
}
