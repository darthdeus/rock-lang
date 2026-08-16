// Inner-loop integration tests: one linked binary instead of one per file.

mod stone_harness;
#[path = "web_test_process.rs"]
mod web_test_process;

#[path = "ablation_census_survives_failure.rs"]
mod ablation_census_survives_failure;
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
#[path = "harness_coverage.rs"]
mod harness_coverage;
#[path = "js_backend_externs_gate.rs"]
mod js_backend_externs_gate;
#[path = "js_backend_ratchet.rs"]
mod js_backend_ratchet;
#[path = "js_coroutines.rs"]
mod js_coroutines;
#[path = "js_test_harness_globals_sentinel.rs"]
mod js_test_harness_globals_sentinel;
#[path = "comptime_build_vars.rs"]
mod comptime_build_vars;
#[path = "cimgui_prelude.rs"]
mod cimgui_prelude;
#[path = "release_mode_deleted.rs"]
mod release_mode_deleted;
#[path = "mag_build_vars.rs"]
mod mag_build_vars;
#[path = "package.rs"]
mod package;
#[path = "fire_manifest.rs"]
mod fire_manifest;
#[path = "js_run_gate.rs"]
mod js_run_gate;
#[path = "js_value_tests.rs"]
mod js_value_tests;
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
#[path = "nobox_diagnostic_attribution.rs"]
mod nobox_diagnostic_attribution;
#[path = "nobox_global_reflection_tests.rs"]
mod nobox_global_reflection_tests;
#[path = "parser_recovery_surfacing.rs"]
mod parser_recovery_surfacing;
#[path = "production_sources_test.rs"]
mod production_sources_test;
#[path = "asset_overlay_defaults.rs"]
mod asset_overlay_defaults;
#[path = "directory_listing_api.rs"]
mod directory_listing_api;
#[path = "run_exit_status.rs"]
mod run_exit_status;
#[path = "shader_pipeline_cli.rs"]
mod shader_pipeline_cli;
#[path = "shader_register_arity_gate.rs"]
mod shader_register_arity_gate;
#[path = "shader_validate_gate.rs"]
mod shader_validate_gate;
#[path = "shape_oracle.rs"]
mod shape_oracle;
#[path = "stone_test_comptime.rs"]
mod stone_test_comptime;
#[path = "stone_test_debounce.rs"]
mod stone_test_debounce;
#[path = "stone_test_decorator_cleanup.rs"]
mod stone_test_decorator_cleanup;
#[path = "stone_test_endtoend.rs"]
mod stone_test_endtoend;
#[path = "test_exit_status_gate.rs"]
mod test_exit_status_gate;
#[path = "test_harness_truncation.rs"]
mod test_harness_truncation;
#[path = "watcher_tmp_project_path.rs"]
mod watcher_tmp_project_path;
#[path = "imgui_web_stubs.rs"]
mod imgui_web_stubs;
#[path = "iter_parse_cli.rs"]
mod iter_parse_cli;
#[path = "mono_js_backend_realization.rs"]
mod mono_js_backend_realization;
#[path = "web_build_gate.rs"]
mod web_build_gate;
#[path = "web_driver_tests.rs"]
mod web_driver_tests;
#[path = "web_fake_gl_tests.rs"]
mod web_fake_gl_tests;
#[path = "web_package_tests.rs"]
mod web_package_tests;
#[path = "web_watch_tests.rs"]
mod web_watch_tests;
