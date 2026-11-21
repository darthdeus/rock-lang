use crate::common::{emit_module, function_body};

#[test]
fn iterators_load_only_live_leaves_and_keep_required_checks() {
    let lua = emit_module(include_str!(
        "../../../tests/arenas/arena_iterator_leaves.rock"
    ));
    for name in ["unused_entity", "unused_pair_entity", "unused_enum"] {
        let body = function_body(&lua, name);
        assert!(!body.contains(".generations"));
    }
    assert!(function_body(&lua, "used_entity").contains(".generations"));

    for name in ["unused_column", "unused_enum"] {
        let body = function_body(&lua, name);
        assert!(!body.contains(".data"));
        assert!(body.contains(".occupied"));
    }

    let column = function_body(&lua, "column_field");
    assert!(!column.contains(".y"));
    assert!(column.contains(".x)[(c"));
    assert!(!column.contains("__vec_checked_index"));
    let strided = function_body(&lua, "strided_field");
    assert!(!strided.contains("__vec_checked_index"));
    let unused = function_body(&lua, "unused_vec_element");
    assert!(unused.contains("__vec_checked_index"));
}
