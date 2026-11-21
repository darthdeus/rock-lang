mod affine_bounds;
mod common;
mod compiler_admission;
mod constructor_bounds;
mod private_records;
mod private_vectors;
mod sort_key;

mod copy_code_refs;
mod nested_strided_inspect;

mod map_empty_snapshots;
mod map_loops;
mod opaque_effects;

mod cdata_leaf_projection;

mod allocation_report;
mod late_float;

mod nested_box_boundary;

mod arena_loop_reads;
mod bounded_standalone_emission;
mod callee_binding;
mod capture_free_hoist;
mod clone_into_immutable;
mod closure_snapshot;
mod code_identity_artifact;
mod concrete_expansion;
mod const_hot_reload;
mod coroutine_frame_writes;
mod coroutine_hook_hoist;
mod coroutine_trace_witness;
mod defer_unwind;
mod descriptor_value_repr;
mod embed_scalarize;
mod enum_cdata;
mod enum_payload_sroa;
mod enum_scalarize;
mod expansion_admission;
mod extern_return_representation;
mod extern_value_return_ownership;
mod flat_element_ref_mutation;
mod flat_enum_match_passthrough;
mod flat_vec_value_shape;
mod fn_record_mt;
mod frame_lambda_hoist;
mod incremental_emit;
mod lua_definition_scopes;
mod lua_driver_execution;
mod mixed_enum_boxing;
mod nomangle_type_symbol;
mod opaque_declared_type_reach;
mod optional_entity_vec;
mod v1_enum_join_scalarize;

mod opaque_container_axes;

mod repr_opaque_param;

mod vec_push_column_route;

mod inline_body_column_map_param;

mod inline_lua_column_container_param_sentinel;
mod inline_value_specialization;

mod generated_glue_roots;

mod dispatched_sites;
mod identity_storage_instances;
mod instance_owned_emission;
mod relocation;
mod site_join;

mod shader_artifact;

mod comptime_inputs;
mod global_storage;
mod int_float_alias;
mod lambda_static;
mod line_directives;
mod loop_bytecode;
mod loop_scoped_locals;
mod mangle_rename_hot_reload;
mod modules_lifecycle;
mod modules_state_identity;
mod modules_type_emission;
mod multi_rejection;
mod native_buffer_alloc;
mod native_buffer_element_admission;
mod native_buffer_leaf_addressing;
mod nil_kill_scope;
mod nobox_column_threshold;
mod nobox_generic_return_boundary_sentinel;
mod nobox_materialize_gc_tier;
mod nobox_nested_ctor;
mod nobox_values;
mod nobox_wide_strided;
mod nullable_embed_clear;
mod nullable_value_join_sroa;
mod optional_value_wire;
mod preflight;
mod program_roots;
mod removed_global_markers;
mod shader_pipeline_register;
mod state_global_bridge;
mod static_hot_reload;
mod tail_calls;
mod tuple_scalarize;
mod uniform_block_pack;
mod unread_runtime_tables;
mod user_fn_emit_rejection;
mod value_leaf_layout;
mod value_scalarize;
mod value_struct_eq_dispatch;
mod value_test_discovery;
mod vec_flat_enum_strided;
mod vec_value_columns;

mod wide_value_abi;
mod wide_value_slot_budget;

mod projection_demand;

mod return_kills;

mod argument_demand;

mod fresh_struct_projection;

mod leaf_table_projection;

mod lua_syntax_level_budget;

mod json_default_factories;
