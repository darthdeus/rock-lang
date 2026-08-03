//! A target's ARTIFACT VIEW of the one shared instance graph
//! (`docs/monomorphization-plan.md` S 9, S 11).
//!
//! The shared [`InstanceGraph`] is target-NEUTRAL: it holds every logical
//! instance any target could reach, including declarations only another target
//! can emit. An artifact view is one target's REALIZATION of it -- which
//! declarations it retains, how each foreign body is actually supplied, which
//! opaque dependency resolves to which declaration, and what it spells each
//! retained instance.
//!
//! **It may DROP and it may REJECT; it may never DISCOVER.** [`realize`] takes
//! `&InstanceGraph`, so it cannot rebuild or extend what it measures -- plan
//! S 20g.1 asks whether logical identity survives realization differing, and a
//! view that invented an instance would be answering its own question.
//!
//! Four decisions are ONE decision, which is why they land together:
//!
//! 1. **Retention reads the DECLARATION, never a symbol.**
//!    [`InstanceGraph::boundaries`] is keyed by mangled symbol, so two
//!    `@nomangle` backend siblings (`__nb_checked_index` in `unified.rock` and
//!    in `js.rock`) collapse into one entry and one silently answers for the
//!    other. Both are `@nomangle`, so both are emission roots, so both are
//!    instances of the Lua graph. The artifact asks
//!    [`instance_graph::serves`] of the declaration each instance names.
//! 2. **Intrinsic availability comes from the artifact's OWN native mapping**
//!    ([`ArtifactMappings`]), never from a compatible `FnBackend` and never
//!    from the presence of a Rock body. `@native_required` skips the portable
//!    rung by definition (`skull_mir::intrinsics`), so "it has a fallback" is
//!    the answer that marker exists to refuse.
//! 3. **Opaque dependencies resolve HERE**, because the declaration serving a
//!    runtime-entry name differs by target. The graph records names; the
//!    artifact turns each into a declaration it retains, or a rejection.
//! 4. **One symbol per `(artifact, instance)`, injective over what the artifact
//!    retains.** `InstanceGraph::logical_symbol` tests the ENCODER, which is
//!    injective over its own input; it cannot answer for an artifact holding
//!    two declarations that encode alike. Retention is what makes the artifact
//!    table injective, which is why (1) and (4) are not separable.

use std::collections::{BTreeMap, BTreeSet};

use bone_semantic::hir::{FnBackend, Function};
use bone_semantic::inference::Workspace;
use skull_mir::intrinsics::{self, IntrinsicTable};
use skull_mir::mir::FuncRef;

use crate::instance_graph::{
    self, BoundaryClass, DeclKey, Instance, InstanceGraph,
};
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

/// A declaration the shared graph holds and this artifact cannot realize.
#[derive(Clone, Debug)]
pub struct Excluded {
    pub instance: Instance,
    pub cause: Unrealizable,
}

/// One opaque-body dependency, resolved to a declaration in THIS artifact.
#[derive(Clone, Debug)]
pub struct InlineDep {
    /// The opaque body that names it.
    pub owner: FuncRef,
    pub entry: &'static str,
    /// The declaration serving `entry` on this artifact.
    pub target: FuncRef,
    /// The artifact's spelling of `target` -- what emission must produce.
    pub symbol: String,
}

/// An opaque-body dependency this artifact cannot resolve. A located rejection:
/// the owner is retained and its text calls a name nothing here supplies, so
/// emission would produce a call to a symbol that does not exist.
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
    /// Instances retained, and the shared-graph population they came from.
    pub instances: usize,
    pub shared_instances: usize,
    pub excluded: usize,
    pub excluded_by_cause: BTreeMap<&'static str, usize>,
    pub realized_by_kind: BTreeMap<&'static str, usize>,
    pub symbols: usize,
    pub symbol_collisions: usize,
    /// Dependencies examined, and how many named no declaration here.
    pub inline_deps_examined: usize,
    pub inline_deps_unresolved: usize,
    /// Resolved dependencies whose target the artifact does NOT retain. Must be
    /// 0: emission would name a symbol this artifact never emits.
    pub inline_deps_outside: usize,
    /// Deferred obligations, carried from the target spec.
    pub deferred: Vec<&'static str>,
}

/// One target's realization of a graph it did not build.
pub struct ArtifactView {
    pub spec: ExecutionTargetSpec,
    /// Retained instances. A SUBSET of `g.instances`, asserted rather than
    /// assumed -- see `realization_never_invents_an_instance`.
    pub instances: BTreeSet<Instance>,
    /// How each retained instance is supplied here.
    pub realization: BTreeMap<Instance, Realization>,
    pub excluded: Vec<Excluded>,
    /// One symbol per retained instance. Total and injective.
    pub symbols: BTreeMap<Instance, String>,
    pub symbol_collisions: Vec<SymbolCollision>,
    pub inline_deps: Vec<InlineDep>,
    pub unresolved_inline_deps: Vec<UnresolvedInlineDep>,
    /// The shared population this view was taken over -- the denominator.
    pub shared_instances: usize,
    pub deferred: Vec<&'static str>,
}

impl ArtifactView {
    pub fn census(&self) -> ArtifactCensus {
        let mut c = ArtifactCensus {
            instances: self.instances.len(),
            shared_instances: self.shared_instances,
            excluded: self.excluded.len(),
            symbols: self.symbols.len(),
            symbol_collisions: self.symbol_collisions.len(),
            inline_deps_examined: self.inline_deps.len() + self.unresolved_inline_deps.len(),
            inline_deps_unresolved: self.unresolved_inline_deps.len(),
            inline_deps_outside: self
                .inline_deps
                .iter()
                .filter(|d| !self.instances.iter().any(|i| i.func == d.target))
                .count(),
            deferred: self.deferred.clone(),
            ..Default::default()
        };
        for e in &self.excluded {
            *c.excluded_by_cause.entry(e.cause.label()).or_insert(0) += 1;
        }
        for r in self.realization.values() {
            *c.realized_by_kind.entry(r.label()).or_insert(0) += 1;
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

    pub fn render_excluded(&self, ws: &Workspace, g: &InstanceGraph) -> Vec<String> {
        self.excluded
            .iter()
            .map(|e| {
                format!("{} -- {}", g.identity(ws, &e.instance), e.cause.label())
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
    decl: FuncRef,
    f: &Function,
    erased: &BTreeSet<String>,
    spec: &ExecutionTargetSpec,
    mappings: &ArtifactMappings<'_>,
) -> Result<Realization, Unrealizable> {
    // The declaration's own backend first. A `@nomangle` sibling that serves
    // another backend is not this artifact's, whatever its class.
    let serves_artifact = match spec.backend {
        Some(b) => instance_graph::serves(f.backend, b),
        // A target with no Rock-level backend of its own names no host, so only
        // a declaration that names none -- `FnBackend::Any`, a portable Rock
        // body -- serves it. Deliberately NOT `serves(f.backend, Any)`, which
        // answers yes to everything: that is the WALK's "any target"
        // convenience and would keep `extern(lua) fn print` on a C artifact.
        None => f.backend == FnBackend::Any,
    };
    if !serves_artifact {
        return Err(Unrealizable::ForeignToArtifact);
    }

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

/// Realize `g` for `spec`. Read-only in the type system, not just by
/// convention: `g` is `&`, so no discovery can happen here.
pub fn realize(
    ws: &Workspace,
    g: &InstanceGraph,
    spec: ExecutionTargetSpec,
    mappings: ArtifactMappings<'_>,
) -> ArtifactView {
    let mut instances = BTreeSet::new();
    let mut realization = BTreeMap::new();
    let mut excluded = Vec::new();
    for i in &g.instances {
        let f = &ws.module(i.func.module).functions[i.func.func];
        match realize_decl(ws, i.func, f, &g.erased_entries, &spec, &mappings) {
            Ok(r) => {
                instances.insert(i.clone());
                realization.insert(i.clone(), r);
            }
            Err(cause) => excluded.push(Excluded { instance: i.clone(), cause }),
        }
    }

    // One symbol per retained instance, from the logical encoder. The artifact
    // OWNS the spelling (plan S 0h), and today's spelling is the encoder's;
    // what is new is that injectivity is asserted over the ARTIFACT's set,
    // which the encoder cannot answer for on its own.
    let mut by_symbol: BTreeMap<String, Vec<Instance>> = BTreeMap::new();
    let mut symbols = BTreeMap::new();
    for i in &instances {
        let s = g.logical_symbol(ws, i);
        by_symbol.entry(s.clone()).or_default().push(i.clone());
        symbols.insert(i.clone(), s);
    }
    let symbol_collisions: Vec<SymbolCollision> = by_symbol
        .into_iter()
        .filter(|(_, v)| v.len() > 1)
        .map(|(symbol, instances)| SymbolCollision { symbol, instances })
        .collect();

    // Opaque dependencies, resolved to declarations THIS artifact retains.
    let mut inline_deps = Vec::new();
    let mut unresolved_inline_deps = Vec::new();
    let retained_decls: BTreeSet<DeclKey> =
        instances.iter().map(|i| DeclKey(i.func)).collect();
    for (owner, entries) in &g.inline_body_deps {
        // An owner this artifact dropped carries no obligation: its text is
        // never emitted here.
        if !retained_decls.contains(owner) {
            continue;
        }
        for entry in entries {
            let backend = spec.backend.unwrap_or(FnBackend::Any);
            let Some(target) = instance_graph::stdlib_entry(ws, entry, backend) else {
                unresolved_inline_deps.push(UnresolvedInlineDep {
                    owner: owner.0,
                    entry,
                    reason: "no declaration serves this entry on the artifact",
                });
                continue;
            };
            let Some(symbol) = instances
                .iter()
                .find(|i| i.func == target)
                .map(|i| symbols[i].clone())
            else {
                unresolved_inline_deps.push(UnresolvedInlineDep {
                    owner: owner.0,
                    entry,
                    reason: "the serving declaration is not retained by the artifact",
                });
                continue;
            };
            inline_deps.push(InlineDep { owner: owner.0, entry, target, symbol });
        }
    }

    ArtifactView {
        shared_instances: g.instances.len(),
        deferred: crate::target_projection::deferred_for(&spec),
        spec,
        instances,
        realization,
        excluded,
        symbols,
        symbol_collisions,
        inline_deps,
        unresolved_inline_deps,
    }
}
