//! A target's ARTIFACT VIEW of the shared instance graph
//! (`docs/monomorphization-plan.md` S 9, S 11).
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
use skull_mir::intrinsics::{self, IntrinsicTable};
use skull_mir::mir::FuncRef;

use crate::concrete_body::BoundaryId;
use crate::instance_graph::{self, BoundaryClass, DeclKey, Instance, InstanceGraph, Site, SiteOutcome};
use crate::target_projection::{supports, ExecutionTargetSpec};

/// Where an artifact's NATIVE MAPPING for an `@intrinsic` fn comes from.
///
/// Two sources, both real: luajit's mapping IS the set of in-source
/// `inline(lua)` bodies (the body-slot rule -- `skull_mir::intrinsics`), and
/// GLSL/JS own a Rust [`IntrinsicTable`]. A target with neither maps nothing,
/// which is the honest answer for a synthetic fixture: it has no backend to
/// map with.
///
/// Deliberately NOT a `&Function -> bool` closure supplied per call site. The
/// question "does this artifact map this intrinsic natively" has exactly these
/// two answers today, and a closure would let a caller pass `|_| true`.
#[derive(Clone, Copy, Default)]
pub struct ArtifactMappings<'a> {
    /// The backend whose in-source body slot IS this artifact's mapping.
    pub inline_backend: Option<FnBackend>,
    /// This artifact's Rust-side table, when it has one.
    pub table: Option<&'a dyn IntrinsicTable>,
}

impl<'a> ArtifactMappings<'a> {
    /// A target that maps nothing natively.
    pub fn none() -> Self {
        Self::default()
    }

    pub fn for_backend(b: FnBackend) -> Self {
        Self { inline_backend: Some(b), table: None }
    }

    pub fn with_table(mut self, t: &'a dyn IntrinsicTable) -> Self {
        self.table = Some(t);
        self
    }

    /// Does this artifact map `f` NATIVELY? A Rock body is not consulted --
    /// that is the portable rung, a different answer to a different question.
    pub fn maps(&self, f: &Function) -> bool {
        if let Some(b) = self.inline_backend {
            if f.inline_body.is_some_and(|(be, _)| instance_graph::serves(be, b)) {
                return true;
            }
        }
        self.table.is_some_and(|t| t.lookup(f).is_some())
    }
}

/// What an artifact SPELLS an instance.
///
/// A seam, not indirection for its own sake: plan S 0h puts the emitted
/// spelling under the artifact, so "every artifact spells this instance alike"
/// is a property of the default planner and not of the model. A test planner
/// that renames deliberately is what keeps the two apart.
pub trait SymbolPlanner {
    fn symbol(&self, ws: &Workspace, g: &InstanceGraph, i: &Instance) -> String;
    fn describe(&self) -> &'static str;
}

/// The default: today's logical encoder (`spec_fn_name`), which is
/// target-neutral. Injective over the instances it is given, which is exactly
/// what it CANNOT answer for an artifact holding two declarations that encode
/// alike -- hence the per-artifact collision check.
pub struct LogicalSymbols;

impl SymbolPlanner for LogicalSymbols {
    fn symbol(&self, ws: &Workspace, g: &InstanceGraph, i: &Instance) -> String {
        g.logical_symbol(ws, i)
    }
    fn describe(&self) -> &'static str {
        "logical"
    }
}

/// Everything one artifact view is taken through.
pub struct ArtifactRequest<'a> {
    pub spec: ExecutionTargetSpec,
    pub mappings: ArtifactMappings<'a>,
    /// The artifact's emission roots. Plan S 11: "the target may change roots",
    /// so they are an INPUT, never re-derived here from the graph's contents.
    pub roots: &'a [FuncRef],
    pub symbols: &'a dyn SymbolPlanner,
}

impl<'a> ArtifactRequest<'a> {
    /// The default request for `spec`: its own backend's mapping, no table, and
    /// the logical spelling.
    pub fn new(spec: ExecutionTargetSpec, roots: &'a [FuncRef]) -> Self {
        Self {
            mappings: match spec.backend {
                Some(b) => ArtifactMappings::for_backend(b),
                None => ArtifactMappings::none(),
            },
            spec,
            roots,
            symbols: &LogicalSymbols,
        }
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
            Self::NativeMapping | Self::CompileTimeExpansion => {
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
#[derive(Clone, Debug)]
pub struct Excluded {
    pub instance: Instance,
    pub cause: Exclusion,
}

/// A call from a RETAINED instance whose concrete callee this artifact cannot
/// realize. The diagnostic a declaration filter structurally cannot produce:
/// the caller survives, so emission would emit a call to a body that is not in
/// the artifact.
#[derive(Clone, Debug)]
pub struct Severed {
    pub caller: Instance,
    pub site: Site,
    pub callee: Instance,
    pub cause: Unrealizable,
}

/// What a call in THIS artifact terminates in -- [`Callee`] with the boundary
/// arm REALIZED.
///
/// The shared form cannot carry a [`BoundaryId`]: an id names a target and an
/// emitted symbol, and a target-neutral body has neither. This is the same
/// split one rung down, not a second callee model.
#[derive(Clone, Debug)]
pub enum RealizedCallee {
    Rock(Instance),
    Boundary(BoundaryId),
}

/// One opaque-body dependency, terminating in a concrete callee.
#[derive(Clone, Debug)]
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
#[derive(Clone, Debug)]
pub struct UnresolvedInlineDep {
    pub owner: FuncRef,
    pub entry: &'static str,
    pub reason: &'static str,
}

/// Two retained instances that spell alike.
#[derive(Clone, Debug)]
pub struct SymbolCollision {
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
    /// Deferred obligations, carried from the target spec.
    pub deferred: Vec<&'static str>,
}

/// One target's realization of a graph it did not build.
pub struct ArtifactView {
    pub spec: ExecutionTargetSpec,
    /// The artifact's root instances -- realizable emission roots, plus the
    /// callees of always-emitted bodies, which have no incoming edge.
    pub roots: BTreeSet<Instance>,
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
    /// The shared population this view was taken over -- the denominator.
    pub shared_instances: usize,
    pub deferred: Vec<&'static str>,
}

impl ArtifactView {
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

    pub fn render_excluded(&self, ws: &Workspace, g: &InstanceGraph) -> Vec<String> {
        self.excluded
            .iter()
            .map(|e| format!("{} -- {}", g.identity(ws, &e.instance), e.cause.label()))
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
    spec: &ExecutionTargetSpec,
    mappings: &ArtifactMappings<'_>,
) -> Result<Realized, Unrealizable> {
    // SIBLING SELECTION, and this is the only place it happens. `rep` is the
    // shared graph's target-neutral representative; `backend_variant` is
    // production's own selector, so the declaration this artifact emits is the
    // one lowering would pick. `Err` -- nothing in the group serves this
    // artifact -- is the `@nomangle`-sibling case: a JS body asked of a Lua
    // artifact.
    let decl = match spec.backend {
        Some(b) => {
            let name = ws.module(rep.module).functions[rep.func].name;
            skull_mir::lower::backend_variant(ws, name, rep, b)
                .map_err(|_| Unrealizable::ForeignToArtifact)?
        }
        // A target with no Rock-level backend of its own names no host, so there
        // is no sibling to select and only a declaration that names none --
        // `FnBackend::Any`, a portable Rock body -- serves it. Deliberately NOT
        // `serves(f.backend, Any)`, which answers yes to everything: that is the
        // "any target" convenience and would keep `extern(lua) fn print` on a C
        // artifact.
        None => {
            if ws.module(rep.module).functions[rep.func].backend != FnBackend::Any {
                return Err(Unrealizable::ForeignToArtifact);
            }
            rep
        }
    };
    let f = &ws.module(decl.module).functions[decl.func];
    let kind = realize_body(ws, decl, f, erased, spec, mappings)?;
    Ok(Realized { decl, kind })
}

/// How the SELECTED declaration is supplied. Split from the selection above so
/// the two questions -- which declaration, and what kind of body it has -- cannot
/// be answered against different `FuncRef`s.
fn realize_body(
    ws: &Workspace,
    decl: FuncRef,
    f: &Function,
    erased: &BTreeSet<String>,
    spec: &ExecutionTargetSpec,
    mappings: &ArtifactMappings<'_>,
) -> Result<Realization, Unrealizable> {
    // An erased runtime entry is an IDENTITY, not a body kind: S 1.4's contract
    // is that the entry exists under its bare name for every target skull
    // emits, so it is classified before anything reads a body slot.
    if erased.contains(f.name.as_str()) {
        return class_or_reject(spec, BoundaryClass::ErasedRuntimeEntry);
    }
    if f.is_extern {
        return class_or_reject(spec, BoundaryClass::Extern);
    }
    if intrinsics::is_intrinsic(f) {
        if !supports(&spec.caps, BoundaryClass::Intrinsic) {
            return Err(Unrealizable::ClassUnsupported(BoundaryClass::Intrinsic));
        }
        // An expansion intrinsic has no callable body on ANY backend -- its
        // emission is compile-time at MIR lowering -- so neither the mapping
        // nor the fallback question applies to it.
        if intrinsics::expand_kind(f).is_some() {
            return Ok(Realization::CompileTimeExpansion);
        }
        if mappings.maps(f) {
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
        return class_or_reject(spec, BoundaryClass::InlineBody);
    }
    Ok(Realization::RockBody)
}

fn class_or_reject(
    spec: &ExecutionTargetSpec,
    class: BoundaryClass,
) -> Result<Realization, Unrealizable> {
    if supports(&spec.caps, class) {
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
    let spec = req.spec;
    // A target with no `FnBackend` of its own selects no variant at all. Stated
    // rather than defaulted: `Any` is not "this target", it is "no host".
    let want = spec.backend.unwrap_or(FnBackend::Any);

    // Realizability of every instance the graph holds, computed once. It is a
    // property of the DECLARATION and the artifact, independent of reachability.
    let mut realizable: BTreeMap<Instance, Result<Realized, Unrealizable>> = BTreeMap::new();
    for i in &g.instances {
        realizable
            .insert(i.clone(), realize_decl(ws, i.func, &g.erased_entries, &spec, &req.mappings));
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
    let mut roots: BTreeSet<Instance> = BTreeSet::new();
    for i in &g.instances {
        let is_root = root_decls.contains(&DeclKey(i.func))
            || g.always_emitted_targets.contains(i);
        if is_root && realizable[i].is_ok() {
            roots.insert(i.clone());
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
    let admit = |i: &Instance,
                 instances: &mut BTreeSet<Instance>,
                 realization: &mut BTreeMap<Instance, Realized>,
                 queue: &mut VecDeque<Instance>| {
        let r = *realizable[i].as_ref().expect("admitted means realizable");
        if realization.insert(i.clone(), r).is_some() {
            return;
        }
        if r.kind.boundary_class().is_none() {
            instances.insert(i.clone());
            queue.push_back(i.clone());
        }
    };

    for r in &roots {
        admit(r, &mut instances, &mut realization, &mut queue);
    }
    while let Some(caller) = queue.pop_front() {
        let Some(sites) = g.call_sites.get(&caller) else { continue };
        for (site, rec) in sites {
            sites_walked += 1;
            let SiteOutcome::Resolved(callee) = &rec.outcome else { continue };
            match realizable.get(callee) {
                Some(Ok(_)) => admit(callee, &mut instances, &mut realization, &mut queue),
                Some(Err(cause)) => severed.push(Severed {
                    caller: caller.clone(),
                    site: site.clone(),
                    callee: callee.clone(),
                    cause: *cause,
                }),
                // The walk resolved a callee the graph does not hold. Already a
                // `ConcreteBody` finding (`unknown_rock_callees`); nothing to
                // realize here.
                None => {}
            }
        }
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
            cause: match realizable[i] {
                Ok(_) => Exclusion::Unreachable,
                Err(u) => Exclusion::Unrealizable(u),
            },
        })
        .collect();

    // One symbol per retained instance, from THIS artifact's planner.
    let mut by_symbol: BTreeMap<String, Vec<Instance>> = BTreeMap::new();
    let mut symbols = BTreeMap::new();
    for i in realization.keys() {
        let s = req.symbols.symbol(ws, g, i);
        by_symbol.entry(s.clone()).or_default().push(i.clone());
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
                        target: want,
                        symbol: symbols[i].clone(),
                    },
                )
            })
        })
        .collect();

    // Opaque dependencies, terminating in a concrete callee of THIS artifact.
    let mut inline_deps = Vec::new();
    let mut unresolved_inline_deps = Vec::new();
    let retained_decls: BTreeSet<DeclKey> =
        realization.values().map(|r| DeclKey(r.decl)).collect();
    for (owner, entries) in &g.inline_body_deps {
        // An owner this artifact dropped carries no obligation: its text is
        // never emitted here.
        if !retained_decls.contains(owner) {
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
            let matching: Vec<&Instance> =
                realization.keys().filter(|i| i.func == rep).collect();
            let [inst] = matching.as_slice() else {
                unresolved_inline_deps.push(UnresolvedInlineDep {
                    owner: owner.0,
                    entry,
                    reason: if matching.is_empty() {
                        "the serving declaration is not retained by the artifact"
                    } else {
                        "the serving declaration has more than one retained instance"
                    },
                });
                continue;
            };
            let inst = (*inst).clone();
            let callee = match boundaries.get(&inst) {
                Some(id) => RealizedCallee::Boundary(id.clone()),
                None => RealizedCallee::Rock(inst),
            };
            inline_deps.push(InlineDep { owner: owner.0, entry, callee });
        }
    }

    ArtifactView {
        shared_instances: g.instances.len(),
        deferred: crate::target_projection::deferred_for(&spec),
        spec,
        roots,
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
    }
}
