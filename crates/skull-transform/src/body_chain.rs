use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use bone_core::hir::FnBackend;
use bone_core::prelude::CompilerError;
use bone_core::workspace::Workspace;
use comfy_la_arena::Idx;
use skull_mir::code_identity::LambdaSiteId;
use skull_mir::concrete::ConcretePlans;
use skull_mir::lower::ScalarCtx;
use skull_mir::mir::{Cell, FuncRef, Inst, MirFunc};
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
    ) -> Result<crate::inline::InlineOutcome, ChainError> {
        obs.begin(pass, mf);
        let read_stores = read_stores(mf);
        let outcome = crate::inline_functions_with_plans(
            ws,
            self.scx,
            self.self_ref,
            mf,
            self.cache,
            self.plans,
        )
        .map_err(|error| ChainError { pass, error })?;
        obs.inlined(pass, outcome.splices, outcome.copied_instructions);
        obs.end(pass, mf);
        verify_after(ws, self.scx.target(), mf, stage, pass);
        no_lost_writes(mf, read_stores, pass);
        Ok(outcome)
    }
}

#[inline(always)]
pub fn debug_verify(
    ws: &Workspace,
    target: FnBackend,
    mf: &MirFunc,
    stage: Stage,
    what: &dyn std::fmt::Display,
) {
    if skull_mir::verify::checks_enabled() {
        skull_mir::verify::assert_verified(ws, target, mf, stage, what);
    }
}

fn verify_after(ws: &Workspace, target: FnBackend, mf: &MirFunc, stage: Stage, pass: BodyPass) {
    debug_verify(ws, target, mf, stage, &format_args!("{pass:?}"));
}

fn read_stores(mf: &MirFunc) -> Option<skull_mir::verify::ReadStores> {
    skull_mir::verify::checks_enabled().then(|| skull_mir::verify::read_stores(mf))
}

fn no_lost_writes(mf: &MirFunc, before: Option<skull_mir::verify::ReadStores>, pass: BodyPass) {
    if let Some(before) = before {
        skull_mir::verify::assert_no_lost_writes(mf, &before, &format_args!("{pass:?}"));
    }
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
    debug_verify(ws, opts.target, mf, Stage::LOWERED, &"lowering");
    let outcome = inliner.run(ws, mf, BodyPass::Inline, Stage::LOWERED, obs)?;
    let mut spliced = outcome.dependencies;
    convert(
        ws,
        mf,
        name,
        opts,
        obs,
        &outcome.callbacks,
        |mf, sites, shared| {
            let caller = &ws.module(inliner.self_ref.module).functions[inliner.self_ref.func];
            crate::callback_lift::run(
                crate::callback_lift::Context {
                    ws,
                    scx: inliner.scx,
                    plans: inliner.plans,
                    allow_acyclic: caller.annotations.iter().any(|a| a.is_builtin("noalloc")),
                },
                &mut inliner.cache.callback_lift,
                mf,
                name,
                &mut spliced,
                sites,
                shared,
            )
        },
    )?;
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
    debug_verify(ws, opts.target, mf, Stage::LOWERED, &"lowering");
    convert(ws, mf, name, opts, obs, &HashSet::default(), |_, _, _| {
        false
    })
}

fn convert(
    ws: &Workspace,
    mf: &mut MirFunc,
    name: &str,
    opts: &ChainOptions<'_>,
    obs: &mut dyn PassObserver,
    callbacks: &HashSet<Idx<Inst>>,
    lift: impl FnOnce(
        &mut MirFunc,
        &mut Option<HashMap<Idx<Inst>, LambdaSiteId>>,
        &mut HashMap<Idx<Inst>, Idx<Cell>>,
    ) -> bool,
) -> Result<(), ChainError> {
    cleanup(ws, opts.target, mf, BodyPass::Cleanup, obs)?;
    let mut lambda_sites = (!callbacks.is_empty()).then(|| crate::closure::lambda_sites(mf));
    if crate::lambda_inline::inline_lambda_calls(mf, callbacks) {
        cleanup(ws, opts.target, mf, BodyPass::Cleanup, obs)?;
    }
    let named = mf.nested_fns.len();
    let mut shared = HashMap::default();
    let lifted = lift(mf, &mut lambda_sites, &mut shared);
    if lifted && shared.is_empty() {
        cleanup(ws, opts.target, mf, BodyPass::Cleanup, obs)?;
    }
    obs.begin(BodyPass::Closure, mf);
    crate::closure::convert_closures_with_lifted(mf, name, lambda_sites.as_ref(), &shared);
    if !shared.is_empty() {
        cleanup_converted(ws, opts.target, mf, obs)?;
    }
    for nf in &mf.nested_fns[named..] {
        (opts.reserve)(&nf.name);
    }
    crate::convert_named_nested_closures(&mut mf.nested_fns[..named], opts.reserve);
    obs.end(BodyPass::Closure, mf);
    verify_after(ws, opts.target, mf, Stage::CONVERTED, BodyPass::Closure);
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
    verify_after(ws, opts.target, mf, Stage::CONVERTED, BodyPass::CountedLoop);
    obs.begin(BodyPass::Bounds, mf);
    let count = crate::elide_bounds_checks_for_target(Some(ws), mf, opts.target);
    obs.bounds_elided(count);
    obs.end(BodyPass::Bounds, mf);
    verify_after(ws, opts.target, mf, Stage::CONVERTED, BodyPass::Bounds);
}

fn cleanup_converted(
    ws: &Workspace,
    target: FnBackend,
    mf: &mut MirFunc,
    obs: &mut dyn PassObserver,
) -> Result<(), ChainError> {
    cleanup(ws, target, mf, BodyPass::HoistedInline, obs)
}

fn cleanup(
    ws: &Workspace,
    target: FnBackend,
    mf: &mut MirFunc,
    pass: BodyPass,
    obs: &mut dyn PassObserver,
) -> Result<(), ChainError> {
    obs.begin(BodyPass::Cleanup, mf);
    let read_stores = read_stores(mf);
    crate::cleanup_with_nobox(ws, mf).map_err(|error| ChainError { pass, error })?;
    obs.end(BodyPass::Cleanup, mf);
    // A hoisted body's cleanup runs after closure conversion.
    let stage = match pass {
        BodyPass::HoistedInline => Stage::CONVERTED,
        _ => Stage::LOWERED,
    };
    verify_after(ws, target, mf, stage, pass);
    no_lost_writes(mf, read_stores, BodyPass::Cleanup);
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
        spliced.extend(
            inliner
                .run(ws, nf, pass, Stage::CONVERTED, obs)?
                .dependencies,
        );
        cleanup(ws, inliner.scx.target(), nf, pass, obs)?;
        inline_hoisted(ws, inliner, &mut nf.nested_fns, spliced, obs)?;
    }
    Ok(())
}
