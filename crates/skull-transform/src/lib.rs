//! MIR-to-MIR passes run between lowering and emit: cleanup (copy
//! propagation, DCE, SROA), closure conversion, counted-loop recovery,
//! inlining, and the coroutine state-machine transform.

pub mod body_chain;
pub mod bound;
mod cleanup;
mod closure;
mod copy_prop;
mod coroutine;
mod coroutine_prepare;
mod detached;
mod forl;
mod frame;
mod in_bounds;
mod inline;
mod lambda_inline;
mod mem2reg;
mod nobox;
pub mod scope;
mod sroa;
#[cfg(test)]
mod test_support;
mod visit;

pub use cleanup::cleanup;
use cleanup::dce;
pub use closure::convert_closures;
use closure::convert_closures_framed;
pub use coroutine::{CoroLowered, STATE, transform_coroutine};
use coroutine_prepare::convert_named_nested_closures;
pub use coroutine_prepare::{PreparedCoroutine, prepare_coroutine};
pub(crate) use detached::{erase_detached_lambdas, lower_detached_region};
pub use forl::recover_counted_loops;
pub use frame::materialize_frame;
pub use in_bounds::elide_bounds_checks_for_target;
use inline::inline_functions_with_plans;
pub use inline::{
    InlineCache, InlineCandidateTable, InlineCandidateWarm, InlineOffGuard, inline_mode_guard,
    inline_off_active, inline_off_guard, validate_no_residual_bound_calls,
};
pub use nobox::{cleanup_with_nobox, nobox_materialization_violations};
pub(crate) use visit::top_level_blocks;
