use crate::common::{emit_module, function_body, run_main, try_emit_module};

const STRING_FIELD: &str = r#"
nobox value SRow { kind: int, label: string }
inline(lua) fn show(v: any) -> string = "return tostring(v.kind) .. v.label"
fn main() {
    let v = SRow { kind: 1, label: "a" }
    print(show(materialize(v)))
}
"#;

const STRING_FIELD_UNMARKED: &str = r#"
value SRow { kind: int, label: string }
inline(lua) fn show(v: any) -> string = "return tostring(v.kind) .. v.label"
fn main() {
    let v = SRow { kind: 1, label: "a" }
    print(show(v))
}
"#;

const ARRAY_FIELD: &str = r#"
nobox value ARow { id: int, xs: [int] }
inline(lua) fn show(v: any) -> string = "return tostring(v.id) .. ':' .. tostring(#v.xs._data) .. ':' .. tostring(v.xs._data[2])"
fn main() {
    let v = ARow { id: 1, xs: [10, 20, 30] }
    print(show(materialize(v)))
}
"#;

const LEAF_TABLE_TIER: &str = r#"
nobox value NV2 { x: float, y: float }
nobox value Opt { pos: NV2?, a: float }
inline(lua) fn show(v: any) -> string = "return tostring(v['pos']) .. ':' .. tostring(v['pos.x']) .. ':' .. tostring(v['pos.y']) .. ':' .. tostring(v.a)"
fn main() {
    let o = Opt { pos: NV2 { x: 1.0, y: 2.0 }, a: 3.0 }
    print(show(materialize(o)))
}
"#;

const IMPLICIT_BOX: &str = r#"
nobox value SRow { kind: int, label: string }
inline(lua) fn show(v: any) -> string = "return tostring(v.kind) .. v.label"
fn main() {
    let v = SRow { kind: 1, label: "a" }
    print(show(v))
}
"#;

const NESTED_CDATA_TIER: &str = r#"
nobox value NV2 { x: float, y: float }
nobox value Box2 { lo: NV2, hi: NV2 }
inline(lua) fn show(v: any) -> string = "return tostring(v.lo.x) .. ' ' .. tostring(v.hi.y)"
fn main() {
    let b = Box2 { lo: NV2 { x: 1.0, y: 2.0 }, hi: NV2 { x: 3.0, y: 4.0 } }
    print(show(materialize(b)))
}
"#;

#[test]
fn explicit_materialize_boxes_through_its_storage_tier_constructor() {
    for (src, ctor, expected) in [
        (STRING_FIELD, "SRow_vt(", "1a"),
        (ARRAY_FIELD, "ARow_vt(", "1:3:20"),
        (LEAF_TABLE_TIER, "Opt_lct(", "true:1:2:3"),
        (NESTED_CDATA_TIER, "Box2_lct(", "1 4"),
    ] {
        let lua = emit_module(src);
        let main = function_body(&lua, "main");
        assert!(main.contains(ctor) && !main.contains("NV2_ct("), "{main}");
        assert_eq!(run_main(&lua), expected);
    }
}

#[test]
fn the_marked_and_unmarked_boxes_are_the_same_shape() {
    let marked = emit_module(STRING_FIELD);
    let plain = emit_module(STRING_FIELD_UNMARKED);
    let ctor = |lua: &str| -> String {
        let body = function_body(lua, "main");
        let at = body.find("SRow_").expect("a SRow constructor in main");
        body[at..]
            .split('(')
            .next()
            .expect("a constructor name")
            .to_string()
    };
    assert_eq!(ctor(&marked), ctor(&plain));
    assert_eq!(run_main(&plain), run_main(&marked));
}

#[test]
fn an_implicit_box_of_a_gc_tier_value_is_still_rejected() {
    let err = try_emit_module(IMPLICIT_BOX)
        .expect_err("an implicit box of a GC-tier nobox value must still be rejected");
    assert!(err.message.contains("cannot cross boxed call boundary"));
    assert!(err.location.line > 0);
}

#[test]
fn live_implicit_materialization_in_a_hoisted_closure_is_rejected() {
    let source = r#"
nobox value NV2 { x: float, y: float }
fn with_check(check: fn() -> ()) { check() }
fn main() {
    with_check(() => {
        let t = (NV2 { x: 1.0, y: 2.0 }, 1.0)
        print("{t}")
    })
}
"#;
    let err = try_emit_module(source).expect_err("live closure boxes must still reject");
    assert!(
        err.message
            .contains("survived lowering as an implicit materialization")
    );
    assert!(err.message.contains("lambda"));
    assert!(err.location.line > 0);
}

#[test]
fn live_implicit_materialization_in_a_coroutine_hook_is_rejected() {
    let source = r#"
nobox value NV2 { x: float, y: float }
co fn worker() -> Coroutine<int> {
    on_killed(() => {
        let t = (NV2 { x: 1.0, y: 2.0 }, 1.0)
        print("{t}")
    })
    yield 1
}
fn main() {
    let c = worker()
    let _ = c.resume()
    c.kill()
}
"#;
    let err = try_emit_module(source).expect_err("live coroutine hook boxes must still reject");
    assert!(
        err.message
            .contains("survived lowering as an implicit materialization")
    );
    assert!(err.location.line > 0);
}
