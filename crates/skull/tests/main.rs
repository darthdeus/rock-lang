// Single harness for this crate's integration tests: one linked binary
// instead of one per file. nextest still runs each test in its own
// process, so isolation is unchanged.

mod common;

mod copy_code_refs;
mod nested_strided_inspect;

mod map_empty_snapshots;
mod map_remove_absent;

#[path = "cdata_leaf_projection.rs"]
mod cdata_leaf_projection;

mod compile_release;

#[path = "allocation_report.rs"]
mod allocation_report;

mod nested_box_boundary;

#[path = "ablation_totality.rs"]
mod ablation_totality;
#[path = "arena_column_oracle.rs"]
mod arena_column_oracle;
#[path = "artifact_fingerprint.rs"]
mod artifact_fingerprint;
#[path = "backend_variant_mangling.rs"]
mod backend_variant_mangling;
#[path = "bounded_lambda_inline.rs"]
mod bounded_lambda_inline;
#[path = "bounded_standalone_emission.rs"]
mod bounded_standalone_emission;
#[path = "callee_binding.rs"]
mod callee_binding;
#[path = "capture_free_hoist.rs"]
mod capture_free_hoist;
#[path = "clone_into_immutable.rs"]
mod clone_into_immutable;
#[path = "closure_snapshot.rs"]
mod closure_snapshot;
#[path = "coalesce_value_struct_literal_field.rs"]
mod coalesce_value_struct_literal_field;
#[path = "code_identity_artifact.rs"]
mod code_identity_artifact;
#[path = "concrete_body.rs"]
mod concrete_body;
#[path = "concrete_coroutine_emission.rs"]
mod concrete_coroutine_emission;
#[path = "concrete_expansion.rs"]
mod concrete_expansion;
#[path = "const_hoist.rs"]
mod const_hoist;
#[path = "const_hot_reload.rs"]
mod const_hot_reload;
#[path = "coroutine_frame_writes.rs"]
mod coroutine_frame_writes;
#[path = "coroutine_hook_hoist.rs"]
mod coroutine_hook_hoist;
#[path = "dead_lambda_scope.rs"]
mod dead_lambda_scope;
#[path = "default_lambda_fields.rs"]
mod default_lambda_fields;
#[path = "defer_unwind.rs"]
mod defer_unwind;
#[path = "descriptor_value_repr.rs"]
mod descriptor_value_repr;
#[path = "embed_scalarize.rs"]
mod embed_scalarize;
#[path = "enum_cdata.rs"]
mod enum_cdata;
#[path = "enum_flat_equality.rs"]
mod enum_flat_equality;
#[path = "enum_flat_field_spread.rs"]
mod enum_flat_field_spread;
#[path = "enum_flat_nullable_format.rs"]
mod enum_flat_nullable_format;
#[path = "enum_format_packages.rs"]
mod enum_format_packages;
#[path = "enum_payload_sroa.rs"]
mod enum_payload_sroa;
#[path = "enum_scalarize.rs"]
mod enum_scalarize;
#[path = "env_param_shadow.rs"]
mod env_param_shadow;
#[path = "env_record_hoist.rs"]
mod env_record_hoist;
#[path = "expansion_admission.rs"]
mod expansion_admission;
#[path = "extern_return_representation.rs"]
mod extern_return_representation;
#[path = "extern_value_return_ownership.rs"]
mod extern_value_return_ownership;
#[path = "flat_element_ref_mutation.rs"]
mod flat_element_ref_mutation;
#[path = "flat_enum_match_passthrough.rs"]
mod flat_enum_match_passthrough;
#[path = "flat_vec_value_shape.rs"]
mod flat_vec_value_shape;
#[path = "fn_record_mt.rs"]
mod fn_record_mt;
#[path = "frame_lambda_hoist.rs"]
mod frame_lambda_hoist;
#[path = "frame_slot_naming.rs"]
mod frame_slot_naming;
#[path = "generic_coroutine_column_param.rs"]
mod generic_coroutine_column_param;
#[path = "hole_runtime_error.rs"]
mod hole_runtime_error;
#[path = "incremental_emit.rs"]
mod incremental_emit;
#[path = "lua_definition_scopes.rs"]
mod lua_definition_scopes;
#[path = "mixed_enum_boxing.rs"]
mod mixed_enum_boxing;
#[path = "nomangle_type_symbol.rs"]
mod nomangle_type_symbol;
#[path = "opaque_declared_type_reach.rs"]
mod opaque_declared_type_reach;
#[path = "optional_entity_vec.rs"]
mod optional_entity_vec;
#[path = "v1_enum_join_scalarize.rs"]
mod v1_enum_join_scalarize;

#[path = "opaque_container_axes.rs"]
mod opaque_container_axes;

#[path = "repr_opaque_param.rs"]
mod repr_opaque_param;

#[path = "vec_push_column_route.rs"]
mod vec_push_column_route;

#[path = "inline_body_column_map_param.rs"]
mod inline_body_column_map_param;

#[path = "graph_contributions.rs"]
mod graph_contributions;
#[path = "inline_lua_column_container_param_sentinel.rs"]
mod inline_lua_column_container_param_sentinel;
#[path = "inline_stats.rs"]
mod inline_stats;
#[path = "inline_value_specialization.rs"]
mod inline_value_specialization;
#[path = "live_call_conversion.rs"]
mod live_call_conversion;
#[path = "parallel_inline_mode.rs"]
mod parallel_inline_mode;

#[path = "foreign_fn_value_delivery.rs"]
mod foreign_fn_value_delivery;

#[path = "generated_glue_roots.rs"]
mod generated_glue_roots;

#[path = "graph_update.rs"]
mod graph_update;

#[path = "graph_snapshot.rs"]
mod graph_snapshot;

#[path = "artifact_view.rs"]
mod artifact_view;
#[path = "dispatched_sites.rs"]
mod dispatched_sites;
#[path = "identity_storage_instances.rs"]
mod identity_storage_instances;
#[path = "instance_graph.rs"]
mod instance_graph;
#[path = "instance_owned_emission.rs"]
mod instance_owned_emission;
#[path = "logical_instance_identity.rs"]
mod logical_instance_identity;
#[path = "relocation.rs"]
mod relocation;
#[path = "site_domain.rs"]
mod site_domain;
#[path = "site_join.rs"]
mod site_join;
#[path = "struct_default_generic_relation.rs"]
mod struct_default_generic_relation;

#[path = "shader_artifact.rs"]
mod shader_artifact;

#[path = "comptime_inputs.rs"]
mod comptime_inputs;
#[path = "global_layout.rs"]
mod global_layout;
#[path = "global_storage.rs"]
mod global_storage;
#[path = "int_float_alias.rs"]
mod int_float_alias;
#[path = "intrinsics.rs"]
mod intrinsics;
#[path = "lambda_static.rs"]
mod lambda_static;
#[path = "leaf_form_invariant.rs"]
mod leaf_form_invariant;
#[path = "line_directives.rs"]
mod line_directives;
#[path = "loop_scoped_locals.rs"]
mod loop_scoped_locals;
#[path = "mangle_rename_hot_reload.rs"]
mod mangle_rename_hot_reload;
#[path = "mem2reg_stats.rs"]
mod mem2reg_stats;
#[path = "modules_lifecycle.rs"]
mod modules_lifecycle;
#[path = "modules_state_identity.rs"]
mod modules_state_identity;
#[path = "modules_type_emission.rs"]
mod modules_type_emission;
#[path = "multi_rejection.rs"]
mod multi_rejection;
#[path = "native_buffer_alloc.rs"]
mod native_buffer_alloc;
#[path = "native_buffer_element_admission.rs"]
mod native_buffer_element_admission;
#[path = "native_buffer_leaf_addressing.rs"]
mod native_buffer_leaf_addressing;
#[path = "nb_storage_registry.rs"]
mod nb_storage_registry;
#[path = "nil_kill_scope.rs"]
mod nil_kill_scope;
#[path = "nobox_arena_random_sentinel.rs"]
mod nobox_arena_random_sentinel;
#[path = "nobox_column_threshold.rs"]
mod nobox_column_threshold;
#[path = "nobox_fn_value_nobox_return.rs"]
mod nobox_fn_value_nobox_return;
#[path = "nobox_fn_value_nullable_return_sentinel.rs"]
mod nobox_fn_value_nullable_return_sentinel;
#[path = "nobox_generic_return_boundary_sentinel.rs"]
mod nobox_generic_return_boundary_sentinel;
#[path = "nobox_global_accessor_shape.rs"]
mod nobox_global_accessor_shape;
#[path = "nobox_materialize_gc_tier.rs"]
mod nobox_materialize_gc_tier;
#[path = "nobox_nested_ctor.rs"]
mod nobox_nested_ctor;
#[path = "nobox_nullable_phi_return_sentinel.rs"]
mod nobox_nullable_phi_return_sentinel;
#[path = "nobox_tuple_call_spec_key.rs"]
mod nobox_tuple_call_spec_key;
#[path = "nobox_tuple_option_presence_sentinel.rs"]
mod nobox_tuple_option_presence_sentinel;
#[path = "nobox_values.rs"]
mod nobox_values;
#[path = "nobox_wide_strided.rs"]
mod nobox_wide_strided;
#[path = "nullable_embed_clear.rs"]
mod nullable_embed_clear;
#[path = "nullable_generic_binding_sroa.rs"]
mod nullable_generic_binding_sroa;
#[path = "nullable_value_in_tuple_sentinel.rs"]
mod nullable_value_in_tuple_sentinel;
#[path = "nullable_value_join_sroa.rs"]
mod nullable_value_join_sroa;
#[path = "optional_value_wire.rs"]
mod optional_value_wire;
#[path = "plain_struct_scalarize.rs"]
mod plain_struct_scalarize;
#[path = "removed_global_markers.rs"]
mod removed_global_markers;
#[path = "reserved_namespace.rs"]
mod reserved_namespace;
#[path = "shader_pipeline_register.rs"]
mod shader_pipeline_register;
#[path = "state_global_bridge.rs"]
mod state_global_bridge;
#[path = "static_hot_reload.rs"]
mod static_hot_reload;
#[path = "static_registration_coverage.rs"]
mod static_registration_coverage;
#[path = "struct_expand.rs"]
mod struct_expand;
#[path = "synthetic_name_collision.rs"]
mod synthetic_name_collision;
#[path = "tail_calls.rs"]
mod tail_calls;
#[path = "trace_frames.rs"]
mod trace_frames;
#[path = "tuple_scalarize.rs"]
mod tuple_scalarize;
#[path = "uniform_block_pack.rs"]
mod uniform_block_pack;
#[path = "user_fn_emit_rejection.rs"]
mod user_fn_emit_rejection;
#[path = "valid_type_identity.rs"]
mod valid_type_identity;
#[path = "value_ctor_splice.rs"]
mod value_ctor_splice;
#[path = "value_leaf_layout.rs"]
mod value_leaf_layout;
#[path = "value_push_generic_boundary.rs"]
mod value_push_generic_boundary;
#[path = "value_scalarize.rs"]
mod value_scalarize;
#[path = "value_struct_eq_dispatch.rs"]
mod value_struct_eq_dispatch;
#[path = "value_test_discovery.rs"]
mod value_test_discovery;
#[path = "vec_flat_enum_strided.rs"]
mod vec_flat_enum_strided;
#[path = "vec_value_columns.rs"]
mod vec_value_columns;
#[path = "vertex_plan.rs"]
mod vertex_plan;
#[path = "wgpu_prelude_hooks.rs"]
mod wgpu_prelude_hooks;
#[path = "wide_value_abi.rs"]
mod wide_value_abi;
#[path = "wide_value_slot_budget.rs"]
mod wide_value_slot_budget;

#[path = "condition_branches.rs"]
mod condition_branches;

#[path = "projection_demand.rs"]
mod projection_demand;

#[path = "result_destinations.rs"]
mod result_destinations;

mod argument_demand;
mod discarded_effects;
mod overlay_function_values;

mod optional_receiver_once;

mod fresh_struct_projection;

mod leaf_table_projection;

#[path = "lua_syntax_level_budget.rs"]
mod lua_syntax_level_budget;
mod nullable_scalar_projection;
