//! A target's ARTIFACT VIEW of the shared instance graph.
//!
//! An artifact view is one target's REALIZATION of a graph it did not build:
//! which instances it REACHES, how each is supplied, which opaque dependency
//! resolves to which callee, and what it spells each retained instance.
//!
//! **It may DROP and it may REJECT; it may never DISCOVER.** [`realize`] takes
//! `&InstanceGraph`, so it cannot rebuild or extend what it measures -- plan
//! S 20g.1 asks whether logical identity survives realization differing, and a
//! view that invented an instance would be answering its own question.
//!
//! **It is a REACHABLE view, not a declaration filter.** Retention is a walk
//! from the artifact's own roots over the graph's recorded call sites, so a
//! declaration this target could compile but nothing here calls is
//! [`Exclusion::Unreachable`], not retained. And a retained caller whose
//! concrete callee is unrealizable is a [`Severed`] call -- the diagnostic a
//! filter cannot produce at all, because a filter never looks at an edge.
//!
//! **There is ONE graph, and it is target-neutral.** It records bone's collapsed
//! group REPRESENTATIVE at every callee, so selecting the backend sibling that
//! serves a target happens here, in [`realize_decl`], and nowhere else. One
//! shared [`Instance`] therefore realizes to a DIFFERENT declaration -- and a
//! different [`BoundaryId`] -- on two artifacts while remaining one logical
//! identity, which is plan S 20a's separation stated as a mechanism rather than
//! as a claim about two graphs that happened to agree.
//!
//! **Identity across artifacts is the shared [`Instance`], NOT the spelling.**
//! Plan S 0h: `InstanceId` is target-neutral and the ARTIFACT owns the emitted
//! spelling, one symbol per `(artifact, instance)`. Two artifacts may legally
//! spell one instance differently, so this module asserts totality and
//! injectivity INSIDE each artifact and leaves cross-artifact comparison to the
//! `Instance`. The default [`SymbolPlanner`] happens to be target-neutral;
//! the renaming planner in `artifact_view` exists so that fact cannot be
//! mistaken for a requirement.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use bone_semantic::hir::{FnBackend, Function};
use bone_semantic::inference::Workspace;
use skull_mir::intrinsics::{self, BackendRealization};
use skull_mir::mir::FuncRef;

use crate::concrete_body::BoundaryId;
use crate::instance_graph::{
    self, BoundaryClass, Concreteness, DeclKey, Instance, InstanceGraph, Site, SiteOutcome,
};
use crate::target_projection::{supports, CpuTargetSpec, TargetSpec};
use rustc_hash::{FxHashMap, FxHashSet};

#[cfg(feature = "test-hooks")]
thread_local! {
    static REALIZE_INVOCATIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static FRESH_INDEX_SITE_ROWS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

pub fn realize_invocations() -> u64 {
    #[cfg(feature = "test-hooks")]
    {
        REALIZE_INVOCATIONS.with(std::cell::Cell::get)
    }
    #[cfg(not(feature = "test-hooks"))]
    {
        0
    }
}

pub fn fresh_index_site_rows() -> u64 {
    #[cfg(feature = "test-hooks")]
    {
        FRESH_INDEX_SITE_ROWS.with(std::cell::Cell::get)
    }
    #[cfg(not(feature = "test-hooks"))]
    {
        0
    }
}

/// Where an artifact's realization of a callee comes from, when the
/// declaration's own body is not it.
///
/// Two independent sources, both real:
///
/// - **the body slot**, which IS luajit's mapping (`skull_mir::intrinsics`'
///   body-slot rule: an `inline(lua)` body is the luajit implementation of an
///   `@intrinsic`). A rule about the declaration, not a table.
/// - **the backend's own realization oracle**, for a backend that supplies
///   callees from Rust: its `@intrinsic` mappings AND everything else it
///   renders itself, layered in its own precedence
///   ([`BackendRealization`]).
///
/// ONE oracle for the second, not one per mechanism. The JS backend renders
/// `nb_get` from an `@intrinsic` template and `phantom` from a hand-written one
/// with no marker at all, and a view that knew about one severed the other --
/// so `skull_codegen_js` publishes a single `js_supply` that `render_call`,
/// `discover_callees` and this view all read.
///
/// A target with neither maps nothing, which is the honest answer for a
/// synthetic fixture: it has no backend to map with.
#[derive(Clone, Copy, Default)]
pub struct ArtifactMappings<'a> {
    /// The backend whose in-source body slot IS this artifact's mapping.
    pub inline_backend: Option<FnBackend>,
    /// This artifact's backend realization oracle, when it has one.
    pub supply: Option<&'a dyn BackendRealization>,
}

impl<'a> ArtifactMappings<'a> {
    /// A target that maps nothing natively.
    pub fn none() -> Self {
        Self::default()
    }

    pub fn for_backend(b: FnBackend) -> Self {
        Self {
            inline_backend: Some(b),
            supply: None,
        }
    }

    pub fn with_supply(mut self, s: &'a dyn BackendRealization) -> Self {
        self.supply = Some(s);
        self
    }

    /// Does this artifact's BACKEND supply `f` itself -- with no declared body
    /// emitted for it at all? Asked at the top of realization, ahead of sibling
    /// selection and ahead of every body branch, because that is where the
    /// backend asks it.
    fn supplies(&self, ws: &Workspace, f: &Function) -> bool {
        self.supply.is_some_and(|s| s.supplies(ws, f))
    }

    /// Does this artifact map `f` NATIVELY? A Rock body is not consulted --
    /// that is the portable rung, a different answer to a different question.
    pub fn maps(&self, ws: &Workspace, f: &Function) -> bool {
        if let Some(b) = self.inline_backend {
            if f.inline_body
                .is_some_and(|(be, _)| instance_graph::serves(be, b))
            {
                return true;
            }
        }
        self.supplies(ws, f)
    }
}

/// What an artifact SPELLS an instance.
///
/// A seam, not indirection for its own sake: plan S 0h puts the emitted
/// spelling under the artifact, so "every artifact spells this instance alike"
/// is a property of the default planner and not of the model. A test planner
/// that renames deliberately is what keeps the two apart.
pub trait SymbolPlanner {
    fn symbol(&self, ws: &Workspace, g: &InstanceGraph, i: &Instance, realized: Realized)
        -> String;
    fn describe(&self) -> &'static str;
}

/// The default: today's logical encoder (`spec_fn_name`), which is
/// target-neutral. Injective over the instances it is given, which is exactly
/// what it CANNOT answer for an artifact holding two declarations that encode
/// alike -- hence the per-artifact collision check.
pub struct LogicalSymbols;

impl SymbolPlanner for LogicalSymbols {
    fn symbol(
        &self,
        ws: &Workspace,
        g: &InstanceGraph,
        i: &Instance,
        _realized: Realized,
    ) -> String {
        g.logical_symbol(ws, i)
    }
    fn describe(&self) -> &'static str {
        "logical"
    }
}

/// Spell an instance through the declaration this artifact selected. This is
/// the production choice for backends whose sibling declarations own different
/// publication conventions (`@nomangle` is the visible case).
pub struct RealizedSymbols;

impl SymbolPlanner for RealizedSymbols {
    fn symbol(
        &self,
        ws: &Workspace,
        g: &InstanceGraph,
        i: &Instance,
        realized: Realized,
    ) -> String {
        Instance {
            func: realized.decl,
            args: i.args.clone(),
        }
        .symbol(ws, g.interner())
    }

    fn describe(&self) -> &'static str {
        "realized-declaration"
    }
}

pub static REALIZED_SYMBOLS: RealizedSymbols = RealizedSymbols;

/// Everything one artifact view is taken through.
pub struct ArtifactRequest<'a> {
    /// The artifact's full target: platform, execution and optional shader.
    /// [`realize`] derives a platform+execution CPU target; shader packaging is
    /// carried and reported but cannot reach admission.
    pub spec: TargetSpec,
    pub mappings: ArtifactMappings<'a>,
    /// The artifact's emission roots. Plan S 11: "the target may change roots",
    /// so they are an INPUT, never re-derived here from the graph's contents.
    pub roots: &'a [FuncRef],
    pub symbols: &'a dyn SymbolPlanner,
    scalar_context: Option<&'a skull_mir::lower::ScalarCtx>,
}

impl<'a> ArtifactRequest<'a> {
    /// The default request for `spec`: its own backend's mapping, no table, and
    /// the logical spelling.
    pub fn new(spec: TargetSpec, roots: &'a [FuncRef]) -> Self {
        Self {
            mappings: match spec.execution.backend {
                Some(b) => ArtifactMappings::for_backend(b),
                None => ArtifactMappings::none(),
            },
            spec,
            roots,
            symbols: &LogicalSymbols,
            scalar_context: None,
        }
    }

    pub fn with_scalar_context(mut self, context: &'a skull_mir::lower::ScalarCtx) -> Self {
        self.scalar_context = Some(context);
        self
    }

    fn scalar_context<'b>(
        &'b self,
        ws: &Workspace,
        g: &InstanceGraph,
        local: &'b std::cell::OnceCell<skull_mir::lower::ScalarCtx>,
    ) -> Option<&'b skull_mir::lower::ScalarCtx> {
        let backend = self.spec.execution.backend?;
        if let Some(context) = self.scalar_context {
            assert_eq!(
                context.target(),
                backend,
                "artifact scalar context backend mismatch"
            );
            assert!(
                std::ptr::eq(context.interner(), g.interner().as_ref()),
                "artifact scalar context must share the graph interner"
            );
            return Some(context);
        }
        // Standalone graph consumers have no emission context to retain.
        Some(local.get_or_init(|| {
            skull_mir::lower::ScalarCtx::new(ws)
                .with_target(backend)
                .sharing(g.interner().clone())
        }))
    }

    pub fn with_mappings(mut self, m: ArtifactMappings<'a>) -> Self {
        self.mappings = m;
        self
    }

    pub fn with_symbols(mut self, p: &'a dyn SymbolPlanner) -> Self {
        self.symbols = p;
        self
    }
}

/// How this artifact actually supplies a retained declaration. Plan S 10's
/// body-kind taxonomy, resolved rather than declared: the same `@intrinsic`
/// declaration is [`Self::NativeMapping`] on one artifact and
/// [`Self::PortableFallback`] on another.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Realization {
    /// A Rock body this artifact lowers.
    RockBody,
    /// A foreign body this artifact emits verbatim.
    ForeignBody(BoundaryClass),
    /// An `@intrinsic` this artifact maps natively.
    NativeMapping,
    /// An `@intrinsic` with no mapping here, served by its portable Rock body.
    PortableFallback,
    /// A declaration with no `@intrinsic` marker that this artifact's backend
    /// supplies itself, with no declared body emitted at all -- the JS
    /// backend's `phantom`, `error` and `map_new` renderings. The same oracle
    /// answers for [`Self::NativeMapping`]; what separates them is whether the
    /// DECLARATION carries the marker.
    HostBinding,
    /// An `@intrinsic` whose emission is compile-time on every backend
    /// (`@struct_expand` and friends). It has no callable body to map or fall
    /// back to, so neither question applies.
    CompileTimeExpansion,
}

impl Realization {
    pub fn label(self) -> &'static str {
        match self {
            Self::RockBody => "rock-body",
            Self::ForeignBody(c) => c.name(),
            Self::NativeMapping => "native-mapping",
            Self::HostBinding => "host-binding",
            Self::PortableFallback => "portable-fallback",
            Self::CompileTimeExpansion => "compile-time-expansion",
        }
    }

    /// The boundary class this realization is, when it is one. A `Rock`
    /// callee has none; a foreign or erased one does, and its class is part of
    /// the [`BoundaryId`] a dependency must terminate in.
    pub fn boundary_class(self) -> Option<BoundaryClass> {
        match self {
            Self::ForeignBody(c) => Some(c),
            // A hand-written backend binding is class 1.3 by content -- a
            // mapping that lives in Rust, per backend, rather than in the
            // source -- so it names the same contract an `@intrinsic` mapping
            // does.
            Self::NativeMapping | Self::CompileTimeExpansion | Self::HostBinding => {
                Some(BoundaryClass::Intrinsic)
            }
            Self::RockBody | Self::PortableFallback => None,
        }
    }
}

/// One instance, realized: the DECLARATION this artifact selected for it and how
/// that declaration is supplied.
///
/// The declaration is not `Instance::func`. The graph holds the target-neutral
/// representative; a backend sibling serving this artifact is a different
/// `FuncRef`, and it is the one whose body is emitted and whose contract a
/// [`BoundaryId`] names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Realized {
    pub decl: FuncRef,
    pub kind: Realization,
}

/// Why a declaration in the shared graph is not realizable on this artifact.
/// Each is plan S 10's `Invalid boundary` outcome with its cause kept, because
/// "unavailable" collapses three different diagnostics into one word.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Unrealizable {
    /// The declaration's backend does not serve this artifact. The
    /// `@nomangle`-sibling case: a JS body in a Lua walk.
    ForeignToArtifact,
    /// The artifact's capabilities do not supply this boundary CLASS
    /// (`extern` with no foreign symbols, a verbatim body with no inline
    /// backend).
    ClassUnsupported(BoundaryClass),
    /// `@native_required`, and this artifact has no native mapping for it.
    /// A Rock fallback does not answer: the marker's whole content is that the
    /// portable rung is skipped.
    NativeMappingMissing,
}

impl Unrealizable {
    pub fn label(self) -> &'static str {
        match self {
            Self::ForeignToArtifact => "foreign-to-artifact",
            Self::ClassUnsupported(_) => "class-unsupported",
            Self::NativeMappingMissing => "native-mapping-missing",
        }
    }
}

/// Why an instance the shared graph holds is not in this artifact. The two are
/// different findings: an unrealizable declaration is a REJECTION the artifact
/// would have to report if anything called it, while an unreachable one is
/// simply not this artifact's program.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Exclusion {
    Unrealizable(Unrealizable),
    /// Realizable here, but no root of this artifact reaches it.
    Unreachable,
}

impl Exclusion {
    pub fn label(self) -> &'static str {
        match self {
            Self::Unrealizable(u) => u.label(),
            Self::Unreachable => "unreachable",
        }
    }
}

/// An instance the shared graph holds and this artifact does not.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Excluded {
    pub instance: Instance,
    pub cause: Exclusion,
}

/// A call from a RETAINED instance whose concrete callee this artifact cannot
/// realize. The diagnostic a declaration filter structurally cannot produce:
/// the caller survives, so emission would emit a call to a body that is not in
/// the artifact.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Severed {
    pub caller: Instance,
    pub site: Site,
    pub callee: Instance,
    pub cause: Unrealizable,
}

/// Severed calls sharing one callee DECLARATION and one cause -- the
/// artifact's obligations, as opposed to the per-site cost of not meeting them.
///
/// Both numbers are kept because either alone reads as the other: 76,972
/// severed JS sites is a worklist of that size only if every site is its own
/// missing declaration, and the declaration count alone hides that one missing
/// body can cost thousands of calls.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SeveredGroup {
    /// The shared graph's representative declaration for the callee. Not a
    /// realized one: nothing here realized, which is the point.
    pub callee: FuncRef,
    pub cause: Unrealizable,
    /// Call sites -- the per-site population this group covers.
    pub sites: usize,
    /// The distinct callee INSTANCES: one declaration can be severed at several
    /// instantiations, and supplying a body discharges all of them.
    pub instances: BTreeSet<Instance>,
    pub callers: BTreeSet<Instance>,
}

/// What a call in THIS artifact terminates in -- [`Callee`] with the boundary
/// arm REALIZED.
///
/// The shared form cannot carry a [`BoundaryId`]: an id names a target and an
/// emitted symbol, and a target-neutral body has neither. This is the same
/// split one rung down, not a second callee model.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RealizedCallee {
    Rock(Instance),
    Boundary(BoundaryId),
}

/// One opaque-body dependency, terminating in a concrete callee.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct InlineDep {
    /// The opaque body that names it.
    pub owner: FuncRef,
    pub entry: &'static str,
    /// [`RealizedCallee::Boundary`] for a runtime entry, [`RealizedCallee::Rock`]
    /// for a declaration this artifact lowers. Never a bare `FuncRef`: that
    /// loses the boundary class and cannot name WHICH instance when a
    /// declaration has more than one.
    pub callee: RealizedCallee,
}

/// An opaque-body dependency this artifact cannot resolve to one callee. A
/// located rejection: the owner is retained and its text calls a name nothing
/// here supplies (or supplies twice over), so emission would produce a call to
/// a symbol that does not exist or cannot be chosen.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UnresolvedInlineDep {
    pub owner: FuncRef,
    pub entry: &'static str,
    pub reason: &'static str,
}

/// Two retained instances that spell alike.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SymbolCollision {
    pub symbol: String,
    pub instances: Vec<Instance>,
}

/// Two PUBLICATION obligations (see [`ArtifactView::publications`]) claiming
/// one physical symbol. Rejected rather than resolved by last-definition-wins,
/// which is what a renderer that did not model this distinctly would silently
/// do to two `@nomangle` declarations sharing a bare name.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PublicationCollision {
    pub symbol: String,
    pub instances: Vec<Instance>,
}

#[derive(Clone, Debug, Default)]
pub struct ArtifactCensus {
    /// Instances retained, the roots they were reached from, and the shared
    /// population they were taken from.
    /// Rock instances the artifact lowers, and boundaries it names. Reported
    /// apart because they are different obligations; `retained` is their sum.
    pub instances: usize,
    pub boundaries: usize,
    pub retained: usize,
    pub roots: usize,
    pub shared_instances: usize,
    pub excluded: usize,
    pub excluded_by_cause: BTreeMap<&'static str, usize>,
    pub realized_by_kind: BTreeMap<&'static str, usize>,
    /// Call sites traversed by the reachability walk -- the denominator for
    /// `severed`. Zero sites walked and zero severed calls are the same number.
    pub sites_walked: usize,
    pub severed: usize,
    pub severed_by_cause: BTreeMap<&'static str, usize>,
    pub symbols: usize,
    pub symbol_collisions: usize,
    /// Dependencies examined, and how many named no single callee here.
    pub inline_deps_examined: usize,
    pub inline_deps_unresolved: usize,
    pub inline_deps_boundary: usize,
    pub inline_deps_rock: usize,
    /// Foreign bodies the artifact emits, and how many of them publish more
    /// than one result lane -- the population plan S 13b's publisher contract
    /// governs, and the widest wire the artifact's scratch bank must cover.
    pub foreign_bodies: usize,
    pub multi_lane_publishers: usize,
    pub widest_result_lanes: usize,
    /// Retained instances with no substitution record in the shared graph. An
    /// invariant finding; 0 is the only acceptable value.
    pub missing_substitutions: usize,
    pub publications: usize,
    pub unrepresented_publications: usize,
    pub publication_collisions: usize,
    /// Deferred obligations, carried from the target spec.
    pub deferred: Vec<&'static str>,
}

/// One target's realization of a graph it did not build.
#[derive(Clone)]
pub struct ArtifactView {
    pub spec: TargetSpec,
    /// The artifact's root instances -- realizable emission roots, plus the
    /// callees of always-emitted bodies, which have no incoming edge.
    pub roots: BTreeSet<Instance>,
    /// The subset of [`Self::roots`] that this artifact was ASKED for --
    /// `ArtifactRequest::roots`, canonicalized -- as distinct from the
    /// structural roots the graph adds because they carry no incoming edge
    /// (`is_always_emitted_target`).
    ///
    /// The two are different obligations and a consumer that conflates them is
    /// wrong in both directions: a declared root's own body must exist whether
    /// or not any call reaches it, while a structural root is an ordinary
    /// reached instance whose splicing eligibility is still open. Recorded here
    /// so emission reads root PROVENANCE off the view instead of re-running the
    /// root predicate beside it.
    ///
    /// A generic root contributes nothing: it has no base instance to emit.
    pub declared_roots: BTreeSet<Instance>,
    /// Retained instances a retained body reaches by taking the function as a
    /// VALUE (`SiteOp::FnValue`) rather than by calling it.
    ///
    /// A value take is not spliceable: there is no call for a body to be
    /// substituted into, so the program stores the definition itself under its
    /// own symbol. That makes it a DELIVERY fact a residual-call scan cannot
    /// produce -- and a foreign (`inline(lua)`) body reached only this way was
    /// retained, called by name and never defined.
    pub fn_value_targets: BTreeSet<Instance>,
    /// Retained ROCK instances: reachable from `roots`, realizable here, and
    /// lowered from a Rock body. A SUBSET of `g.instances`.
    ///
    /// Plan S 9's `ArtifactGraph` separates `instances` from `boundaries`, and
    /// so does this: a boundary has no body the artifact lowers, so enqueueing
    /// it as an instance would ask for callees it does not have.
    pub instances: BTreeSet<Instance>,
    /// Retained BOUNDARIES: the identity a call must name, PER INSTANCE.
    /// Reached, never traversed.
    ///
    /// Keyed by [`Instance`] rather than by [`BoundaryId`]. A boundary is
    /// retained because an instance reached it, so the instance is the identity
    /// that cannot collide; keying the other way made two instances of one
    /// generic boundary declaration overwrite each other.
    pub boundaries: BTreeMap<Instance, BoundaryId>,
    /// How each retained instance is supplied here, and by which declaration.
    pub realization: BTreeMap<Instance, Realized>,
    pub excluded: Vec<Excluded>,
    pub severed: Vec<Severed>,
    pub sites_walked: usize,
    /// One symbol per retained instance. Total and injective WITHIN this
    /// artifact; nothing here claims another artifact spells it the same.
    pub symbols: BTreeMap<Instance, String>,
    pub symbol_collisions: Vec<SymbolCollision>,
    pub inline_deps: Vec<InlineDep>,
    pub unresolved_inline_deps: Vec<UnresolvedInlineDep>,
    /// Bare-name roots recorded by the shared graph for retained `inline(js)`
    /// declarations. These are opaque-boundary obligations, not MIR delivery
    /// edges; JS adds the targets to its next graph/view construction.
    pub opaque_boundary_roots: BTreeMap<DeclKey, FxHashSet<FuncRef>>,
    /// The physical RESULT LANES of every retained instance the artifact emits
    /// as a foreign body: how many values its result wire carries. Plan S 13b's
    /// JS publisher takes its arity from exactly this, so an artifact that
    /// retains a multi-lane `inline(js)` body states the lane count here rather
    /// than leaving emission to read it out of the body text.
    ///
    /// Derived from `skull_mir::lower::BackendFnAbiPlan`, built under the
    /// instance's own substitution -- the same ABI policy definitions and call
    /// sites read, asked of the shape this artifact actually carries.
    ///
    /// Recorded for EVERY artifact, including luajit, even though only the JS
    /// publisher needs the number spelled out: the width is a property of the
    /// wire, and luajit carrying it on native multi-return is that backend's
    /// answer to it rather than an absence of one.
    pub publisher_lanes: BTreeMap<Instance, usize>,
    /// **An invariant finding, not an unknown.** Retained instances the shared
    /// graph holds no substitution record for at all. `reach` records one for
    /// every instance it reaches, roots included, so an absent record means a
    /// producer bypassed the recorder -- and until it is explained, nothing may
    /// read a shape for the instance at all. This is the ONLY way a retained
    /// foreign body can go without a stated wire: there is no "generic, so the
    /// declaration cannot answer" population any more, because the width is
    /// built from the instance's substitution rather than from the declaration.
    pub missing_substitutions: BTreeSet<Instance>,
    /// The shared population this view was taken over -- the denominator.
    pub shared_instances: usize,
    pub deferred: Vec<&'static str>,
    /// **Bare-name PUBLICATION obligations, distinct from [`Self::realization`].**
    ///
    /// `realization` (and `instances`/`boundaries` derived from it) answers
    /// "how are CALLS to this canonical `Instance` served on this artifact" --
    /// `realize_decl`'s sibling selection may legitimately route every call to
    /// a same-signature backend sibling. A `publication` answers a different
    /// question: "which EXACT declarations does the program's source demand
    /// exist under their own name regardless of any call". An explicit
    /// `@nomangle` root is the source of that demand -- its whole contract is a
    /// stable bare-named entry point for code OUTSIDE the MIR call graph
    /// (hand-written Lua, FFI callbacks), which backend-variant collapse must
    /// not silently erase just because some sibling happens to serve the calls
    /// this artifact CAN see.
    ///
    /// Keyed by the EXACT declaration's own `Instance` (never canonicalized
    /// away), which can legitimately equal a key in `realization` naming a
    /// DIFFERENT `decl` -- one `Instance` can carry both a call-serving
    /// realization through a sibling and a publication obligation for its own
    /// body, and that is not a conflict: a publication is not a call.
    ///
    /// Reuses the graph's OWN expansion for the exact declaration -- no second
    /// discovery walk. An `@nomangle` root that is not, itself, the graph's own
    /// canonical representative for its declaration has no contribution record
    /// this pass can reuse, and is reported via
    /// [`Self::unrepresented_publications`] rather than silently dropped.
    pub publications: BTreeMap<Instance, String>,
    /// `@nomangle` roots whose exact declaration is not the graph's own
    /// canonical representative (so this pass has no expansion to reuse for
    /// it) -- an invariant finding this pass fails closed on rather than
    /// silently omitting the promise. Not expected in the shipped corpus: an
    /// ordinary `@nomangle` declaration with no `inline`/`extern` annotation is
    /// backend `Any`, which is always the LOWEST backend-rank in its group and
    /// therefore always the representative already.
    pub unrepresented_publications: Vec<FuncRef>,
    pub publication_collisions: Vec<PublicationCollision>,
    index: ArtifactIndex,
}

#[derive(Clone, Default)]
struct ArtifactIndex {
    realizable: FxHashMap<Instance, Result<Realized, Unrealizable>>,
    outgoing: FxHashMap<Instance, FxHashMap<Site, Instance>>,
    incoming: FxHashMap<Instance, FxHashSet<Instance>>,
    site_counts: FxHashMap<Instance, usize>,
    fn_values_by_caller: FxHashMap<Instance, FxHashSet<Instance>>,
    fn_value_users: FxHashMap<Instance, FxHashSet<Instance>>,
    severed_by_caller: FxHashMap<Instance, Vec<Severed>>,
    exclusions: FxHashMap<Instance, Exclusion>,
    symbol_users: FxHashMap<String, FxHashSet<Instance>>,
    realization_by_decl: FxHashMap<FuncRef, FxHashSet<Instance>>,
    instances_by_decl: FxHashMap<FuncRef, FxHashSet<Instance>>,
    inline_by_owner: BTreeMap<DeclKey, (Vec<InlineDep>, Vec<UnresolvedInlineDep>)>,
    inline_consumers: BTreeMap<DeclKey, BTreeSet<DeclKey>>,
    inline_dependencies_by_owner: BTreeMap<DeclKey, BTreeSet<DeclKey>>,
    publication_candidates: BTreeMap<Instance, String>,
    publication_decls: BTreeMap<DeclKey, BTreeSet<Instance>>,
    publication_users: BTreeMap<String, BTreeSet<Instance>>,
    root_decls: BTreeSet<DeclKey>,
}

#[derive(Debug)]
pub struct ArtifactUpdateStats {
    pub revisited: usize,
    pub added: usize,
    pub removed: usize,
    /// Local queue subjects and neighbours actually processed by the delta.
    pub iterations: usize,
    /// Root entries examined while applying an explicit root delta.
    pub root_entries_examined: usize,
    /// Rows traversed in the connected reverse indexes while retracting and
    /// reinstalling local publications, inline dependencies, and findings.
    pub local_index_rows_examined: usize,
    /// Whole-output scans performed by a local update. The ordinary delta path
    /// keeps this at zero; a nonzero value is a structural finding, not a
    /// timing claim.
    pub broad_scans: usize,
    pub scalar_context_builds: usize,
}

fn severed_order(left: &Severed, right: &Severed) -> std::cmp::Ordering {
    left.caller
        .cmp(&right.caller)
        .then_with(|| left.site.cmp(&right.site))
        .then_with(|| left.callee.cmp(&right.callee))
        .then_with(|| left.cause.cmp(&right.cause))
}

fn func_ref_order(left: &FuncRef, right: &FuncRef) -> std::cmp::Ordering {
    left.module
        .index()
        .cmp(&right.module.index())
        .then_with(|| u32::from(left.func.into_raw()).cmp(&u32::from(right.func.into_raw())))
}

fn inline_dep_order(left: &InlineDep, right: &InlineDep) -> std::cmp::Ordering {
    func_ref_order(&left.owner, &right.owner).then_with(|| left.entry.cmp(right.entry))
}

fn unresolved_inline_dep_order(
    left: &UnresolvedInlineDep,
    right: &UnresolvedInlineDep,
) -> std::cmp::Ordering {
    func_ref_order(&left.owner, &right.owner).then_with(|| left.entry.cmp(right.entry))
}

fn publication_candidates(ws: &Workspace, req: &ArtifactRequest<'_>) -> BTreeMap<Instance, String> {
    let target = req.spec.cpu();
    req.roots
        .iter()
        .filter_map(|r| {
            let f = &ws.module(r.module).functions[r.func];
            (f.annotations
                .iter()
                .any(|a| a.name.name.as_str() == "nomangle")
                && instance_graph::serves(
                    f.backend,
                    target.execution.backend.unwrap_or(FnBackend::Any),
                ))
            .then(|| (Instance::base(*r), f.name.to_string()))
        })
        .collect()
}

impl ArtifactView {
    #[cfg(feature = "test-hooks")]
    fn forced_broad_scan_count(&self) -> usize {
        if std::env::var_os("SKULL_FORCE_ARTIFACT_BROAD_SCAN").is_none() {
            return 0;
        }
        self.realization.keys().count()
            + self
                .index
                .publication_users
                .values()
                .map(BTreeSet::len)
                .sum::<usize>()
    }

    #[cfg(not(feature = "test-hooks"))]
    fn forced_broad_scan_count(&self) -> usize {
        0
    }

    /// Apply graph deltas to this target without re-realizing the workspace.
    /// The retained view owns the indexes needed to retract an old owner and
    /// install its replacement; unchanged owners keep their immutable output.
    pub fn update_local(
        &mut self,
        ws: &Workspace,
        g: &InstanceGraph,
        req: &ArtifactRequest<'_>,
        revisited: &BTreeSet<Instance>,
        added: &BTreeSet<Instance>,
        removed: &BTreeSet<Instance>,
    ) -> ArtifactUpdateStats {
        let old_roots = self.index.root_decls.clone();
        let new_roots: BTreeSet<DeclKey> = req
            .roots
            .iter()
            .map(|r| DeclKey(skull_mir::lower::canonical_variant(ws, *r)))
            .collect();
        let root_added: BTreeSet<DeclKey> = new_roots.difference(&old_roots).cloned().collect();
        let root_removed: BTreeSet<DeclKey> = old_roots.difference(&new_roots).cloned().collect();

        self.update_local_with_decl_delta(
            ws,
            g,
            req,
            revisited,
            added,
            removed,
            &root_added,
            &root_removed,
        )
    }

    /// Apply a graph delta and an explicit root delta. Production callers keep
    /// the requested root set in their own cache and pass only changed roots;
    /// [`Self::update_local`] remains the full-request oracle for callers that
    /// do not retain that state.
    pub fn update_local_with_root_delta(
        &mut self,
        ws: &Workspace,
        g: &InstanceGraph,
        req: &ArtifactRequest<'_>,
        revisited: &BTreeSet<Instance>,
        added: &BTreeSet<Instance>,
        removed: &BTreeSet<Instance>,
        root_added: &[FuncRef],
        root_removed: &[FuncRef],
    ) -> ArtifactUpdateStats {
        let root_added: BTreeSet<DeclKey> = root_added
            .iter()
            .map(|r| DeclKey(skull_mir::lower::canonical_variant(ws, *r)))
            .collect();
        let root_removed: BTreeSet<DeclKey> = root_removed
            .iter()
            .map(|r| DeclKey(skull_mir::lower::canonical_variant(ws, *r)))
            .collect();
        self.update_local_with_decl_delta(
            ws,
            g,
            req,
            revisited,
            added,
            removed,
            &root_added,
            &root_removed,
        )
    }

    fn update_local_with_decl_delta(
        &mut self,
        ws: &Workspace,
        g: &InstanceGraph,
        req: &ArtifactRequest<'_>,
        revisited: &BTreeSet<Instance>,
        added: &BTreeSet<Instance>,
        removed: &BTreeSet<Instance>,
        root_added: &BTreeSet<DeclKey>,
        root_removed: &BTreeSet<DeclKey>,
    ) -> ArtifactUpdateStats {
        let broad_scans = self.forced_broad_scan_count();
        let mut local_index_rows_examined = 0;
        let mut root_subjects = BTreeSet::new();
        for decl in root_added.iter().chain(root_removed) {
            local_index_rows_examined += 1;
            root_subjects.extend(
                self.index
                    .instances_by_decl
                    .get(&decl.0)
                    .into_iter()
                    .flat_map(|instances| {
                        local_index_rows_examined += instances.len();
                        instances.iter().cloned()
                    }),
            );
            let instances: Vec<_> = g.instances_of(decl.0).cloned().collect();
            local_index_rows_examined += instances.len();
            root_subjects.extend(instances);
        }
        let mut subjects = revisited.clone();
        subjects.extend(added.iter().cloned());
        subjects.extend(removed.iter().cloned());
        subjects.extend(root_subjects.iter().cloned());
        if subjects.is_empty() && root_added.is_empty() && root_removed.is_empty() {
            self.shared_instances = g.instances.len();
            return ArtifactUpdateStats {
                revisited: 0,
                added: 0,
                removed: 0,
                iterations: 0,
                root_entries_examined: 0,
                local_index_rows_examined,
                broad_scans,
                scalar_context_builds: 0,
            };
        }

        let old_root_subjects: BTreeSet<Instance> = subjects
            .iter()
            .filter(|instance| self.roots.contains(*instance))
            .cloned()
            .collect();
        let old_realizable: FxHashMap<Instance, Result<Realized, Unrealizable>> = subjects
            .iter()
            .filter_map(|i| self.index.realizable.get(i).map(|r| (i.clone(), *r)))
            .collect();
        let old_outgoing: FxHashMap<Instance, FxHashMap<Site, Instance>> = subjects
            .iter()
            .filter_map(|i| {
                self.index
                    .outgoing
                    .get(i)
                    .map(|edges| (i.clone(), edges.clone()))
            })
            .collect();
        let old_predecessors: FxHashMap<Instance, FxHashSet<Instance>> = subjects
            .iter()
            .filter_map(|i| {
                self.index
                    .incoming
                    .get(i)
                    .map(|preds| (i.clone(), preds.clone()))
            })
            .collect();
        let old_symbols: BTreeMap<Instance, String> = subjects
            .iter()
            .filter_map(|i| {
                self.symbols
                    .get(i)
                    .map(|symbol| (i.clone(), symbol.clone()))
            })
            .collect();

        // Remove only the outgoing rows owned by the changed subjects. The
        // old rows remain in `old_outgoing` for the local dirty-region walk.
        for i in &subjects {
            local_index_rows_examined += 1;
            if let Some(edges) = self.index.outgoing.remove(i) {
                local_index_rows_examined += edges.len();
                for callee in edges.values() {
                    if let Some(preds) = self.index.incoming.get_mut(callee) {
                        preds.remove(i);
                        if preds.is_empty() {
                            self.index.incoming.remove(callee);
                        }
                    }
                }
            }
            if let Some(values) = self.index.fn_values_by_caller.remove(i) {
                local_index_rows_examined += values.len();
                for callee in values {
                    if let Some(users) = self.index.fn_value_users.get_mut(&callee) {
                        users.remove(i);
                        if users.is_empty() {
                            self.index.fn_value_users.remove(&callee);
                            self.fn_value_targets.remove(&callee);
                        }
                    }
                }
            }
            self.index.severed_by_caller.remove(i);
        }

        let target = req.spec.cpu();
        for i in &subjects {
            local_index_rows_examined += 1;
            if !g.instances.contains(i) {
                self.index.realizable.remove(i);
                continue;
            }
            self.index.realizable.insert(
                i.clone(),
                realize_decl(ws, i.func, &g.erased_entries, &target, &req.mappings),
            );
            let sites = g.sites_of(i).cloned().unwrap_or_default();
            self.index.site_counts.insert(i.clone(), sites.len());
            for (site, record) in &sites {
                local_index_rows_examined += 1;
                let SiteOutcome::Resolved(callee) = &record.outcome else {
                    continue;
                };
                self.index
                    .outgoing
                    .entry(i.clone())
                    .or_default()
                    .insert(site.clone(), callee.clone());
                self.index
                    .incoming
                    .entry(callee.clone())
                    .or_default()
                    .insert(i.clone());
                if site.op == skull_mir::site::SiteOp::FnValue {
                    self.index
                        .fn_values_by_caller
                        .entry(i.clone())
                        .or_default()
                        .insert(callee.clone());
                    self.index
                        .fn_value_users
                        .entry(callee.clone())
                        .or_default()
                        .insert(i.clone());
                }
            }
        }

        let mut root_entries_examined = 0;
        for decl in root_removed {
            local_index_rows_examined += 1;
            root_entries_examined += usize::from(self.index.root_decls.remove(decl));
        }
        for decl in root_added {
            local_index_rows_examined += 1;
            root_entries_examined += usize::from(self.index.root_decls.insert(*decl));
        }
        for i in &subjects {
            local_index_rows_examined += 1;
            self.roots.remove(i);
            self.declared_roots.remove(i);
        }
        for i in subjects.iter().filter(|i| g.instances.contains(*i)) {
            local_index_rows_examined += 1;
            let declared = self.index.root_decls.contains(&DeclKey(i.func));
            let structural = g.is_always_emitted_target(i);
            if (declared || structural) && self.index.realizable[i].is_ok() {
                self.roots.insert(i.clone());
                if declared {
                    self.declared_roots.insert(i.clone());
                }
            }
        }
        let root_delta_changed = !root_added.is_empty() || !root_removed.is_empty();

        let edge_changed = subjects
            .iter()
            .any(|i| old_outgoing.get(i) != self.index.outgoing.get(i));
        let realization_changed = subjects
            .iter()
            .any(|i| old_realizable.get(i) != self.index.realizable.get(i));
        let roots_changed = subjects
            .iter()
            .any(|i| old_root_subjects.contains(i) != self.roots.contains(i));
        if !edge_changed
            && !realization_changed
            && !roots_changed
            && !root_delta_changed
            && added.is_empty()
            && removed.is_empty()
        {
            self.shared_instances = g.instances.len();
            return ArtifactUpdateStats {
                revisited: revisited.len(),
                added: added.len(),
                removed: removed.len(),
                iterations: subjects.len() + local_index_rows_examined,
                root_entries_examined,
                local_index_rows_examined,
                broad_scans,
                scalar_context_builds: 0,
            };
        }

        // Only the connected region around a changed owner can change
        // reachability. This uses the retained view's predecessor/successor
        // indexes; no graph-wide candidate scan is needed.
        let mut dirty = subjects.clone();
        let mut queue: VecDeque<Instance> = subjects.iter().cloned().collect();
        while let Some(i) = queue.pop_front() {
            local_index_rows_examined += 1;
            let mut neighbours = Vec::new();
            if let Some(edges) = old_outgoing.get(&i) {
                local_index_rows_examined += edges.len();
                neighbours.extend(edges.values().cloned());
            }
            if let Some(edges) = self.index.outgoing.get(&i) {
                local_index_rows_examined += edges.len();
                neighbours.extend(edges.values().cloned());
            }
            if let Some(preds) = self.index.incoming.get(&i) {
                local_index_rows_examined += preds.len();
                neighbours.extend(preds.iter().cloned());
            }
            if let Some(preds) = old_predecessors.get(&i) {
                local_index_rows_examined += preds.len();
                neighbours.extend(preds.iter().cloned());
            }
            for neighbour in neighbours {
                if dirty.insert(neighbour.clone()) {
                    queue.push_back(neighbour);
                }
            }
        }

        let mut reachable_dirty = BTreeSet::new();
        let mut reach_queue: VecDeque<Instance> = self
            .roots
            .iter()
            .filter(|i| dirty.contains(*i))
            .cloned()
            .collect();
        while let Some(i) = reach_queue.pop_front() {
            local_index_rows_examined += 1;
            if !dirty.contains(&i) || reachable_dirty.contains(&i) {
                continue;
            }
            let Some(Ok(realized)) = self.index.realizable.get(&i) else {
                continue;
            };
            reachable_dirty.insert(i.clone());
            if realized.kind.boundary_class().is_some() {
                continue;
            }
            if let Some(edges) = self.index.outgoing.get(&i) {
                local_index_rows_examined += edges.len();
                for callee in edges.values() {
                    if dirty.contains(callee) {
                        reach_queue.push_back(callee.clone());
                    }
                }
            }
        }
        for i in &dirty {
            local_index_rows_examined += 1;
            self.index.severed_by_caller.remove(i);
            self.remove_instance_output(i, self.index.site_counts.get(i).copied().unwrap_or(0));
            self.index.exclusions.remove(i);
        }
        for i in removed {
            local_index_rows_examined += 1;
            self.index.site_counts.remove(i);
        }
        let scalar_context = std::cell::OnceCell::new();
        for i in dirty.iter().filter(|i| g.instances.contains(*i)) {
            local_index_rows_examined += 1;
            let Some(result) = self.index.realizable.get(i).copied() else {
                continue;
            };
            if reachable_dirty.contains(i) {
                let realized = result.expect("retained artifact instance must be realizable");
                self.install_instance_output(ws, g, req, i, realized, &scalar_context);
                let declared = self.index.root_decls.contains(&DeclKey(i.func));
                if declared || g.is_always_emitted_target(i) {
                    self.roots.insert(i.clone());
                    if declared {
                        self.declared_roots.insert(i.clone());
                    }
                }
                if realized.kind.boundary_class().is_none() {
                    let mut severed = Vec::new();
                    if let Some(sites) = self.index.outgoing.get(i) {
                        for (site, callee) in sites {
                            if let Some(Err(cause)) = self.index.realizable.get(callee) {
                                severed.push(Severed {
                                    caller: i.clone(),
                                    site: site.clone(),
                                    callee: callee.clone(),
                                    cause: *cause,
                                });
                            }
                        }
                    }
                    self.index.severed_by_caller.insert(i.clone(), severed);
                }
            } else {
                let cause = match result {
                    Ok(_) => Exclusion::Unreachable,
                    Err(reason) => Exclusion::Unrealizable(reason),
                };
                self.index.exclusions.insert(i.clone(), cause);
            }
        }

        let mut changed_decls = BTreeSet::new();
        for i in &dirty {
            local_index_rows_examined += 1;
            changed_decls.insert(DeclKey(i.func));
            if let Some(Ok(realized)) = old_realizable.get(i) {
                changed_decls.insert(DeclKey(realized.decl));
            }
            if let Some(Ok(realized)) = self.index.realizable.get(i) {
                changed_decls.insert(DeclKey(realized.decl));
            }
        }
        local_index_rows_examined += self.refresh_inline_local(ws, g, &changed_decls);
        local_index_rows_examined += self.refresh_opaque_roots(g, &changed_decls);
        let mut changed_publication_symbols: BTreeSet<String> = dirty
            .iter()
            .filter_map(|instance| self.index.publication_candidates.get(instance).cloned())
            .collect();
        let (publication_symbols, publication_rows) =
            self.refresh_publication_candidates(ws, req, root_added, root_removed);
        changed_publication_symbols.extend(publication_symbols);
        local_index_rows_examined += publication_rows;
        local_index_rows_examined +=
            self.refresh_publications_local(g, &changed_publication_symbols);
        local_index_rows_examined += self.refresh_excluded_local(&dirty);
        local_index_rows_examined += self.refresh_severed_local(&dirty);
        let mut changed_symbols: BTreeSet<String> = old_symbols.into_values().collect();
        changed_symbols.extend(dirty.iter().filter_map(|i| self.symbols.get(i).cloned()));
        local_index_rows_examined += self.refresh_symbol_collisions_local(&changed_symbols);
        self.shared_instances = g.instances.len();
        ArtifactUpdateStats {
            revisited: revisited.len(),
            added: added.len(),
            removed: removed.len(),
            iterations: dirty.len() + reachable_dirty.len() + local_index_rows_examined,
            root_entries_examined,
            local_index_rows_examined,
            broad_scans,
            scalar_context_builds: usize::from(scalar_context.get().is_some()),
        }
    }

    fn remove_instance_output(&mut self, i: &Instance, old_site_count: usize) {
        self.roots.remove(i);
        self.declared_roots.remove(i);
        if let Some(symbol) = self.symbols.remove(i) {
            if let Some(users) = self.index.symbol_users.get_mut(&symbol) {
                users.remove(i);
                if users.is_empty() {
                    self.index.symbol_users.remove(&symbol);
                }
            }
        }
        if let Some(realized) = self.realization.remove(i) {
            let decl = realized.decl;
            let remove_decl = self
                .index
                .realization_by_decl
                .get_mut(&decl)
                .is_some_and(|users| {
                    users.remove(i);
                    users.is_empty()
                });
            if remove_decl {
                self.index.realization_by_decl.remove(&decl);
            }
            let instance_decl = i.func;
            let remove_instance_decl = self
                .index
                .instances_by_decl
                .get_mut(&instance_decl)
                .is_some_and(|users| {
                    users.remove(i);
                    users.is_empty()
                });
            if remove_instance_decl {
                self.index.instances_by_decl.remove(&instance_decl);
            }
            self.boundaries.remove(i);
            if realized.kind.boundary_class().is_none() {
                self.instances.remove(i);
                self.sites_walked = self.sites_walked.saturating_sub(old_site_count);
            }
        }
        self.publisher_lanes.remove(i);
        self.missing_substitutions.remove(i);
        if let Some(values) = self.index.fn_values_by_caller.get(i).cloned() {
            for callee in values {
                if let Some(users) = self.index.fn_value_users.get_mut(&callee) {
                    users.remove(i);
                    if users.is_empty() {
                        self.index.fn_value_users.remove(&callee);
                        self.fn_value_targets.remove(&callee);
                    }
                }
            }
        }
    }

    fn install_instance_output(
        &mut self,
        ws: &Workspace,
        g: &InstanceGraph,
        req: &ArtifactRequest<'_>,
        i: &Instance,
        realized: Realized,
        scalar_context: &std::cell::OnceCell<skull_mir::lower::ScalarCtx>,
    ) {
        let symbol = req.symbols.symbol(ws, g, i, realized);
        self.symbols.insert(i.clone(), symbol.clone());
        self.index
            .symbol_users
            .entry(symbol.clone())
            .or_default()
            .insert(i.clone());
        self.realization.insert(i.clone(), realized);
        self.index
            .realization_by_decl
            .entry(realized.decl)
            .or_default()
            .insert(i.clone());
        self.index
            .instances_by_decl
            .entry(i.func)
            .or_default()
            .insert(i.clone());
        if realized.kind.boundary_class().is_none() {
            self.instances.insert(i.clone());
            self.sites_walked += self.index.site_counts.get(i).copied().unwrap_or(0);
        }
        if let Some(class) = realized.kind.boundary_class() {
            self.boundaries.insert(
                i.clone(),
                BoundaryId {
                    declaration: realized.decl,
                    class,
                    target: req.spec.cpu(),
                    symbol,
                },
            );
        }
        if let Some(values) = self.index.fn_values_by_caller.get(i) {
            for callee in values {
                self.fn_value_targets.insert(callee.clone());
            }
        }
        if g.concreteness(i) == Concreteness::Missing {
            self.missing_substitutions.insert(i.clone());
        }
        if matches!(
            realized.kind,
            Realization::ForeignBody(BoundaryClass::InlineBody)
        ) {
            if let Some(scx) = req.scalar_context(ws, g, scalar_context) {
                if let Ok(signature) =
                    crate::concrete_body::concrete_signature_for_instance(ws, g, i)
                {
                    let selected = Instance {
                        func: realized.decl,
                        args: i.args.clone(),
                    };
                    self.publisher_lanes.insert(
                        i.clone(),
                        skull_mir::lower::BackendFnAbiPlan::for_concrete_signature(
                            ws, scx, &selected, &signature,
                        )
                        .result_lanes(),
                    );
                }
            }
        }
    }

    fn refresh_inline_local(
        &mut self,
        ws: &Workspace,
        g: &InstanceGraph,
        changed_decls: &BTreeSet<DeclKey>,
    ) -> usize {
        let mut rows_examined = 0;
        let mut owners = BTreeSet::new();
        for decl in changed_decls {
            if let Some(consumers) = self.index.inline_consumers.get(decl) {
                rows_examined += consumers.len();
                owners.extend(consumers.iter().cloned());
            }
            if g.inline_body_deps.contains_key(decl) {
                rows_examined += 1;
                owners.insert(*decl);
            }
        }
        if owners.is_empty() {
            return rows_examined;
        }
        for owner in &owners {
            rows_examined += 1;
            self.remove_inline_rows(owner.0);
            if let Some(dependencies) = self.index.inline_dependencies_by_owner.remove(owner) {
                rows_examined += dependencies.len();
                for dependency in dependencies {
                    if let Some(consumers) = self.index.inline_consumers.get_mut(&dependency) {
                        consumers.remove(owner);
                        if consumers.is_empty() {
                            self.index.inline_consumers.remove(&dependency);
                        }
                    }
                }
            }
            let Some(entries) = g.inline_body_deps.get(owner) else {
                self.index.inline_by_owner.remove(owner);
                continue;
            };
            if !self.index.realization_by_decl.contains_key(&owner.0)
                && !self.index.instances_by_decl.contains_key(&owner.0)
            {
                self.index.inline_by_owner.remove(owner);
                continue;
            }
            for entry in entries {
                rows_examined += 1;
                if let Some(rep) = instance_graph::stdlib_entry_representative(ws, entry) {
                    let dependency = DeclKey(rep);
                    self.index
                        .inline_consumers
                        .entry(dependency)
                        .or_default()
                        .insert(*owner);
                    self.index
                        .inline_dependencies_by_owner
                        .entry(*owner)
                        .or_default()
                        .insert(dependency);
                }
            }
            let mut inline = Vec::new();
            let mut unresolved = Vec::new();
            for entry in entries {
                rows_examined += 1;
                let Some(rep) = instance_graph::stdlib_entry_representative(ws, entry) else {
                    unresolved.push(UnresolvedInlineDep {
                        owner: owner.0,
                        entry,
                        reason: "no single declaration supplies this entry",
                    });
                    continue;
                };
                let Some(instances) = self.index.instances_by_decl.get(&rep) else {
                    unresolved.push(UnresolvedInlineDep {
                        owner: owner.0,
                        entry,
                        reason: "the serving declaration is not retained by the artifact",
                    });
                    continue;
                };
                rows_examined += instances.len();
                let mut matching = instances.iter();
                let Some(inst) = matching.next() else {
                    unresolved.push(UnresolvedInlineDep {
                        owner: owner.0,
                        entry,
                        reason: "the serving declaration is not retained by the artifact",
                    });
                    continue;
                };
                if matching.next().is_some() {
                    unresolved.push(UnresolvedInlineDep {
                        owner: owner.0,
                        entry,
                        reason: "the serving declaration has more than one retained instance",
                    });
                    continue;
                }
                let callee = match self.boundaries.get(inst) {
                    Some(id) => RealizedCallee::Boundary(id.clone()),
                    None => RealizedCallee::Rock(inst.clone()),
                };
                inline.push(InlineDep {
                    owner: owner.0,
                    entry,
                    callee,
                });
            }
            self.index
                .inline_by_owner
                .insert(*owner, (inline, unresolved));
            if let Some((deps, unresolved)) = self.index.inline_by_owner.get(owner) {
                rows_examined += deps.len() + unresolved.len();
                for dep in deps {
                    let position = self
                        .inline_deps
                        .binary_search_by(|current| inline_dep_order(current, dep))
                        .unwrap_or_else(|position| position);
                    self.inline_deps.insert(position, dep.clone());
                }
                for dep in unresolved {
                    let position = self
                        .unresolved_inline_deps
                        .binary_search_by(|current| unresolved_inline_dep_order(current, dep))
                        .unwrap_or_else(|position| position);
                    self.unresolved_inline_deps.insert(position, dep.clone());
                }
            }
        }
        rows_examined
    }

    fn remove_inline_rows(&mut self, owner: FuncRef) {
        while let Ok(position) = self
            .inline_deps
            .binary_search_by(|current| func_ref_order(&current.owner, &owner))
        {
            self.inline_deps.remove(position);
        }
        while let Ok(position) = self
            .unresolved_inline_deps
            .binary_search_by(|current| func_ref_order(&current.owner, &owner))
        {
            self.unresolved_inline_deps.remove(position);
        }
    }

    fn refresh_opaque_roots(
        &mut self,
        g: &InstanceGraph,
        changed_decls: &BTreeSet<DeclKey>,
    ) -> usize {
        for owner in changed_decls {
            self.opaque_boundary_roots.remove(owner);
            if self.index.realization_by_decl.contains_key(&owner.0)
                || self.index.instances_by_decl.contains_key(&owner.0)
            {
                if let Some(targets) = g.inline_body_nomangle_roots.get(owner) {
                    self.opaque_boundary_roots.insert(*owner, targets.clone());
                }
            }
        }
        changed_decls.len()
    }

    fn refresh_publication_candidates(
        &mut self,
        ws: &Workspace,
        req: &ArtifactRequest<'_>,
        root_added: &BTreeSet<DeclKey>,
        root_removed: &BTreeSet<DeclKey>,
    ) -> (BTreeSet<String>, usize) {
        let mut changed = BTreeSet::new();
        let mut rows_examined = root_added.len() + root_removed.len();
        for decl in root_removed {
            let Some(instances) = self.index.publication_decls.remove(decl) else {
                continue;
            };
            rows_examined += instances.len();
            for instance in instances {
                let Some(symbol) = self.index.publication_candidates.remove(&instance) else {
                    continue;
                };
                changed.insert(symbol.clone());
                if let Some(users) = self.index.publication_users.get_mut(&symbol) {
                    rows_examined += users.len();
                    users.remove(&instance);
                    if users.is_empty() {
                        self.index.publication_users.remove(&symbol);
                    }
                }
                self.publications.remove(&instance);
                if let Ok(position) = self
                    .unrepresented_publications
                    .binary_search_by(|current| func_ref_order(current, &instance.func))
                {
                    self.unrepresented_publications.remove(position);
                }
            }
        }
        let target = req.spec.cpu();
        for decl in root_added {
            let f = &ws.module(decl.0.module).functions[decl.0.func];
            if !f
                .annotations
                .iter()
                .any(|a| a.name.name.as_str() == "nomangle")
                || !instance_graph::serves(
                    f.backend,
                    target.execution.backend.unwrap_or(FnBackend::Any),
                )
            {
                continue;
            }
            let instance = Instance::base(decl.0);
            let symbol = f.name.to_string();
            if self
                .index
                .publication_candidates
                .insert(instance.clone(), symbol.clone())
                .is_some()
            {
                continue;
            }
            self.index
                .publication_decls
                .entry(*decl)
                .or_default()
                .insert(instance.clone());
            self.index
                .publication_users
                .entry(symbol.clone())
                .or_default()
                .insert(instance);
            changed.insert(symbol);
        }
        (changed, rows_examined)
    }

    fn refresh_publications_local(
        &mut self,
        g: &InstanceGraph,
        changed_symbols: &BTreeSet<String>,
    ) -> usize {
        let mut rows_examined = changed_symbols.len();
        for symbol in changed_symbols {
            let users = self
                .index
                .publication_users
                .get(symbol)
                .cloned()
                .unwrap_or_default();
            rows_examined += users.len();
            for instance in &users {
                self.publications.remove(instance);
                let fref = instance.func;
                if let Ok(position) = self
                    .unrepresented_publications
                    .binary_search_by(|current| func_ref_order(current, &fref))
                {
                    self.unrepresented_publications.remove(position);
                }
            }
            if let Ok(position) = self
                .publication_collisions
                .binary_search_by(|row| row.symbol.as_str().cmp(symbol.as_str()))
            {
                self.publication_collisions.remove(position);
            }
            for instance in &users {
                if !g.instances.contains(instance) {
                    let fref = instance.func;
                    let position = self
                        .unrepresented_publications
                        .binary_search_by(|current| func_ref_order(current, &fref))
                        .unwrap_or_else(|position| position);
                    self.unrepresented_publications.insert(position, fref);
                }
            }
            let represented: Vec<Instance> = users
                .iter()
                .filter(|instance| g.instances.contains(*instance))
                .cloned()
                .collect();
            rows_examined += represented.len();
            if represented.len() > 1 {
                let row = PublicationCollision {
                    symbol: symbol.clone(),
                    instances: represented,
                };
                let position = self
                    .publication_collisions
                    .binary_search_by(|current| current.symbol.cmp(symbol))
                    .unwrap_or_else(|position| position);
                self.publication_collisions.insert(position, row);
            } else if let Some(instance) = represented.into_iter().next() {
                self.publications.insert(instance, symbol.clone());
            }
        }
        rows_examined
    }

    fn refresh_excluded_local(&mut self, changed: &BTreeSet<Instance>) -> usize {
        let rows_examined = changed.len();
        for instance in changed {
            if let Ok(position) = self
                .excluded
                .binary_search_by(|row| row.instance.cmp(instance))
            {
                self.excluded.remove(position);
            }
            if let Some(&cause) = self.index.exclusions.get(instance) {
                let row = Excluded {
                    instance: instance.clone(),
                    cause,
                };
                let position = self
                    .excluded
                    .binary_search_by(|current| current.instance.cmp(instance))
                    .unwrap_or_else(|position| position);
                self.excluded.insert(position, row);
            }
        }
        rows_examined
    }

    fn refresh_severed_local(&mut self, changed_callers: &BTreeSet<Instance>) -> usize {
        let mut rows_examined = changed_callers.len();
        for caller in changed_callers {
            while let Ok(position) = self.severed.binary_search_by(|row| row.caller.cmp(caller)) {
                self.severed.remove(position);
            }
            if let Some(rows) = self.index.severed_by_caller.get(caller) {
                rows_examined += rows.len();
                for row in rows {
                    let position = self
                        .severed
                        .binary_search_by(|current| severed_order(current, row))
                        .unwrap_or_else(|position| position);
                    self.severed.insert(position, row.clone());
                }
            }
        }
        rows_examined
    }

    fn refresh_symbol_collisions_local(&mut self, changed_symbols: &BTreeSet<String>) -> usize {
        let mut rows_examined = changed_symbols.len();
        for symbol in changed_symbols {
            if let Ok(position) = self
                .symbol_collisions
                .binary_search_by(|row| row.symbol.cmp(symbol))
            {
                self.symbol_collisions.remove(position);
            }
            let Some(users) = self.index.symbol_users.get(symbol) else {
                continue;
            };
            rows_examined += users.len();
            if users.len() < 2 {
                continue;
            }
            let mut instances: Vec<Instance> = users.iter().cloned().collect();
            instances.sort();
            let row = SymbolCollision {
                symbol: symbol.clone(),
                instances,
            };
            let position = self
                .symbol_collisions
                .binary_search_by(|current| current.symbol.cmp(symbol))
                .unwrap_or_else(|position| position);
            self.symbol_collisions.insert(position, row);
        }
        rows_examined
    }

    /// Compare every realized output except the target description itself.
    pub fn same_cpu_decisions(&self, other: &Self) -> bool {
        self.roots == other.roots
            && self.declared_roots == other.declared_roots
            && self.fn_value_targets == other.fn_value_targets
            && self.instances == other.instances
            && self.boundaries == other.boundaries
            && self.realization == other.realization
            && self.excluded == other.excluded
            && self.severed == other.severed
            && self.sites_walked == other.sites_walked
            && self.symbols == other.symbols
            && self.symbol_collisions == other.symbol_collisions
            && self.inline_deps == other.inline_deps
            && self.unresolved_inline_deps == other.unresolved_inline_deps
            && self.opaque_boundary_roots == other.opaque_boundary_roots
            && self.publisher_lanes == other.publisher_lanes
            && self.missing_substitutions == other.missing_substitutions
            && self.shared_instances == other.shared_instances
            && self.deferred == other.deferred
            && self.publications == other.publications
            && self.unrepresented_publications == other.unrepresented_publications
            && self.publication_collisions == other.publication_collisions
    }

    /// Every retained instance -- Rock bodies AND boundaries. The positive side
    /// of the partition, and what `symbols` is total over: a boundary still
    /// needs a spelling, it just has no body to traverse.
    pub fn retained(&self) -> impl Iterator<Item = &Instance> {
        self.realization.keys()
    }

    /// Whether this artifact retains `i`, however it is supplied.
    pub fn retains(&self, i: &Instance) -> bool {
        self.realization.contains_key(i)
    }

    pub fn census(&self) -> ArtifactCensus {
        let mut c = ArtifactCensus {
            instances: self.instances.len(),
            boundaries: self.boundaries.len(),
            retained: self.realization.len(),
            roots: self.roots.len(),
            shared_instances: self.shared_instances,
            excluded: self.excluded.len(),
            sites_walked: self.sites_walked,
            severed: self.severed.len(),
            symbols: self.symbols.len(),
            symbol_collisions: self.symbol_collisions.len(),
            inline_deps_examined: self.inline_deps.len() + self.unresolved_inline_deps.len(),
            inline_deps_unresolved: self.unresolved_inline_deps.len(),
            inline_deps_boundary: self
                .inline_deps
                .iter()
                .filter(|d| matches!(d.callee, RealizedCallee::Boundary(_)))
                .count(),
            inline_deps_rock: self
                .inline_deps
                .iter()
                .filter(|d| matches!(d.callee, RealizedCallee::Rock(_)))
                .count(),
            foreign_bodies: self.publisher_lanes.len(),
            multi_lane_publishers: self.publisher_lanes.values().filter(|n| **n > 1).count(),
            widest_result_lanes: self.publisher_lanes.values().copied().max().unwrap_or(0),
            missing_substitutions: self.missing_substitutions.len(),
            publications: self.publications.len(),
            unrepresented_publications: self.unrepresented_publications.len(),
            publication_collisions: self.publication_collisions.len(),
            deferred: self.deferred.clone(),
            ..Default::default()
        };
        for e in &self.excluded {
            *c.excluded_by_cause.entry(e.cause.label()).or_insert(0) += 1;
        }
        for r in self.realization.values() {
            *c.realized_by_kind.entry(r.kind.label()).or_insert(0) += 1;
        }
        for s in &self.severed {
            *c.severed_by_cause.entry(s.cause.label()).or_insert(0) += 1;
        }
        c
    }

    /// The severed population grouped by `(callee declaration, cause)`, widest
    /// first. A PARTITION of `severed`: `sites` sums to it.
    pub fn severed_groups(&self) -> Vec<SeveredGroup> {
        let mut by: BTreeMap<(DeclKey, Unrealizable), SeveredGroup> = BTreeMap::new();
        for s in &self.severed {
            let e = by
                .entry((DeclKey(s.callee.func), s.cause))
                .or_insert_with(|| SeveredGroup {
                    callee: s.callee.func,
                    cause: s.cause,
                    sites: 0,
                    instances: BTreeSet::new(),
                    callers: BTreeSet::new(),
                });
            e.sites += 1;
            e.instances.insert(s.callee.clone());
            e.callers.insert(s.caller.clone());
        }
        let mut groups: Vec<SeveredGroup> = by.into_values().collect();
        groups.sort_by(|a, b| {
            b.sites
                .cmp(&a.sites)
                .then(DeclKey(a.callee).cmp(&DeclKey(b.callee)))
        });
        groups
    }

    pub fn render_symbol_collisions(&self, ws: &Workspace) -> Vec<String> {
        self.symbol_collisions
            .iter()
            .map(|c| {
                let names: Vec<String> = c
                    .instances
                    .iter()
                    .map(|i| {
                        let f = &ws.module(i.func.module).functions[i.func.func];
                        format!("{} [{:?}]", f.name, f.backend)
                    })
                    .collect();
                format!("`{}` <- {}", c.symbol, names.join(", "))
            })
            .collect()
    }

    pub fn render_unresolved_inline_deps(&self, ws: &Workspace) -> Vec<String> {
        self.unresolved_inline_deps
            .iter()
            .map(|d| {
                let f = &ws.module(d.owner.module).functions[d.owner.func];
                format!("`{}` names `{}`: {}", f.name, d.entry, d.reason)
            })
            .collect()
    }

    pub fn render_severed(&self, ws: &Workspace, g: &InstanceGraph) -> Vec<String> {
        self.severed
            .iter()
            .map(|s| {
                format!(
                    "{} {} -> {} ({})",
                    g.identity(ws, &s.caller),
                    s.site.op.name(),
                    g.identity(ws, &s.callee),
                    s.cause.label()
                )
            })
            .collect()
    }
}

/// How this artifact realizes `f`, or why it cannot.
///
/// **The intrinsic arm does NOT route through
/// [`instance_graph::boundary_class_for`].** That classifier answers "does the
/// WALK enter this body", and it folds `@native_required` into its own answer:
/// a `@native_required` intrinsic is a boundary there precisely BECAUSE it is
/// `@native_required`. Asking it here and then also asking
/// `is_native_required` is a proxy for the same fact, so the second question
/// can never disagree with the first -- measured: deleting the
/// `is_native_required` arm changed no test, because the reject fell out of the
/// classifier that had already consulted it.
///
/// The artifact asks its own question in its own order, which is
/// `skull_mir::intrinsics`' emit resolution: **backend mapping > portable
/// fallback > reject**, with `@native_required` skipping the middle rung. That
/// is the only order under which a view cannot answer "available" where
/// emission would reject.
fn realize_decl(
    ws: &Workspace,
    rep: FuncRef,
    erased: &BTreeSet<String>,
    target: &CpuTargetSpec,
    mappings: &ArtifactMappings<'_>,
) -> Result<Realized, Unrealizable> {
    // THE BODY-INDEPENDENT RUNGS, first, and IN THE EMITTER'S OWN ORDER. Each
    // answers without emitting any declaration's body, so "which backend wrote
    // the source body" has no consumer -- and asking for a sibling ahead of
    // them rejects work the artifact actually performs. Measured on the JS
    // corpus: `nb_get` (1,127 sites) is mapped by `JsIntrinsics` and
    // `rock_shader` is expanded at MIR lowering, and both were
    // `foreign-to-artifact` because the sibling question came first.
    //
    // The ORDER is not a preference either. `skull_codegen_js`'s callee walk
    // skips a host-bound callee BEFORE it reaches the `inline(js)` co-emit and
    // plain-Rock branches, and `render_call` reaches for a host template before
    // the intrinsic table -- so a host binding wins over the declaration's own
    // body, whatever that body is. `map_new` is the case that makes this
    // load-bearing: it is a PURE ROCK body the JS backend renders as
    // `new Map()`, so classifying it by its body would have the artifact
    // traverse a body emission never emits. The portable-fallback rung stays
    // below with the selected declaration, because a fallback IS a Rock body.
    let rep_f = &ws.module(rep.module).functions[rep.func];
    if intrinsics::expand_kind(rep_f).is_some() && supports(target, BoundaryClass::Intrinsic) {
        return Ok(Realized {
            decl: rep,
            kind: Realization::CompileTimeExpansion,
        });
    }
    if supports(target, BoundaryClass::Intrinsic) && mappings.supplies(ws, rep_f) {
        // ONE rung, ONE oracle. `@intrinsic` or not only decides which LABEL
        // the census carries -- both are the same backend-supplied contract
        // (`Realization::boundary_class`), and asking the two questions
        // separately is what let `map_new` reach a body branch.
        let kind = if intrinsics::is_intrinsic(rep_f) {
            Realization::NativeMapping
        } else {
            Realization::HostBinding
        };
        return Ok(Realized { decl: rep, kind });
    }

    // SIBLING SELECTION, and this is the only place it happens. `rep` is the
    // shared graph's target-neutral representative; `backend_variant` is
    // production's own selector, so the declaration this artifact emits is the
    // one lowering would pick. `Err` -- nothing in the group serves this
    // artifact -- is the `@nomangle`-sibling case: a JS body asked of a Lua
    // artifact.
    let selected: Result<FuncRef, Unrealizable> = match target.execution.backend {
        Some(b) => {
            let name = ws.module(rep.module).functions[rep.func].name;
            skull_mir::lower::backend_variant(ws, name, rep, b)
                .map_err(|_| Unrealizable::ForeignToArtifact)
        }
        // A target with no Rock-level backend of its own names no host, so there
        // is no sibling to select and only a declaration that names none --
        // `FnBackend::Any`, a portable Rock body -- serves it. Deliberately NOT
        // `serves(f.backend, Any)`, which answers yes to everything: that is the
        // "any target" convenience and would keep `extern(lua) fn print` on a C
        // artifact.
        None => {
            if ws.module(rep.module).functions[rep.func].backend != FnBackend::Any {
                Err(Unrealizable::ForeignToArtifact)
            } else {
                Ok(rep)
            }
        }
    };
    selected.and_then(|decl| {
        let f = &ws.module(decl.module).functions[decl.func];
        realize_body(ws, decl, f, erased, target, mappings).map(|kind| Realized { decl, kind })
    })
}

/// How the SELECTED declaration is supplied. Split from the selection above so
/// the two questions -- which declaration, and what kind of body it has -- cannot
/// be answered against different `FuncRef`s.
fn realize_body(
    ws: &Workspace,
    decl: FuncRef,
    f: &Function,
    erased: &BTreeSet<String>,
    target: &CpuTargetSpec,
    mappings: &ArtifactMappings<'_>,
) -> Result<Realization, Unrealizable> {
    // An erased runtime entry is an IDENTITY, not a body kind: S 1.4's contract
    // is that the entry exists under its bare name for every target skull
    // emits, so it is classified before anything reads a body slot.
    if erased.contains(f.name.as_str()) {
        return class_or_reject(target, BoundaryClass::ErasedRuntimeEntry);
    }
    if f.is_extern {
        // `extern(js)` names the browser/runtime surface, not a host C symbol;
        // the JS artifact owns that boundary even though the web platform does
        // not expose the desktop foreign-symbol capability.
        if f.backend == FnBackend::Js && target.execution.backend == Some(FnBackend::Js) {
            return Ok(Realization::ForeignBody(BoundaryClass::Extern));
        }
        return class_or_reject(target, BoundaryClass::Extern);
    }
    if intrinsics::is_intrinsic(f) {
        if !supports(target, BoundaryClass::Intrinsic) {
            return Err(Unrealizable::ClassUnsupported(BoundaryClass::Intrinsic));
        }
        // An expansion intrinsic has no callable body on ANY backend -- its
        // emission is compile-time at MIR lowering -- so neither the mapping
        // nor the fallback question applies to it.
        if intrinsics::expand_kind(f).is_some() {
            return Ok(Realization::CompileTimeExpansion);
        }
        if mappings.maps(ws, f) {
            return Ok(Realization::NativeMapping);
        }
        // THE discriminant. Asked of the declaration, before the fallback rung,
        // and never satisfied by the presence of a Rock body.
        if intrinsics::is_native_required(f) {
            return Err(Unrealizable::NativeMappingMissing);
        }
        if has_rock_body(ws, decl) {
            return Ok(Realization::PortableFallback);
        }
        return Err(Unrealizable::NativeMappingMissing);
    }
    if f.inline_body.is_some() {
        return class_or_reject(target, BoundaryClass::InlineBody);
    }
    Ok(Realization::RockBody)
}

fn class_or_reject(
    target: &CpuTargetSpec,
    class: BoundaryClass,
) -> Result<Realization, Unrealizable> {
    if supports(target, class) {
        Ok(Realization::ForeignBody(class))
    } else {
        Err(Unrealizable::ClassUnsupported(class))
    }
}

/// Whether the module actually HOLDS a Rock body for `decl`.
///
/// `instance_graph::has_rock_body` reads the declaration only -- "no foreign
/// body" -- which answers TRUE for an expansion intrinsic that has no body at
/// all. The portable rung needs the body itself.
fn has_rock_body(ws: &Workspace, decl: FuncRef) -> bool {
    ws.module(decl.module)
        .bodies
        .contains_key(&bone_semantic::hir::BodyOwner::Function(decl.func))
}

/// Realize the shared `g` for `req`. Read-only in the type system, not just by
/// convention: `g` is `&`, so no discovery can happen here.
///
/// Every artifact realizes the SAME graph. There is no per-target build and
/// therefore no mismatch to detect -- backend-sibling selection lives in
/// [`realize_decl`], which is reached from here and from nowhere else.
pub fn realize(ws: &Workspace, g: &InstanceGraph, req: &ArtifactRequest<'_>) -> ArtifactView {
    #[cfg(feature = "test-hooks")]
    REALIZE_INVOCATIONS.with(|count| count.set(count.get() + 1));
    // Shader packaging is absent from this value by construction. Platform
    // capabilities remain available for host boundary admission.
    let target = req.spec.cpu();
    // Realizability of every instance the graph holds, computed once. It is a
    // property of the DECLARATION and the artifact, independent of reachability.
    let mut realizable: FxHashMap<Instance, Result<Realized, Unrealizable>> = FxHashMap::default();
    for i in &g.instances {
        realizable.insert(
            i.clone(),
            realize_decl(ws, i.func, &g.erased_entries, &target, &req.mappings),
        );
    }

    // The artifact's ROOTS: its emission roots that are realizable here, plus
    // the callees of always-emitted bodies, which carry no incoming edge and
    // would otherwise read as unreachable.
    //
    // Canonicalized, because an instance names the group REPRESENTATIVE while
    // production hands over whichever declaration it discovered. A root given as
    // a backend sibling would otherwise match nothing.
    let root_decls: BTreeSet<DeclKey> = req
        .roots
        .iter()
        .map(|r| DeclKey(skull_mir::lower::canonical_variant(ws, *r)))
        .collect();
    let mut index = ArtifactIndex {
        realizable,
        root_decls: root_decls.clone(),
        ..Default::default()
    };
    for (owner, sites) in g.call_sites() {
        #[cfg(feature = "test-hooks")]
        FRESH_INDEX_SITE_ROWS.with(|count| count.set(count.get() + sites.len() as u64));
        index.site_counts.insert(owner.clone(), sites.len());
        for (site, record) in sites {
            let SiteOutcome::Resolved(callee) = &record.outcome else {
                continue;
            };
            index
                .outgoing
                .entry(owner.clone())
                .or_default()
                .insert(site.clone(), callee.clone());
            index
                .incoming
                .entry(callee.clone())
                .or_default()
                .insert(owner.clone());
            if site.op == skull_mir::site::SiteOp::FnValue {
                index
                    .fn_values_by_caller
                    .entry(owner.clone())
                    .or_default()
                    .insert(callee.clone());
                index
                    .fn_value_users
                    .entry(callee.clone())
                    .or_default()
                    .insert(owner.clone());
            }
        }
    }
    let mut roots: BTreeSet<Instance> = BTreeSet::new();
    let mut declared_roots: BTreeSet<Instance> = BTreeSet::new();
    for i in &g.instances {
        let declared = root_decls.contains(&DeclKey(i.func));
        let is_root = declared || g.is_always_emitted_target(i);
        if is_root && index.realizable[i].is_ok() {
            roots.insert(i.clone());
            if declared {
                declared_roots.insert(i.clone());
            }
        }
    }

    // Reachability over the walk's OWN record of what each site resolved to.
    // Not over `g.edges`: an edge says two instances are related, a site says
    // WHICH operation related them, which is what a severed call has to name.
    let mut instances: BTreeSet<Instance> = BTreeSet::new();
    let mut realization: BTreeMap<Instance, Realized> = BTreeMap::new();
    let mut severed: Vec<Severed> = Vec::new();
    let mut sites_walked = 0usize;
    let mut queue: VecDeque<Instance> = VecDeque::new();

    // Admitting one reached instance. EVERY reached callee goes through here --
    // roots and call sites alike -- so plan S 9's `Rock` / `Boundary` split is
    // made once. Only a Rock body is ENQUEUED: a boundary has no body the
    // artifact lowers, which is the same rule the graph walk follows when it
    // stops at a boundary rather than enumerating its callees.
    //
    // The boundary's IDENTITY is not minted here. It needs this artifact's
    // spelling, which is the planner's answer over the full retained set, so it
    // is built once below -- see `boundaries`.
    let mut admit = |i: &Instance,
                     instances: &mut BTreeSet<Instance>,
                     realization: &mut BTreeMap<Instance, Realized>,
                     queue: &mut VecDeque<Instance>| {
        let r = *index.realizable[i]
            .as_ref()
            .expect("admitted means realizable");
        if realization.insert(i.clone(), r).is_some() {
            return;
        }
        index
            .realization_by_decl
            .entry(r.decl)
            .or_default()
            .insert(i.clone());
        index
            .instances_by_decl
            .entry(i.func)
            .or_default()
            .insert(i.clone());
        if r.kind.boundary_class().is_none() {
            instances.insert(i.clone());
            queue.push_back(i.clone());
        }
    };

    for r in &roots {
        admit(r, &mut instances, &mut realization, &mut queue);
    }
    let mut fn_value_targets: BTreeSet<Instance> = BTreeSet::new();
    while let Some(caller) = queue.pop_front() {
        sites_walked += index.site_counts.get(&caller).copied().unwrap_or(0);
        let Some(sites) = index.outgoing.get(&caller) else {
            continue;
        };
        for (site, callee) in sites {
            match index.realizable.get(callee) {
                Some(Ok(_)) => {
                    if site.op == skull_mir::site::SiteOp::FnValue {
                        fn_value_targets.insert(callee.clone());
                    }
                    admit(callee, &mut instances, &mut realization, &mut queue)
                }
                Some(Err(cause)) => {
                    let row = Severed {
                        caller: caller.clone(),
                        site: site.clone(),
                        callee: callee.clone(),
                        cause: *cause,
                    };
                    index
                        .severed_by_caller
                        .entry(caller.clone())
                        .or_default()
                        .push(row.clone());
                    severed.push(row);
                }
                // The walk resolved a callee the graph does not hold. Already a
                // `ConcreteBody` finding (`unknown_rock_callees`); nothing to
                // realize here.
                None => {}
            }
        }
    }
    severed.sort_by(severed_order);
    for rows in index.severed_by_caller.values_mut() {
        rows.sort_by(severed_order);
    }

    // The partition: every shared instance is a retained Rock instance, a
    // retained boundary, unrealizable, or unreachable. Reported as a partition
    // so a lost instance cannot hide.
    let excluded: Vec<Excluded> = g
        .instances
        .iter()
        .filter(|i| !realization.contains_key(*i))
        .map(|i| Excluded {
            instance: i.clone(),
            cause: match index.realizable[i] {
                Ok(_) => Exclusion::Unreachable,
                Err(u) => Exclusion::Unrealizable(u),
            },
        })
        .collect();
    for e in &excluded {
        index.exclusions.insert(e.instance.clone(), e.cause);
    }

    // One symbol per retained instance, from THIS artifact's planner.
    let mut by_symbol: BTreeMap<String, Vec<Instance>> = BTreeMap::new();
    let mut symbols = BTreeMap::new();
    for i in realization.keys() {
        let s = req.symbols.symbol(ws, g, i, realization[i]);
        by_symbol.entry(s.clone()).or_default().push(i.clone());
        index
            .symbol_users
            .entry(s.clone())
            .or_default()
            .insert(i.clone());
        symbols.insert(i.clone(), s);
    }
    let symbol_collisions: Vec<SymbolCollision> = by_symbol
        .into_iter()
        .filter(|(_, v)| v.len() > 1)
        .map(|(symbol, instances)| SymbolCollision { symbol, instances })
        .collect();

    // THE boundary identities, minted once, per retained instance. Every
    // consumer -- an ordinary call site and an opaque dependency alike -- reads
    // this map rather than constructing an id of its own, so an artifact cannot
    // name one boundary two ways. The symbol is the PLANNER's, which is what
    // makes a renaming planner visible here instead of silently ignored.
    let boundaries: BTreeMap<Instance, BoundaryId> = realization
        .iter()
        .filter_map(|(i, r)| {
            r.kind.boundary_class().map(|class| {
                (
                    i.clone(),
                    BoundaryId {
                        declaration: r.decl,
                        class,
                        target,
                        symbol: symbols[i].clone(),
                    },
                )
            })
        })
        .collect();

    // Opaque dependencies, terminating in a concrete callee of THIS artifact.
    let mut inline_deps = Vec::new();
    let mut unresolved_inline_deps = Vec::new();
    let retained_decls: BTreeSet<DeclKey> = index
        .realization_by_decl
        .keys()
        .copied()
        .map(DeclKey)
        .collect();
    for (owner, entries) in &g.inline_body_deps {
        let owner_retained = retained_decls.contains(owner);
        for entry in entries {
            let Some(rep) = instance_graph::stdlib_entry_representative(ws, entry) else {
                continue;
            };
            let dependency = DeclKey(rep);
            index
                .inline_consumers
                .entry(dependency)
                .or_default()
                .insert(*owner);
            index
                .inline_dependencies_by_owner
                .entry(*owner)
                .or_default()
                .insert(dependency);
        }
        // An owner this artifact dropped carries no obligation: its text is
        // never emitted here.
        if !owner_retained {
            continue;
        }
        for entry in entries {
            // The REPRESENTATIVE, because that is what an instance is keyed by.
            // Which declaration serves this artifact was already decided by
            // `realize_decl`, and asking a target-specific lookup here would be
            // a second selector answering the same question.
            let Some(rep) = instance_graph::stdlib_entry_representative(ws, entry) else {
                unresolved_inline_deps.push(UnresolvedInlineDep {
                    owner: owner.0,
                    entry,
                    reason: "no single declaration supplies this entry",
                });
                continue;
            };
            // WHICH instance, not merely which declaration. A runtime entry is
            // non-generic so one instance is expected, but picking the first of
            // several would silently choose.
            let Some(instances) = index.instances_by_decl.get(&rep) else {
                unresolved_inline_deps.push(UnresolvedInlineDep {
                    owner: owner.0,
                    entry,
                    reason: "the serving declaration is not retained by the artifact",
                });
                continue;
            };
            let mut matching = instances.iter();
            let Some(inst) = matching.next() else {
                unresolved_inline_deps.push(UnresolvedInlineDep {
                    owner: owner.0,
                    entry,
                    reason: "the serving declaration is not retained by the artifact",
                });
                continue;
            };
            if matching.next().is_some() {
                unresolved_inline_deps.push(UnresolvedInlineDep {
                    owner: owner.0,
                    entry,
                    reason: "the serving declaration has more than one retained instance",
                });
                continue;
            }
            let inst = inst.clone();
            let callee = match boundaries.get(&inst) {
                Some(id) => RealizedCallee::Boundary(id.clone()),
                None => RealizedCallee::Rock(inst),
            };
            inline_deps.push(InlineDep {
                owner: owner.0,
                entry,
                callee,
            });
        }
    }
    inline_deps.sort_by(inline_dep_order);
    unresolved_inline_deps.sort_by(unresolved_inline_dep_order);
    for dep in &inline_deps {
        index
            .inline_by_owner
            .entry(DeclKey(dep.owner))
            .or_default()
            .0
            .push(dep.clone());
    }
    for dep in &unresolved_inline_deps {
        index
            .inline_by_owner
            .entry(DeclKey(dep.owner))
            .or_default()
            .1
            .push(dep.clone());
    }

    // The result-lane width of every foreign body this artifact emits. Asked of
    // the SELECTED declaration -- the one whose body is emitted -- under this
    // artifact's own execution backend.
    let scalar_context = std::cell::OnceCell::new();
    let scx = req.scalar_context(ws, g, &scalar_context);
    // ...under THAT INSTANCE's own substitution, which is what makes the width
    // a property of the instance and not of the declaration: `-> T` is one
    // boxed slot on the declaration and two lanes at `T = (bool, float)`, and
    // `publisher_lanes` is keyed by instance so both answers coexist.
    //
    // The only thing that can leave an instance unanswered is the graph having
    // no record at all -- an invariant finding, below. There is no "generic, so
    // unknown" arm to be in.
    let mut publisher_lanes: BTreeMap<Instance, usize> = BTreeMap::new();
    if let Some(scx) = &scx {
        for (i, r) in &realization {
            if !matches!(r.kind, Realization::ForeignBody(BoundaryClass::InlineBody)) {
                continue;
            }
            // The graph must have a record, or this instance is the invariant
            // finding below rather than a lane width. The plan is then built
            // POSITIONALLY, from the instance's own arguments against the
            // SELECTED declaration's generics -- the same route every emitted
            // body's plan takes. It used to pass the recorded substitution
            // instead, which is keyed by `i.func`'s `VarId`s and applied to a
            // possibly-different backend sibling; the declaration supplies the
            // order, and a sibling is chosen for having the same signature.
            if g.substitution(i).is_none() {
                continue;
            }
            let selected = Instance {
                func: r.decl,
                args: i.args.clone(),
            };
            debug_assert_eq!(
                ws.module(selected.func.module).functions[selected.func.func]
                    .generics
                    .len(),
                ws.module(i.func.module).functions[i.func.func]
                    .generics
                    .len(),
                "a backend sibling must declare the same generics as the instance it serves",
            );
            let signature = crate::concrete_body::concrete_signature_for_instance(ws, g, i)
                .unwrap_or_else(|reason| {
                    panic!(
                        "retained inline body {:?} has no concrete publisher signature: {reason:?}",
                        i
                    )
                });
            publisher_lanes.insert(
                i.clone(),
                skull_mir::lower::BackendFnAbiPlan::for_concrete_signature(
                    ws, scx, &selected, &signature,
                )
                .result_lanes(),
            );
        }
    }

    // The invariant finding, over EVERY retained instance rather than only the
    // foreign-body slice: a producer that bypassed `reach`'s recorder is a
    // graph defect wherever it lands, and the lane question is just the first
    // consumer that would have read the hole as "concrete".
    let missing_substitutions: BTreeSet<Instance> = realization
        .keys()
        .filter(|i| g.concreteness(i) == Concreteness::Missing)
        .cloned()
        .collect();

    // **Bare-name PUBLICATION obligations** -- see `ArtifactView::publications`.
    // An explicit `@nomangle` root's promise is about the EXACT declaration a
    // program wrote, never about whichever sibling `backend_variant` would
    // pick for a resolved call; ordinary call-serving realization above
    // (`realizable`/`realization`) is untouched by this, and
    // `backend_variant_group`/`canonical_variant` are not consulted here at
    // all -- this is a second, independent obligation over the SAME graph, not
    // a second discovery walk: the exact instance's own contribution, if the
    // graph has one, is what `g.instances`/`g.sites_of` already recorded.
    let publication_candidates = publication_candidates(ws, req);
    for (instance, symbol) in &publication_candidates {
        index
            .publication_users
            .entry(symbol.clone())
            .or_default()
            .insert(instance.clone());
        index
            .publication_decls
            .entry(DeclKey(instance.func))
            .or_default()
            .insert(instance.clone());
        index
            .publication_candidates
            .insert(instance.clone(), symbol.clone());
    }
    let mut by_pub_symbol: BTreeMap<String, Vec<Instance>> = BTreeMap::new();
    let mut unrepresented_publications: Vec<FuncRef> = Vec::new();
    for (inst, symbol) in &publication_candidates {
        if !g.instances.contains(inst) {
            unrepresented_publications.push(inst.func);
            continue;
        }
        by_pub_symbol
            .entry(symbol.clone())
            .or_default()
            .push(inst.clone());
    }
    let mut publications: BTreeMap<Instance, String> = BTreeMap::new();
    let mut publication_collisions: Vec<PublicationCollision> = Vec::new();
    for (symbol, mut instances) in by_pub_symbol {
        instances.sort();
        instances.dedup();
        if instances.len() > 1 {
            publication_collisions.push(PublicationCollision { symbol, instances });
        } else {
            publications.insert(
                instances.into_iter().next().expect("non-empty group"),
                symbol,
            );
        }
    }
    unrepresented_publications.sort_by(func_ref_order);

    let opaque_boundary_roots = g
        .inline_body_nomangle_roots
        .iter()
        .filter(|(owner, _)| retained_decls.contains(owner))
        .map(|(owner, targets)| (*owner, targets.clone()))
        .collect();

    ArtifactView {
        shared_instances: g.instances.len(),
        publisher_lanes,
        missing_substitutions,
        publications,
        unrepresented_publications,
        publication_collisions,
        // An execution-axis question too: every obligation it names is about
        // memory, coroutines, widths or foreign symbols. A shader-derived
        // deferral would be a separate list on a separate axis.
        deferred: crate::target_projection::deferred_for(&target.execution),
        spec: req.spec,
        roots,
        declared_roots,
        fn_value_targets,
        instances,
        boundaries,
        realization,
        excluded,
        severed,
        sites_walked,
        symbols,
        symbol_collisions,
        inline_deps,
        unresolved_inline_deps,
        opaque_boundary_roots,
        index,
    }
}
