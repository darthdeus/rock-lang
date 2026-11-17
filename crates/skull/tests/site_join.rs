#[test]
fn spliced_iterator_headers_preserve_numeric_loop_emission() {
    let lua = crate::common::emit_module(include_str!("../../../tests/functions/site_calls.rock"));
    let caller = "iterator_headers_survive_splicing";
    let body = crate::common::function_body(&lua, caller);
    assert!(body.contains("for "));
    assert!(crate::common::callees_named_in(&lua, caller, "sum_inner").is_empty());
}
