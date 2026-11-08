//! The per-body pass chain both backends run before rendering: inline,
//! cleanup, closure conversion, counted-loop recovery and, for Lua's ordinary
//! bodies, inlining into the hoisted bodies. One sequence, so a new pass is
//! added in one place; the real backend differences are [`ChainOptions`].

use rustc_hash::FxHashSet as HashSet;

use bone_core::prelude::CompilerError;
use bone_core::workspace::Workspace;
use skull_mir::concrete::ConcretePlans;
use skull_mir::lower::ScalarCtx;
use skull_mir::mir::{FuncRef, MirFunc};
use skull_mir::verify::Stage;

use crate::{InlineCache, InlineScanStats, LoadStoreCensus, SpliceRecord};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyPass {
    Inline,
    Cleanup,
    Closure,
    CountedLoop,
    /// Inlining into one hoisted body; its cleanup reports as `Cleanup`.
    HoistedInline,
}

/// Sees each pass of the chain, for timing and work attribution.
pub trait PassObserver {
    fn begin(&mut self, _pass: BodyPass, _mf: &MirFunc) {}
    fn end(&mut self, _pass: BodyPass, _mf: &MirFunc) {}
    fn inlined(&mut self, _pass: BodyPass, _records: &[SpliceRecord], _scan: InlineScanStats) {}
    /// Where a post-inline cleanup charges the Load/Store traffic it unlinks,
    /// when the inline census is enabled.
    fn load_store(&mut self) -> Option<&mut LoadStoreCensus> {
        None
    }
}

impl PassObserver for () {}

/// Which bodies take `recover_counted_loops`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CountedLoops {
    Skip,
    Root,
    /// The root and each body closure conversion left directly under it.
    RootAndNested,
}

pub struct ChainOptions<'a> {
    /// Sees every synthetic name closure conversion mints.
    pub reserve: &'a dyn Fn(&str),
    pub counted_loops: CountedLoops,
    /// Inline into the hoisted bodies after conversion. A hoisted body's calls
    /// may need a specialized body only a splice can serve.
    pub inline_hoisted: bool,
}

impl ChainOptions<'_> {
    pub const PLAIN: ChainOptions<'static> = ChainOptions {
        reserve: &|_| {},
        counted_loops: CountedLoops::Root,
        inline_hoisted: false,
    };
}

pub struct Inliner<'a> {
    pub scx: &'a ScalarCtx,
    pub self_ref: FuncRef,
    pub cache: &'a mut InlineCache,
    pub plans: &'a ConcretePlans,
}

/// Run the MIR verifier on `mf` in debug builds, panicking on a violation.
/// `what` names the pass that just ran. Compiles to nothing in release.
#[inline(always)]
pub fn debug_verify(mf: &MirFunc, stage: Stage, what: &dyn std::fmt::Display) {
    #[cfg(debug_assertions)]
    skull_mir::verify::assert_verified(mf, stage, what);
    #[cfg(not(debug_assertions))]
    let _ = (mf, stage, what);
}

fn verify_after(mf: &MirFunc, stage: Stage, pass: BodyPass) {
    debug_verify(mf, stage, &format_args!("{pass:?}"));
}

/// A chain failure and the pass it came from.
pub struct ChainError {
    pub pass: BodyPass,
    pub error: CompilerError,
}

/// Inline, then [`convert_body`], then (when asked) inline into the hoisted
/// bodies. Returns every callee whose body was spliced in, plus the sources
/// of flat variants among the hoisted bodies.
pub fn prepare_body(
    ws: &Workspace,
    inliner: Inliner<'_>,
    mf: &mut MirFunc,
    name: &str,
    opts: &ChainOptions<'_>,
    obs: &mut dyn PassObserver,
) -> Result<HashSet<FuncRef>, ChainError> {
    debug_verify(mf, Stage::LOWERED, &"lowering");
    let floor = inliner.cache.census_enabled().then_some(mf.insts.len());
    obs.begin(BodyPass::Inline, mf);
    let (mut spliced, records, scan) = crate::inline_functions_with_plans(
        ws,
        inliner.scx,
        inliner.self_ref,
        mf,
        inliner.cache,
        inliner.plans,
    )
    .map_err(|error| ChainError {
        pass: BodyPass::Inline,
        error,
    })?;
    obs.inlined(BodyPass::Inline, &records, scan);
    obs.end(BodyPass::Inline, mf);
    verify_after(mf, Stage::LOWERED, BodyPass::Inline);
    convert(ws, mf, name, opts, obs, floor)?;
    if opts.inline_hoisted {
        inline_hoisted(ws, inliner, &mut mf.nested_fns, &mut spliced, obs)?;
    }
    Ok(spliced)
}

/// Cleanup, closure conversion (the root, then each named nested fn
/// depth-first) and counted-loop recovery: the chain for a body that is not
/// inlined.
pub fn convert_body(
    ws: &Workspace,
    mf: &mut MirFunc,
    name: &str,
    opts: &ChainOptions<'_>,
    obs: &mut dyn PassObserver,
) -> Result<(), ChainError> {
    debug_verify(mf, Stage::LOWERED, &"lowering");
    convert(ws, mf, name, opts, obs, None)
}

fn convert(
    ws: &Workspace,
    mf: &mut MirFunc,
    name: &str,
    opts: &ChainOptions<'_>,
    obs: &mut dyn PassObserver,
    census_floor: Option<usize>,
) -> Result<(), ChainError> {
    cleanup(ws, mf, BodyPass::Cleanup, obs, census_floor)?;
    obs.begin(BodyPass::Closure, mf);
    let named = mf.nested_fns.len();
    crate::convert_closures(mf, name);
    for nf in &mf.nested_fns[named..] {
        (opts.reserve)(&nf.name);
    }
    crate::convert_named_nested_closures(&mut mf.nested_fns[..named], opts.reserve);
    obs.end(BodyPass::Closure, mf);
    verify_after(mf, Stage::CONVERTED, BodyPass::Closure);
    if opts.counted_loops != CountedLoops::Skip {
        obs.begin(BodyPass::CountedLoop, mf);
        crate::recover_counted_loops(ws, mf);
        if opts.counted_loops == CountedLoops::RootAndNested {
            for nf in &mut mf.nested_fns {
                crate::recover_counted_loops(ws, nf);
            }
        }
        obs.end(BodyPass::CountedLoop, mf);
        verify_after(mf, Stage::CONVERTED, BodyPass::CountedLoop);
    }
    Ok(())
}

fn cleanup(
    ws: &Workspace,
    mf: &mut MirFunc,
    pass: BodyPass,
    obs: &mut dyn PassObserver,
    census_floor: Option<usize>,
) -> Result<(), ChainError> {
    obs.begin(BodyPass::Cleanup, mf);
    let result = match census_floor {
        Some(floor) => crate::cleanup_with_nobox_census(ws, mf, floor, obs.load_store()),
        None => crate::cleanup_with_nobox(ws, mf),
    };
    result.map_err(|error| ChainError { pass, error })?;
    obs.end(BodyPass::Cleanup, mf);
    // A hoisted body's cleanup runs after closure conversion.
    let stage = match pass {
        BodyPass::HoistedInline => Stage::CONVERTED,
        _ => Stage::LOWERED,
    };
    verify_after(mf, stage, pass);
    Ok(())
}

/// Inline into a hoisted-body tree depth-first, so a lambda nested in a lambda
/// is reached too. The inliner never scans a closure sub-CFG (a cell used in a
/// lambda body is an upvalue), so this runs only once conversion has split
/// those bodies out.
fn inline_hoisted(
    ws: &Workspace,
    inliner: Inliner<'_>,
    nested: &mut [MirFunc],
    spliced: &mut HashSet<FuncRef>,
    obs: &mut dyn PassObserver,
) -> Result<(), ChainError> {
    let Inliner {
        scx,
        self_ref,
        cache,
        plans,
    } = inliner;
    for nf in nested.iter_mut() {
        // Flat variants embed source without going through an inline splice.
        if let Some(source) = nf.flat_variant_source {
            spliced.insert(source);
        }
        let floor = cache.census_enabled().then_some(nf.insts.len());
        obs.begin(BodyPass::HoistedInline, nf);
        let (callees, records, scan) = crate::inline_functions_with_plans(
            ws, scx, self_ref, nf, cache, plans,
        )
        .map_err(|error| ChainError {
            pass: BodyPass::HoistedInline,
            error,
        })?;
        obs.inlined(BodyPass::HoistedInline, &records, scan);
        obs.end(BodyPass::HoistedInline, nf);
        verify_after(nf, Stage::CONVERTED, BodyPass::HoistedInline);
        spliced.extend(callees);
        cleanup(ws, nf, BodyPass::HoistedInline, obs, floor)?;
        let mut children = std::mem::take(&mut nf.nested_fns);
        let inliner = Inliner {
            scx,
            self_ref,
            cache: &mut *cache,
            plans,
        };
        inline_hoisted(ws, inliner, &mut children, spliced, obs)?;
        nf.nested_fns = children;
    }
    Ok(())
}
