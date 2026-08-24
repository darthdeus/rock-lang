// Game-spawning tests, excluded from the default nextest profile (see .config/nextest.toml).

mod stone_harness;
#[path = "web_test_process.rs"]
mod web_test_process;

#[path = "arena_serialization_tests.rs"]
mod arena_serialization_tests;

#[path = "game_snapshot_tests.rs"]
mod game_snapshot_tests;
#[path = "perf_json_format_test.rs"]
mod perf_json_format_test;
#[path = "quit_teardown_test.rs"]
mod quit_teardown_test;
#[path = "replay_desync_tests.rs"]
mod replay_desync_tests;
#[path = "replay_flight_tests.rs"]
mod replay_flight_tests;
#[path = "replay_bindings_tests.rs"]
mod replay_bindings_tests;
#[path = "action_layer_v2_landing_gaps.rs"]
mod action_layer_v2_landing_gaps;
#[path = "replay_restore_tests.rs"]
mod replay_restore_tests;
#[path = "replay_roundtrip_tests.rs"]
mod replay_roundtrip_tests;
#[path = "replay_timeline_tests.rs"]
mod replay_timeline_tests;
#[path = "replay_window_tests.rs"]
mod replay_window_tests;
#[path = "engine_cdylib_freshness_gate.rs"]
mod engine_cdylib_freshness_gate;

#[path = "frame_error_gate.rs"]
mod frame_error_gate;
#[path = "frame_watchdog_gate.rs"]
mod frame_watchdog_gate;
#[path = "macos_wgpu_lifecycle_gate.rs"]
mod macos_wgpu_lifecycle_gate;
#[path = "macos_wgpu_recovery_gate.rs"]
mod macos_wgpu_recovery_gate;
#[path = "bounded_summary_gate.rs"]
mod bounded_summary_gate;
#[path = "wgpu_refusal_gate.rs"]
mod wgpu_refusal_gate;
#[path = "wgpu_capture_gate.rs"]
mod wgpu_capture_gate;
#[path = "wgpu_capture_contract_gate.rs"]
mod wgpu_capture_contract_gate;
#[path = "wgpu_gameplay_origin_gate.rs"]
mod wgpu_gameplay_origin_gate;
#[path = "wgpu_menu_parity_gate.rs"]
mod wgpu_menu_parity_gate;
#[path = "rich_text_reject_gate.rs"]
mod rich_text_reject_gate;
#[path = "plain_text_reject_gate.rs"]
mod plain_text_reject_gate;
#[path = "shader_failure_gate.rs"]
mod shader_failure_gate;
#[path = "uniform_block_gate.rs"]
mod uniform_block_gate;
#[path = "texture_slot_gate.rs"]
mod texture_slot_gate;
#[path = "skull_overlay_wipe.rs"]
mod skull_overlay_wipe;
#[path = "sim_concurrent_invocations.rs"]
mod sim_concurrent_invocations;
#[path = "sim_allocation_census.rs"]
mod sim_allocation_census;
#[path = "sim_worker_differential.rs"]
mod sim_worker_differential;
#[path = "sim_reload_lane_drain.rs"]
mod sim_reload_lane_drain;
#[path = "sim_worker_pool.rs"]
mod sim_worker_pool;
#[path = "web_smoke_tests.rs"]
mod web_smoke_tests;

#[path = "shader_texture_census.rs"]
mod shader_texture_census;
#[path = "shader_corpus_oracle.rs"]
mod shader_corpus_oracle;

#[path = "graph_update_projects.rs"]
mod graph_update_projects;
