// Single harness for this crate's integration tests: one linked binary
// instead of one per file. nextest still runs each test in its own
// process, so isolation is unchanged.

mod common;

mod copy_code_refs;
mod nested_strided_inspect;

mod map_empty_snapshots;
mod map_remove_absent;

mod cdata_leaf_projection;

mod compile_release;

mod allocation_report;

mod nested_box_boundary;

mod arena_column_oracle;
mod artifact_fingerprint;
mod backend_variant_mangling;
mod bounded_lambda_inline;
mod bounded_standalone_emission;
mod callee_binding;
mod capture_free_hoist;
mod clone_into_immutable;
mod closure_snapshot;
mod coalesce_value_struct_literal_field;
mod code_identity_artifact;
mod concrete_body;
mod concrete_coroutine_emission;
mod concrete_expansion;
mod const_hoist;
mod const_hot_reload;
mod coroutine_frame_writes;
mod coroutine_hook_hoist;
mod coroutine_trace_witness;
mod dead_lambda_scope;
mod default_lambda_fields;
mod defer_unwind;
mod descriptor_value_repr;
mod embed_scalarize;
mod enum_cdata;
mod enum_flat_equality;
mod enum_flat_field_spread;
mod enum_flat_nullable_format;
mod enum_format_packages;
mod enum_payload_sroa;
mod enum_scalarize;
mod env_param_shadow;
mod env_record_hoist;
mod expansion_admission;
mod extern_return_representation;
mod extern_value_return_ownership;
mod flat_element_ref_mutation;
mod flat_enum_match_passthrough;
mod flat_vec_value_shape;
mod fn_record_mt;
mod frame_lambda_hoist;
mod frame_slot_naming;
mod generic_coroutine_column_param;
mod hole_runtime_error;
mod incremental_emit;
mod lua_definition_scopes;
mod mixed_enum_boxing;
mod nomangle_type_symbol;
mod opaque_declared_type_reach;
mod optional_entity_vec;
mod v1_enum_join_scalarize;

mod opaque_container_axes;

mod repr_opaque_param;

mod vec_push_column_route;

mod inline_body_column_map_param;

mod graph_contributions;
mod inline_lua_column_container_param_sentinel;
mod inline_value_specialization;
mod live_call_conversion;
mod parallel_inline_mode;

mod foreign_fn_value_delivery;

mod generated_glue_roots;

mod graph_update;

mod graph_snapshot;

mod artifact_view;
mod dispatched_sites;
mod identity_storage_instances;
mod instance_graph;
mod instance_owned_emission;
mod logical_instance_identity;
mod relocation;
mod site_domain;
mod site_join;
mod struct_default_generic_relation;

mod shader_artifact;

mod comptime_inputs;
mod global_layout;
mod global_storage;
mod int_float_alias;
mod intrinsics;
mod lambda_static;
mod leaf_form_invariant;
mod line_directives;
mod loop_scoped_locals;
mod mangle_rename_hot_reload;
mod modules_lifecycle;
mod modules_state_identity;
mod modules_type_emission;
mod multi_rejection;
mod native_buffer_alloc;
mod native_buffer_element_admission;
mod native_buffer_leaf_addressing;
mod nb_storage_registry;
mod nil_kill_scope;
mod nobox_arena_random_sentinel;
mod nobox_column_threshold;
mod nobox_fn_value_nobox_return;
mod nobox_fn_value_nullable_return_sentinel;
mod nobox_generic_return_boundary_sentinel;
mod nobox_global_accessor_shape;
mod nobox_materialize_gc_tier;
mod nobox_nested_ctor;
mod nobox_nullable_phi_return_sentinel;
mod nobox_tuple_call_spec_key;
mod nobox_tuple_option_presence_sentinel;
mod nobox_values;
mod nobox_wide_strided;
mod nullable_embed_clear;
mod nullable_generic_binding_sroa;
mod nullable_value_in_tuple_sentinel;
mod nullable_value_join_sroa;
mod optional_value_wire;
mod plain_struct_scalarize;
mod removed_global_markers;
mod reserved_namespace;
mod shader_pipeline_register;
mod state_global_bridge;
mod static_hot_reload;
mod static_registration_coverage;
mod struct_expand;
mod synthetic_name_collision;
mod tail_calls;
mod trace_frames;
mod tuple_scalarize;
mod uniform_block_pack;
mod user_fn_emit_rejection;
mod valid_type_identity;
mod value_ctor;
mod value_leaf_layout;
mod value_push_generic_boundary;
mod value_scalarize;
mod value_struct_eq_dispatch;
mod value_test_discovery;
mod vec_flat_enum_strided;
mod vec_value_columns;
mod vertex_plan;

mod wide_value_abi;
mod wide_value_slot_budget;

mod condition_branches;

mod projection_demand;

mod result_destinations;

mod argument_demand;
mod discarded_effects;
mod overlay_function_values;

mod optional_receiver_once;

mod fresh_struct_projection;

mod leaf_table_projection;

mod lua_syntax_level_budget;
mod nullable_scalar_projection;
