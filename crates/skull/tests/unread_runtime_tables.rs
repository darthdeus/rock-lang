use crate::common::{emit_module, run_main};

#[test]
fn only_stringified_payload_free_enums_get_a_names_table() {
    let lua = emit_module(
        r#"
enum Shown { Alpha, Beta }
enum Hidden { Gamma, Delta }
fn pick(n: int) -> Hidden { if n > 0 { return Hidden::Gamma } return Hidden::Delta }
fn main() {
    let s = Shown::Beta
    print("{s}")
    print(pick(1) == Hidden::Gamma)
}
"#,
    );
    assert!(lua.contains("_names = { [0] = \"Shown::Alpha\""));
    assert!(!lua.contains("Hidden::Gamma"));
    assert_eq!(run_main(&lua), "Shown::Beta\ntrue");
}

#[test]
fn only_called_leaf_ctors_are_defined() {
    let lua = emit_module(
        r#"
nobox value NV2 { x: float, y: float }
value Used { pos: NV2? }
value Spare { pos: NV2? }
global G: Used = Used { pos: NV2 { x: 1.0, y: 2.0 } }
fn main() { print("{G.pos?.x ?? -1.0}") }
"#,
    );
    let defined = |sym: &str| lua.contains(&format!("\n{sym}_lct = function("));
    let called = |sym: &str| lua.contains(&format!("{sym}_lct("));
    assert!(called("Used") && defined("Used"));
    assert!(!called("Spare") && !defined("Spare"));
    assert!(!called("Mat4") && !defined("Mat4"));
    assert_eq!(run_main(&lua), "1");
}

#[test]
fn nobox_global_accessors_keep_their_leaf_ctor() {
    let lua = emit_module(
        r#"
nobox value SpaceId { value: int }
nobox value Place { space: SpaceId, x: int, y: int }
global HOVERED: Place? = null
fn main() { print(HOVERED == null) }
"#,
    );
    assert!(lua.contains("\nPlace_lct = function("));
}

#[test]
fn only_called_gc_value_ctors_are_defined() {
    let lua = emit_module(
        r#"
value Named { name: string, n: int }
value Spare { label: string, m: int }
global G: Named = Named { name: "a", n: 1 }
fn main() { print("{G.name}{G.n}") }
"#,
    );
    let defined = |sym: &str| lua.contains(&format!("\n{sym}_vt = function("));
    let called = |sym: &str| lua.contains(&format!("{sym}_vt("));
    assert_eq!(called("Named"), defined("Named"));
    assert!(!called("Spare") && !defined("Spare"));
    assert_eq!(run_main(&lua), "a1");
}
