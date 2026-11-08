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
mod inline;
mod load_store_census;
mod mem2reg;
mod native_buffer;
mod nobox;
pub mod scope;
mod sroa;
#[cfg(test)]
mod test_support;
mod visit;

pub use cleanup::{cleanup, dce, fold_const_branches};
pub use closure::{FrameInfo, convert_closures, convert_closures_framed};
pub(crate) use copy_prop::resolve_subst;
pub use coroutine::{
    CoroLowered, CoroutineFramePlan, STATE, transform_coroutine,
    transform_coroutine_with_nonescaping_lambdas,
};
pub use coroutine_prepare::{
    CoroutineKeys, PreparedCoroutine, convert_named_nested_closures, prepare_coroutine,
};
pub use detached::{
    LoweredDetachedRegion, erase_detached_lambdas, lower_detached_region,
    lower_detached_region_with_source,
};
pub use forl::recover_counted_loops;
pub use frame::{materialize_frame, materialize_frame_cells};
pub use inline::{
    InlineCache, InlineCandidateProfile, InlineCandidateTable, InlineCandidateTableStats,
    InlineCandidateWarm, InlineOffGuard, InlineScanStats, SpliceRecord, fn_has_bounds,
    inline_functions_with_plans, inline_off_active, inline_off_guard,
    validate_no_residual_bound_calls,
};
pub use load_store_census::{
    LoadStoreCensus, LoadStoreRow, cleanup_with_nobox_census, observe_cleanup,
};
pub use mem2reg::{Mem2RegPromotion, promote_direct_cells};
pub use native_buffer::scalarize_native_buffer_stores;
pub use nobox::{cleanup_with_nobox, flatten_nobox_nested_ctors, nobox_materialization_violations};
pub(crate) use visit::{remap_inst, remap_term, top_level_blocks};
