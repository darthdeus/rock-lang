//! Cell-model CFG IR: variables are mutable cells, an `Idx<Inst>` is its single-use
//! value, control flow lives only in terminators, and merges go through cells
//! (no SSA, no block params).
pub mod append_vec;
pub mod bound;
pub mod cfg;
pub mod code_identity;
pub mod concrete;
pub mod decorator;
pub mod definition;
pub mod display;
pub mod global_storage;
pub mod gpu_layout;
pub mod instance;
pub mod intrinsics;
pub mod lifecycle;
pub mod lower;
pub mod lua_nesting;
pub mod mangle;
pub mod mir;
pub mod monoty;
pub mod names;
pub mod nilness;
pub mod reach;
pub mod shader_ir;
pub mod sinking;
pub mod site;
mod striped;
pub mod struct_defaults;
pub mod type_table;
pub mod verdict;
pub mod verify;
pub mod vertex_plan;
