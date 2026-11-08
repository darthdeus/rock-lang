//! bone's umbrella prelude: the `bone-core` foundation plus this crate's own
//! lowering / inference-algorithm / semantic-pass exports. Consumers and the
//! incremental/query layers import `bone_semantic::prelude::*` for everything.

pub use bone_core::modules::{
    IntrinsicType, intrinsic_type, is_descriptor_key, is_field_ref_former, is_intrinsic_key,
    is_intrinsic_key_in, is_stdlib_module_file,
};
pub use bone_core::prelude::*;

pub use crate::inference::*;
pub use crate::lower::*;
pub use crate::semantic_passes::*;
