// Single harness for this crate's integration tests: one linked binary
// instead of one per file. nextest still runs each test in its own
// process, so isolation is unchanged.

mod affine_bounds;
mod common;
mod private_vectors;

#[path = "host_supply_keys.rs"]
mod host_supply_keys;

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
#[path = "defer_unwind.rs"]
mod defer_unwind;
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
#[path = "map_key_admission.rs"]
mod map_key_admission;
#[path = "misc_intrinsics.rs"]
mod misc_intrinsics;
#[path = "multi_rejection.rs"]
mod multi_rejection;
#[path = "publisher_abi.rs"]
mod publisher_abi;
#[path = "qualified_globals.rs"]
mod qualified_globals;
#[path = "scalar_abi_arity.rs"]
mod scalar_abi_arity;

#[path = "global_layout.rs"]
mod global_layout;
#[path = "global_storage.rs"]
mod global_storage;
#[path = "multivalue.rs"]
mod multivalue;
#[path = "nb_differential.rs"]
mod nb_differential;
#[path = "nb_leaf_layout_premise.rs"]
mod nb_leaf_layout_premise;
#[path = "no_inline_function.rs"]
mod no_inline_function;
#[path = "nomangle_roots.rs"]
mod nomangle_roots;
#[path = "number_format.rs"]
mod number_format;
#[path = "session_arena.rs"]
mod session_arena;

#[path = "host.rs"]
mod host;
#[path = "native_buffer_element_admission_js.rs"]
mod native_buffer_element_admission_js;
#[path = "nb_value_struct.rs"]
mod nb_value_struct;
#[path = "opaque_container_axes_js.rs"]
mod opaque_container_axes_js;
#[path = "overlay.rs"]
mod overlay;
#[path = "polymorphic_recursion.rs"]
mod polymorphic_recursion;
#[path = "program.rs"]
mod program;
#[path = "ret_bank_sizing.rs"]
mod ret_bank_sizing;
#[path = "shader_register.rs"]
mod shader_register;
#[path = "snippets.rs"]
mod snippets;
#[path = "stdlib_sibling_location.rs"]
mod stdlib_sibling_location;
#[path = "trampoline_fallback.rs"]
mod trampoline_fallback;
#[path = "try_call.rs"]
mod try_call;
#[path = "uint.rs"]
mod uint;
#[path = "vec_columns.rs"]
mod vec_columns;

#[path = "opaque_effects.rs"]
mod opaque_effects;
