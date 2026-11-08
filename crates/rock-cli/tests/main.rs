// One harness avoids linking the compiler into a separate binary per test file.

mod backend_stats_summary;
mod fix_imports;
mod gpu_process;
mod guide;
mod lsp_code_action;
mod metric_add_window;
mod modules_display_paths;
mod no_stdlib;
mod package_test_program_shape;
mod plain_process;
mod stone_program_identity;

// `rock --backend js` integration tests: the node run/test executor over pure
// Rock programs, plus the luajit controls the parity arms compare against.
#[cfg(feature = "backend-js")]
#[path = "js_support/command.rs"]
mod command;
#[cfg(feature = "backend-js")]
#[path = "js_support/js_backend_externs_gate.rs"]
mod js_backend_externs_gate;
#[cfg(feature = "backend-js")]
#[path = "js_support/js_backend_externs_lua.rs"]
mod js_backend_externs_lua;
#[cfg(feature = "backend-js")]
#[path = "js_support/js_coroutines.rs"]
mod js_coroutines;
#[cfg(feature = "backend-js")]
#[path = "js_support/js_run_gate.rs"]
mod js_run_gate;
#[cfg(feature = "backend-js")]
#[path = "js_support/js_test_harness_globals_lua.rs"]
mod js_test_harness_globals_lua;
#[cfg(feature = "backend-js")]
#[path = "js_support/js_test_harness_globals_sentinel.rs"]
mod js_test_harness_globals_sentinel;
#[cfg(feature = "backend-js")]
#[path = "js_support/js_value_tests.rs"]
mod js_value_tests;
#[cfg(feature = "backend-js")]
#[path = "js_support/param_default_explicit_null.rs"]
mod param_default_explicit_null;
#[cfg(feature = "backend-js")]
#[path = "js_support/test_backends.rs"]
mod test_backends;
#[cfg(feature = "backend-js")]
#[path = "js_support/test_exit_status_gate_js.rs"]
mod test_exit_status_gate_js;
