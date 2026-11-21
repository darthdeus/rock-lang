// Single harness for this crate's integration tests: one linked binary
// instead of one per file. nextest still runs each test in its own
// process, so isolation is unchanged.

mod backend_externs;
mod common;
mod deepcopy;
mod global_read;
mod global_storage;
mod host;
mod host_supply_keys;
mod inspect;
mod misc_intrinsics;
mod multi_rejection;
mod native_buffer_element_admission_js;
mod nb_checked_set;
mod opaque_container_axes_js;
mod opaque_effects;
mod polymorphic_recursion;
mod program;
mod publisher_abi;
mod qualified_globals;
mod shader_register;
