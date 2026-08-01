// Inner-loop integration tests: one linked binary instead of one per file.

mod stone_harness;

#[path = "arena_reuse_alloc_gate.rs"]
mod arena_reuse_alloc_gate;
#[path = "bone_incremental_timing.rs"]
mod bone_incremental_timing;
#[path = "completions_cli.rs"]
mod completions_cli;
#[path = "glsl_backend_ratchet.rs"]
mod glsl_backend_ratchet;
#[path = "glsl_runner_gate.rs"]
mod glsl_runner_gate;
#[path = "js_backend_externs_gate.rs"]
mod js_backend_externs_gate;
#[path = "js_backend_ratchet.rs"]
mod js_backend_ratchet;
#[path = "js_coroutines.rs"]
mod js_coroutines;
#[path = "js_run_gate.rs"]
mod js_run_gate;
#[path = "js_value_tests.rs"]
mod js_value_tests;
#[path = "harness_coverage.rs"]
mod harness_coverage;
#[path = "json_serialize_tests.rs"]
mod json_serialize_tests;
#[path = "lint_cli.rs"]
mod lint_cli;
#[path = "lsp_panic_exit.rs"]
mod lsp_panic_exit;
#[path = "mangle_corpus_scan.rs"]
mod mangle_corpus_scan;
#[path = "metric_alloc_gate.rs"]
mod metric_alloc_gate;
#[path = "multi_error_surfacing.rs"]
mod multi_error_surfacing;
#[path = "parser_recovery_surfacing.rs"]
mod parser_recovery_surfacing;
#[path = "production_sources_test.rs"]
mod production_sources_test;
#[path = "shader_pipeline_cli.rs"]
mod shader_pipeline_cli;
#[path = "shader_validate_gate.rs"]
mod shader_validate_gate;
#[path = "stone_test_debounce.rs"]
mod stone_test_debounce;
#[path = "stone_test_decorator_cleanup.rs"]
mod stone_test_decorator_cleanup;
#[path = "stone_test_endtoend.rs"]
mod stone_test_endtoend;
#[path = "watcher_tmp_project_path.rs"]
mod watcher_tmp_project_path;
#[path = "web_build_gate.rs"]
mod web_build_gate;
#[path = "web_watch_tests.rs"]
mod web_watch_tests;
