use rustc_hash::FxHashSet as HashSet;

use bone_core::prelude::CompilerError;
use bone_core::workspace::Workspace;
use skull_mir::concrete::ConcretePlans;
use skull_mir::lower::ScalarCtx;
use skull_mir::mir::{FuncRef, MirFunc};
use skull_mir::verify::Stage;

use crate::InlineCache;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyPass {
    Inline,
    Cleanup,
    Closure,
    CountedLoop,
    Bounds,
    /// Inlining into one hoisted body; its cleanup reports as `Cleanup`.
    HoistedInline,
}

pub trait PassObserver {
    fn begin(&mut self, _pass: BodyPass, _mf: &MirFunc) {}
    fn end(&mut self, _pass: BodyPass, _mf: &MirFunc) {}
    fn inlined(&mut self, _pass: BodyPass, _splices: usize, _copied_instructions: usize) {}
    fn bounds_elided(&mut self, _count: usize) {}
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
    /// Runtime allocation guarantees used by bounds analysis.
    pub target: bone_core::hir::FnBackend,
    /// Sees every synthetic name closure conversion mints.
    pub reserve: &'a dyn Fn(&str),
    pub counted_loops: CountedLoops,
    /// Inline into the hoisted bodies after conversion. A hoisted body's calls
    /// may need a specialized body only a splice can serve.
    pub inline_hoisted: bool,
}

impl ChainOptions<'_> {
    /// Standalone Lua bodies without a name-reservation callback.
    pub const PLAIN: ChainOptions<'static> = ChainOptions {
        target: bone_core::hir::FnBackend::Lua,
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

impl Inliner<'_> {
    fn run(
        &mut self,
        ws: &Workspace,
        mf: &mut MirFunc,
        pass: BodyPass,
        stage: Stage,
        obs: &mut dyn PassObserver,
    ) -> Result<HashSet<FuncRef>, ChainError> {
        obs.begin(pass, mf);
        let (callees, splices, copied_instructions) = crate::inline_functions_with_plans(
            ws,
            self.scx,
            self.self_ref,
            mf,
            self.cache,
            self.plans,
        )
        .map_err(|error| ChainError { pass, error })?;
        obs.inlined(pass, splices, copied_instructions);
        obs.end(pass, mf);
        verify_after(mf, stage, pass);
        Ok(callees)
    }
}

#[inline(always)]
pub fn debug_verify(mf: &MirFunc, stage: Stage, what: &dyn std::fmt::Display) {
    if skull_mir::verify::checks_enabled() {
        skull_mir::verify::assert_verified(mf, stage, what);
    }
}

fn verify_after(mf: &MirFunc, stage: Stage, pass: BodyPass) {
    debug_verify(mf, stage, &format_args!("{pass:?}"));
}

pub struct ChainError {
    pub pass: BodyPass,
    pub error: CompilerError,
}

pub fn prepare_body(
    ws: &Workspace,
    mut inliner: Inliner<'_>,
    mf: &mut MirFunc,
    name: &str,
    opts: &ChainOptions<'_>,
    obs: &mut dyn PassObserver,
) -> Result<HashSet<FuncRef>, ChainError> {
    debug_verify(mf, Stage::LOWERED, &"lowering");
    let mut spliced = inliner.run(ws, mf, BodyPass::Inline, Stage::LOWERED, obs)?;
    convert(ws, mf, name, opts, obs)?;
    if opts.inline_hoisted {
        inline_hoisted(ws, &mut inliner, &mut mf.nested_fns, &mut spliced, obs)?;
    }
    Ok(spliced)
}

pub fn convert_body(
    ws: &Workspace,
    mf: &mut MirFunc,
    name: &str,
    opts: &ChainOptions<'_>,
    obs: &mut dyn PassObserver,
) -> Result<(), ChainError> {
    debug_verify(mf, Stage::LOWERED, &"lowering");
    convert(ws, mf, name, opts, obs)
}

fn convert(
    ws: &Workspace,
    mf: &mut MirFunc,
    name: &str,
    opts: &ChainOptions<'_>,
    obs: &mut dyn PassObserver,
) -> Result<(), ChainError> {
    cleanup(ws, mf, BodyPass::Cleanup, obs)?;
    if crate::lambda_inline::inline_lambda_calls(mf) {
        cleanup(ws, mf, BodyPass::Cleanup, obs)?;
    }
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
        recover_and_elide(ws, mf, opts, obs);
        if opts.counted_loops == CountedLoops::RootAndNested {
            for nf in &mut mf.nested_fns {
                recover_and_elide(ws, nf, opts, obs);
            }
        }
    }
    Ok(())
}

fn recover_and_elide(
    ws: &Workspace,
    mf: &mut MirFunc,
    opts: &ChainOptions<'_>,
    obs: &mut dyn PassObserver,
) {
    obs.begin(BodyPass::CountedLoop, mf);
    crate::recover_counted_loops(ws, mf);
    obs.end(BodyPass::CountedLoop, mf);
    verify_after(mf, Stage::CONVERTED, BodyPass::CountedLoop);
    obs.begin(BodyPass::Bounds, mf);
    let count = crate::elide_bounds_checks_for_target(Some(ws), mf, opts.target);
    obs.bounds_elided(count);
    obs.end(BodyPass::Bounds, mf);
    verify_after(mf, Stage::CONVERTED, BodyPass::Bounds);
}

fn cleanup(
    ws: &Workspace,
    mf: &mut MirFunc,
    pass: BodyPass,
    obs: &mut dyn PassObserver,
) -> Result<(), ChainError> {
    obs.begin(BodyPass::Cleanup, mf);
    crate::cleanup_with_nobox(ws, mf).map_err(|error| ChainError { pass, error })?;
    obs.end(BodyPass::Cleanup, mf);
    // A hoisted body's cleanup runs after closure conversion.
    let stage = match pass {
        BodyPass::HoistedInline => Stage::CONVERTED,
        _ => Stage::LOWERED,
    };
    verify_after(mf, stage, pass);
    Ok(())
}

fn inline_hoisted(
    ws: &Workspace,
    inliner: &mut Inliner<'_>,
    nested: &mut [MirFunc],
    spliced: &mut HashSet<FuncRef>,
    obs: &mut dyn PassObserver,
) -> Result<(), ChainError> {
    for nf in nested.iter_mut() {
        // Flat variants embed source without going through an inline splice.
        if let Some(source) = nf.flat_variant_source {
            spliced.insert(source);
        }
        let pass = BodyPass::HoistedInline;
        spliced.extend(inliner.run(ws, nf, pass, Stage::CONVERTED, obs)?);
        cleanup(ws, nf, pass, obs)?;
        inline_hoisted(ws, inliner, &mut nf.nested_fns, spliced, obs)?;
    }
    Ok(())
}
