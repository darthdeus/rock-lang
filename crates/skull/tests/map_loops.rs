use crate::common::{emit_module, function_body};

#[test]
fn map_loops_use_generic_for_and_skip_unused_column_reads() {
    let lua = emit_module(include_str!(
        "../../../tests/vectors/map_loop_mutation.rock"
    ));
    assert!(function_body(&lua, "scalar_sum").contains(" in next, "));
    assert!(!function_body(&lua, "column_count").contains(".data"));
}
