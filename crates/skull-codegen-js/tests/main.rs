// Single harness for this crate's integration tests: one linked binary
// instead of one per file. nextest still runs each test in its own
// process, so isolation is unchanged.

mod common;

#[path = "abi_plan_memo.rs"]
mod abi_plan_memo;
#[path = "backend_externs.rs"]
mod backend_externs;
#[path = "bounded_standalone_emission.rs"]
mod bounded_standalone_emission;
#[path = "closure.rs"]
mod closure;
#[path = "deepcopy.rs"]
mod deepcopy;
#[path = "differential.rs"]
mod differential;
#[path = "emit.rs"]
mod emit;
#[path = "enum_boxes.rs"]
mod enum_boxes;
#[path = "format_specs.rs"]
mod format_specs;
#[path = "generic_inline.rs"]
mod generic_inline;
#[path = "inline_pipeline.rs"]
mod inline_pipeline;
#[path = "inspect.rs"]
mod inspect;
#[path = "lambda.rs"]
mod lambda;
#[path = "log_dedup.rs"]
mod log_dedup;
#[path = "loops.rs"]
mod loops;
#[path = "publisher_abi.rs"]
mod publisher_abi;
#[path = "scalar_abi_arity.rs"]
mod scalar_abi_arity;
#[path = "map_iter.rs"]
mod map_iter;
#[path = "map_key_admission.rs"]
mod map_key_admission;
#[path = "match_fold.rs"]
mod match_fold;
#[path = "misc_intrinsics.rs"]
mod misc_intrinsics;
#[path = "module_globals.rs"]
mod module_globals;
#[path = "multi_rejection.rs"]
mod multi_rejection;

#[path = "multivalue.rs"]
mod multivalue;
#[path = "nb_differential.rs"]
mod nb_differential;
#[path = "no_inline_function.rs"]
mod no_inline_function;
#[path = "nomangle_roots.rs"]
mod nomangle_roots;
#[path = "number_format.rs"]
mod number_format;
#[path = "session_arena.rs"]
mod session_arena;
#[path = "state_schema.rs"]
mod state_schema;
#[path = "session_state.rs"]
mod session_state;

#[path = "overlay.rs"]
mod overlay;
#[path = "parse_float.rs"]
mod parse_float;
#[path = "polymorphic_recursion.rs"]
mod polymorphic_recursion;
#[path = "program.rs"]
mod program;
#[path = "put.rs"]
mod put;
#[path = "ret_bank_sizing.rs"]
mod ret_bank_sizing;
#[path = "rng.rs"]
mod rng;
#[path = "shared_tail.rs"]
mod shared_tail;
#[path = "snippets.rs"]
mod snippets;
#[path = "sort.rs"]
mod sort;
#[path = "spec_column_callee.rs"]
mod spec_column_callee;
#[path = "string_builder.rs"]
mod string_builder;
#[path = "string_primitives.rs"]
mod string_primitives;
#[path = "strings.rs"]
mod strings;
#[path = "table_unset.rs"]
mod table_unset;
#[path = "trampoline_fallback.rs"]
mod trampoline_fallback;
#[path = "try_call.rs"]
mod try_call;
#[path = "uint.rs"]
mod uint;
#[path = "value_gc_eq.rs"]
mod value_gc_eq;
#[path = "value_structs.rs"]
mod value_structs;
#[path = "vec_columns.rs"]
mod vec_columns;
#[path = "vec_flat_enum_strided.rs"]
mod vec_flat_enum_strided;
#[path = "optional_entity_vec.rs"]
mod optional_entity_vec;
#[path = "shader_register.rs"]
mod shader_register;
#[path = "opaque_container_axes_js.rs"]
mod opaque_container_axes_js;
#[path = "nb_value_struct.rs"]
mod nb_value_struct;
#[path = "native_buffer_element_admission_js.rs"]
mod native_buffer_element_admission_js;
#[path = "stdlib_sibling_location.rs"]
mod stdlib_sibling_location;
#[path = "host.rs"]
mod host;
#[path = "safenav_fold.rs"]
mod safenav_fold;
