// Single harness for this crate's integration tests: one linked binary
// instead of one per file. nextest still runs each test in its own
// process, so isolation is unchanged.

mod common;

#[path = "ablation_totality.rs"]
mod ablation_totality;
#[path = "backend_variant_mangling.rs"]
mod backend_variant_mangling;
#[path = "bounded_lambda_inline.rs"]
mod bounded_lambda_inline;
#[path = "capture_free_hoist.rs"]
mod capture_free_hoist;
#[path = "closure_snapshot.rs"]
mod closure_snapshot;
#[path = "coalesce_value_struct_literal_field_sentinel.rs"]
mod coalesce_value_struct_literal_field_sentinel;
#[path = "concrete_body.rs"]
mod concrete_body;
#[path = "const_hoist.rs"]
mod const_hoist;
#[path = "const_hot_reload.rs"]
mod const_hot_reload;
#[path = "coroutine_hook_hoist.rs"]
mod coroutine_hook_hoist;
#[path = "dead_lambda_scope.rs"]
mod dead_lambda_scope;
#[path = "embed_scalarize.rs"]
mod embed_scalarize;
#[path = "enum_cdata.rs"]
mod enum_cdata;
#[path = "enum_payload_sroa.rs"]
mod enum_payload_sroa;
#[path = "enum_scalarize.rs"]
mod enum_scalarize;
#[path = "env_param_shadow.rs"]
mod env_param_shadow;
#[path = "env_record_hoist.rs"]
mod env_record_hoist;
#[path = "extern_value_return_ownership.rs"]
mod extern_value_return_ownership;
#[path = "flat_vec_value_shape.rs"]
mod flat_vec_value_shape;
#[path = "fn_record_mt.rs"]
mod fn_record_mt;
#[path = "frame_lambda_hoist.rs"]
mod frame_lambda_hoist;
#[path = "frame_slot_naming.rs"]
mod frame_slot_naming;
#[path = "generic_coroutine_column_param_sentinel.rs"]
mod generic_coroutine_column_param_sentinel;
#[path = "hole_runtime_error.rs"]
mod hole_runtime_error;
#[path = "incremental_emit.rs"]
mod incremental_emit;
#[path = "opaque_declared_type_reach.rs"]
mod opaque_declared_type_reach;

#[path = "opaque_container_axes.rs"]
mod opaque_container_axes;

#[path = "repr_opaque_param.rs"]
mod repr_opaque_param;

#[path = "vec_push_column_route.rs"]
mod vec_push_column_route;

#[path = "inline_body_column_map_param.rs"]
mod inline_body_column_map_param;

#[path = "inline_lua_column_container_param_sentinel.rs"]
mod inline_lua_column_container_param_sentinel;
#[path = "inline_value_specialization.rs"]
mod inline_value_specialization;
#[path = "instance_graph.rs"]
mod instance_graph;
#[path = "instance_owned_emission.rs"]
mod instance_owned_emission;
#[path = "site_join.rs"]
mod site_join;
#[path = "dispatched_sites.rs"]
mod dispatched_sites;
#[path = "site_domain.rs"]
mod site_domain;
#[path = "struct_default_generic_relation.rs"]
mod struct_default_generic_relation;
#[path = "logical_instance_identity.rs"]
mod logical_instance_identity;
#[path = "artifact_view.rs"]
mod artifact_view;

#[path = "shader_artifact.rs"]
mod shader_artifact;

#[path = "intrinsics.rs"]
mod intrinsics;
#[path = "lambda_static.rs"]
mod lambda_static;
#[path = "leaf_form_invariant.rs"]
mod leaf_form_invariant;
#[path = "loop_scoped_locals.rs"]
mod loop_scoped_locals;
#[path = "mangle_rename_hot_reload.rs"]
mod mangle_rename_hot_reload;
#[path = "mir_execution_oracle.rs"]
mod mir_execution_oracle;
#[path = "multi_rejection.rs"]
mod multi_rejection;
#[path = "native_buffer_alloc.rs"]
mod native_buffer_alloc;
#[path = "native_buffer_leaf_addressing.rs"]
mod native_buffer_leaf_addressing;
#[path = "nobox_arena_random_sentinel.rs"]
mod nobox_arena_random_sentinel;
#[path = "nobox_column_threshold.rs"]
mod nobox_column_threshold;
#[path = "nobox_fn_value_nullable_return_sentinel.rs"]
mod nobox_fn_value_nullable_return_sentinel;
#[path = "nobox_global_accessor_shape.rs"]
mod nobox_global_accessor_shape;
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
#[path = "nullable_generic_binding_sroa.rs"]
mod nullable_generic_binding_sroa;
#[path = "nullable_value_in_tuple_sentinel.rs"]
mod nullable_value_in_tuple_sentinel;
#[path = "plain_struct_scalarize.rs"]
mod plain_struct_scalarize;
#[path = "reserved_namespace.rs"]
mod reserved_namespace;
#[path = "shader_pipeline_register.rs"]
mod shader_pipeline_register;
#[path = "static_hot_reload.rs"]
mod static_hot_reload;
#[path = "struct_expand.rs"]
mod struct_expand;
#[path = "synthetic_name_collision.rs"]
mod synthetic_name_collision;
#[path = "tuple_scalarize.rs"]
mod tuple_scalarize;
#[path = "uniform_block_pack.rs"]
mod uniform_block_pack;
#[path = "user_fn_emit_rejection.rs"]
mod user_fn_emit_rejection;
#[path = "value_leaf_layout.rs"]
mod value_leaf_layout;
#[path = "value_push_generic_boundary.rs"]
mod value_push_generic_boundary;
#[path = "value_scalarize.rs"]
mod value_scalarize;
#[path = "value_test_discovery.rs"]
mod value_test_discovery;
#[path = "valid_type_identity.rs"]
mod valid_type_identity;
#[path = "vec_value_columns.rs"]
mod vec_value_columns;
#[path = "vertex_plan.rs"]
mod vertex_plan;
