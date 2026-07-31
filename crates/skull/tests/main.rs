// Single harness for this crate's integration tests: one linked binary
// instead of one per file. nextest still runs each test in its own
// process, so isolation is unchanged.

mod common;

#[path = "backend_variant_mangling.rs"]
mod backend_variant_mangling;
#[path = "bounded_lambda_inline.rs"]
mod bounded_lambda_inline;
#[path = "capture_free_hoist.rs"]
mod capture_free_hoist;
#[path = "closure_snapshot.rs"]
mod closure_snapshot;
#[path = "const_hoist.rs"]
mod const_hoist;
#[path = "const_hot_reload.rs"]
mod const_hot_reload;
#[path = "coroutine_hook_hoist.rs"]
mod coroutine_hook_hoist;
#[path = "dead_lambda_scope.rs"]
mod dead_lambda_scope;
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
#[path = "flat_vec_value_shape.rs"]
mod flat_vec_value_shape;
#[path = "fn_record_mt.rs"]
mod fn_record_mt;
#[path = "frame_lambda_hoist.rs"]
mod frame_lambda_hoist;
#[path = "frame_slot_naming.rs"]
mod frame_slot_naming;
#[path = "hole_runtime_error.rs"]
mod hole_runtime_error;
#[path = "incremental_emit.rs"]
mod incremental_emit;
#[path = "inline_value_specialization.rs"]
mod inline_value_specialization;
#[path = "intrinsics.rs"]
mod intrinsics;
#[path = "lambda_static.rs"]
mod lambda_static;
#[path = "loop_scoped_locals.rs"]
mod loop_scoped_locals;
#[path = "mangle_rename_hot_reload.rs"]
mod mangle_rename_hot_reload;
#[path = "mir_execution_oracle.rs"]
mod mir_execution_oracle;
#[path = "native_buffer_alloc.rs"]
mod native_buffer_alloc;
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
#[path = "user_fn_emit_rejection.rs"]
mod user_fn_emit_rejection;
#[path = "value_push_generic_boundary.rs"]
mod value_push_generic_boundary;
#[path = "value_scalarize.rs"]
mod value_scalarize;
#[path = "value_test_discovery.rs"]
mod value_test_discovery;
