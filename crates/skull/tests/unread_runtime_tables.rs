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
