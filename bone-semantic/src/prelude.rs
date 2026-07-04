//! bone's umbrella prelude: the `bone-core` foundation plus this crate's own
//! lowering / inference-algorithm / semantic-pass exports. Consumers and the
//! incremental/query layers import `bone_semantic::prelude::*` for everything.

pub use bone_core::prelude::*;

pub use crate::inference::*;
pub use crate::lower::*;
pub use crate::semantic_passes::*;
