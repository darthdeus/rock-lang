// Inner-loop integration tests: one linked binary instead of one per file.

mod stone_harness;

#[path = "ablation_census_survives_failure.rs"]
mod ablation_census_survives_failure;
#[path = "arena_reuse_alloc_gate.rs"]
mod arena_reuse_alloc_gate;
#[path = "allocation_jit_trace_gate.rs"]
mod allocation_jit_trace_gate;
#[path = "bone_incremental_timing.rs"]
mod bone_incremental_timing;
#[path = "completions_cli.rs"]
mod completions_cli;
#[path = "sound_inspect_cli.rs"]
mod sound_inspect_cli;
#[path = "glsl_backend_ratchet.rs"]
mod glsl_backend_ratchet;
#[path = "glsl_runner_gate.rs"]
mod glsl_runner_gate;
#[path = "harness_coverage.rs"]
mod harness_coverage;
#[path = "backend_feature_gates.rs"]
mod backend_feature_gates;
#[path = "mag_build_vars.rs"]
mod mag_build_vars;
#[path = "asset_overlay_defaults.rs"]
mod asset_overlay_defaults;
#[path = "cimgui_prelude.rs"]
mod cimgui_prelude;
#[path = "test_exit_status_gate.rs"]
mod test_exit_status_gate;
#[path = "js_test_harness_globals_lua.rs"]
mod js_test_harness_globals_lua;
#[path = "js_backend_externs_lua.rs"]
mod js_backend_externs_lua;
#[path = "comptime_build_vars_lua.rs"]
mod comptime_build_vars_lua;
#[path = "lua_run_gate.rs"]
mod lua_run_gate;
#[path = "shape_oracle_desktop.rs"]
mod shape_oracle_desktop;
#[path = "sample_game_c_build_vars.rs"]
mod sample_game_c_build_vars;
#[path = "release_mode_deleted.rs"]
mod release_mode_deleted;
#[path = "replay_report_tests.rs"]
mod replay_report_tests;
#[path = "replay_profile_tests.rs"]
mod replay_profile_tests;
#[path = "replay_profile_report_tests.rs"]
mod replay_profile_report_tests;
#[path = "package.rs"]
mod package;
#[path = "fire_manifest.rs"]
mod fire_manifest;
#[path = "c_run_gate.rs"]
mod c_run_gate;
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
#[path = "launcher_engine_commands.rs"]
mod launcher_engine_commands;
#[path = "mag_direct_state_authority.rs"]
mod mag_direct_state_authority;
#[path = "directory_listing_api.rs"]
mod directory_listing_api;
#[path = "editor_inspector_position.rs"]
mod editor_inspector_position;
#[path = "run_exit_status.rs"]
mod run_exit_status;
#[path = "shader_pipeline_cli.rs"]
mod shader_pipeline_cli;
#[path = "shader_validate_gate.rs"]
mod shader_validate_gate;
#[path = "stone_test_comptime.rs"]
mod stone_test_comptime;
#[path = "stone_test_debounce.rs"]
mod stone_test_debounce;
#[path = "stone_test_decorator_cleanup.rs"]
mod stone_test_decorator_cleanup;
#[path = "stone_test_endtoend.rs"]
mod stone_test_endtoend;
#[path = "test_harness_truncation.rs"]
mod test_harness_truncation;
#[path = "watcher_tmp_project_path.rs"]
mod watcher_tmp_project_path;
#[path = "iter_parse_cli.rs"]
mod iter_parse_cli;
#[path = "backend_stats_gate.rs"]
mod backend_stats_gate;
