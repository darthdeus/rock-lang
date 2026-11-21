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
