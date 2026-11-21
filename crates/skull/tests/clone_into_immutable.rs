use crate::common::{emit_module, function_body};

#[test]
fn strided_clone_uses_bulk_copy_only_for_immutable_leaves() {
    let lua = emit_module(include_str!(
        "../../../tests/vectors/clone_into_storage.rock"
    ));
    for name in ["copy_wide", "copy_mixed"] {
        assert!(function_body(&lua, name).contains("__rock_stride_copy"));
    }
    for name in ["clone_list", "clone_bag", "clone_mutable_wide"] {
        assert!(function_body(&lua, name).contains("clone_into"));
    }
}
