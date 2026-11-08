//! skull-mir: the cell-model CFG IR and the HIR->CFG lowering for skull.
//!
//! Variables are mutable
//! cells, expression results are single-use values, `Idx<Inst>` IS the value,
//! control flow lives only in block terminators, and there are NO block params
//! (merges go through cells). Transforms live in `skull-transform`; codegen in
//! `skull-codegen-*`.

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
pub mod reach;
pub mod shader_ir;
pub mod sinking;
pub mod site;
pub mod spec_census;
pub mod struct_defaults;
pub mod verdict;
pub mod verify;
pub mod vertex_plan;
