//! `rock --backend js` integration tests: the node run/test executor over pure
//! Rock programs, plus the luajit controls the parity arms compare against.
#[path = "js_support/command.rs"]
mod command;
#[path = "js_support/js_backend_externs_gate.rs"]
mod js_backend_externs_gate;
#[path = "js_support/js_backend_externs_lua.rs"]
mod js_backend_externs_lua;
#[path = "js_support/js_coroutines.rs"]
mod js_coroutines;
#[path = "js_support/js_run_gate.rs"]
mod js_run_gate;
#[path = "js_support/js_test_harness_globals_lua.rs"]
mod js_test_harness_globals_lua;
#[path = "js_support/js_test_harness_globals_sentinel.rs"]
mod js_test_harness_globals_sentinel;
#[path = "js_support/js_value_tests.rs"]
mod js_value_tests;
#[path = "js_support/test_exit_status_gate_js.rs"]
mod test_exit_status_gate_js;
