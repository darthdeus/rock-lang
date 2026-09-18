// Game-spawning tests, excluded from the default nextest profile (see .config/nextest.toml).

mod stone_harness;

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
#[path = "artifact_fingerprint_replay_tests.rs"]
mod artifact_fingerprint_replay_tests;
#[path = "replay_bindings_tests.rs"]
mod replay_bindings_tests;
#[path = "action_layer_v2_landing_gaps.rs"]
mod action_layer_v2_landing_gaps;
#[path = "replay_restore_tests.rs"]
mod replay_restore_tests;
#[path = "replay_roundtrip_tests.rs"]
mod replay_roundtrip_tests;
#[path = "replay_bootstrap_tests.rs"]
mod replay_bootstrap_tests;
#[path = "replay_timeline_tests.rs"]
mod replay_timeline_tests;
#[path = "replay_window_tests.rs"]
mod replay_window_tests;
#[path = "replay_timing_tests.rs"]
mod replay_timing_tests;
#[path = "frame_error_gate.rs"]
mod frame_error_gate;
#[path = "allocation_activation_gate.rs"]
mod allocation_activation_gate;
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
#[path = "mag_worker_product_tests.rs"]
mod mag_worker_product_tests;
#[path = "sim_memory_cap.rs"]
mod sim_memory_cap;
#[path = "sim_reload_lane.rs"]
mod sim_reload_lane;
#[path = "sim_worker_pool.rs"]
mod sim_worker_pool;
#[path = "sim_observability_heavy.rs"]
mod sim_observability_heavy;
#[path = "shader_texture_census.rs"]
mod shader_texture_census;
#[path = "shader_corpus_oracle.rs"]
mod shader_corpus_oracle;

#[path = "graph_update_projects.rs"]
mod graph_update_projects;

#[path = "sim_presentation_differential.rs"]
mod sim_presentation_differential;
