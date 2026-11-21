use crate::common::emit_module;

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
