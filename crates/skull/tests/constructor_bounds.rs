use crate::common::{emit_module, function_body};

#[test]
fn native_buffer_constructor_checks_follow_proven_capacity() {
    let lua = emit_module(include_str!(
        "../../../tests/native_buffers/native_buffer_constructor_bounds.rock"
    ));
    for name in [
        "fill_int",
        "fill_u8",
        "fill_alias",
        "fill_product",
        "reuse_buffer",
        "reuse_reversed",
        "reuse_unequal",
        "reuse_negated",
        "reuse_rows",
    ] {
        let body = function_body(&lua, name);
        assert!(!body.contains("__nb_checked"), "{name}:\n{body}");
    }
    for name in [
        "changed_alias",
        "changed_shrink",
        "changed_count",
        "changed_length",
        "changed_size",
        "old_buffer",
        "product_width",
        "product_height",
        "product_conditional",
        "product_shrink",
        "old_product",
        "reuse_wrong_condition",
        "reuse_wrong_capacity",
        "reuse_shrunk",
        "reuse_longer_loop",
        "stale_reuse",
    ] {
        let body = function_body(&lua, name);
        assert!(body.contains("__nb_checked"), "{name}:\n{body}");
    }
    assert!(function_body(&lua, "copy_mask").contains("__nb_checked_set"));
}
