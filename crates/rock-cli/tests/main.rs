// One harness avoids linking the compiler into a separate binary per test file.

mod backend_stats_summary;
mod broken_stdlib;
mod compile_budget;
mod gpu_process;
mod guide;
mod host_api;
mod lsp_code_action;
mod lua_body_limits;
mod no_stdlib;
mod package_relative_output;
mod package_test_program_shape;
mod perf_stats_json;
mod plain_process;
mod stone_program_identity;

// `rock --backend js` integration tests: the node run/test executor over pure
// Rock programs, plus the luajit controls the parity arms compare against.
#[cfg(feature = "backend-js")]
#[path = "js_support/command.rs"]
mod command;
#[cfg(feature = "backend-js")]
#[path = "js_support/js_deep_programs.rs"]
mod js_deep_programs;
#[cfg(feature = "backend-js")]
#[path = "js_support/js_run_gate.rs"]
mod js_run_gate;
#[cfg(feature = "backend-js")]
#[path = "js_support/test_backends.rs"]
mod test_backends;
#[cfg(feature = "backend-js")]
#[path = "js_support/test_exit_status_gate_js.rs"]
mod test_exit_status_gate_js;

mod native_commands;

mod stone_harness;
mod stone_test_comptime;
mod stone_test_debounce;
mod stone_test_decorator_cleanup;
mod stone_test_endtoend;

mod package_cli;
