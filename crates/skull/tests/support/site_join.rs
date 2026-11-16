//! The MIR-call -> graph site records join, measured.
//!
//! Instance-driven emission answers "what does THIS call instruction call"
//! from the graph's own resolution, keyed by the [`CallSite`] lowering stamps
//! on each call form (`MirFunc::call_sites`). Fn-value symbol mints are callee
//! obligations too, though they are not calls.
//!
//! The census measures the body that comes out of
//! [`skull::mir_emit::prepare_instance_body_with_plans`]: inlining deletes calls
//! and splices others in, and the spliced ones join through the CALLEE
//! instance's graph site records. Only reachable blocks count, since cleanup leaves
//! unreachable ones in the arena and they are never emitted.
//!
//! Every exclusion is counted by cause, and the join fails closed: an owner the
//! graph does not have is refused, and a resolved callee must be the same
//! concrete instance the MIR call targets.

use std::collections::{BTreeMap, HashMap, HashSet};

use bone_semantic::hir::FnBackend;
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::Idx;
use skull_mir::lower::ScalarCtx;
use skull_mir::mir::{FuncRef, Inst, MirFunc};
use skull_mir::site::CallSite;
use ustr::Ustr;

use skull::concrete_body::ConcreteBodies;
use skull::instance_graph::{Instance, InstanceGraph};

/// Why a call instruction is not joined.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Unjoined {
    /// The call carries no site.
    NoSite,
    /// The site's owner index is outside the body's owner table; a splice that
    /// failed to re-intern its owner lands here rather than reading entry 0.
    OwnerNotInTable,
    /// The owner corresponds to no `Instance` in the graph.
    OwnerNotAnInstance,
    /// The owning instance has no concrete lowering plan.
    NoBody,
    /// The body records no callee at this site: lowering synthesized a call the
    /// graph walk does not model.
    SiteNotInBody,
    /// The site resolved to a different declaration than the call targets.
    CalleeMismatch,
    /// Same declaration, but the call's instance claim differs from the
    /// resolved instance.
    CalleeInstanceMismatch,
    /// Generic Rock callee with no recorded concrete instance.
    CalleeNoInstance,
    /// The recorded instance names another declaration: a recorder bug.
    CalleeClaimDeclMismatch,
    /// The resolved instance has no substitution in the graph to compare with.
    CalleeResolvedUnrecorded,
}

impl Unjoined {
    pub fn label(self) -> &'static str {
        match self {
            Self::NoSite => "no-site",
            Self::OwnerNotInTable => "owner-not-in-table",
            Self::OwnerNotAnInstance => "owner-not-an-instance",
            Self::NoBody => "no-body",
            Self::SiteNotInBody => "site-not-in-body",
            Self::CalleeMismatch => "callee-decl-mismatch",
            Self::CalleeInstanceMismatch => "callee-instance-mismatch",
            Self::CalleeNoInstance => "callee-no-instance",
            Self::CalleeClaimDeclMismatch => "callee-claim-decl-mismatch",
            Self::CalleeResolvedUnrecorded => "callee-resolved-unrecorded",
        }
    }
}

/// Why an expanded instance contributed no calls at all.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum BodySkip {
    NoSubstitution,
    LowerFailed,
    PrepareFailed,
    /// Coroutines take a different emission sequence, not measured here.
    Coroutine,
    /// Extern, opaque inline body, or an `Iterator`-bounded fn that exists only
    /// inlined at its call sites.
    NoLoweredBody,
}

#[derive(Clone, Default, Debug)]
pub struct SiteJoinCensus {
    /// Instances whose body was lowered and prepared.
    pub bodies_measured: usize,
    /// Expanded instances that contributed nothing, by cause, so a shrinking
    /// denominator is visible.
    pub bodies_skipped: BTreeMap<BodySkip, usize>,
    /// Reachable callee obligations: call instructions plus fn-value mints.
    pub obligations_examined: usize,
    /// How many of `obligations_examined` are fn-value symbol mints.
    pub symbol_mints: usize,
    pub joined: usize,
    pub joined_rock: usize,
    pub joined_boundary: usize,
    /// Joined calls whose owner is not the measured body: spliced by inlining.
    pub joined_spliced: usize,
    /// Joined with generic arguments compared.
    pub joined_instance_verified: usize,
    /// Joined to a non-generic callee, where the declaration is the instance.
    pub joined_non_generic: usize,
    /// Joined to a boundary, verified by declaration and class.
    pub joined_boundary_verified: usize,
    /// Indirect calls and bound placeholders: admitted, not joined, because
    /// emission renders their operand and spells no symbol.
    pub admitted_indirect: usize,
    pub admitted_indirect_by_op: BTreeMap<&'static str, usize>,
    /// Every indirect call form by producer, including `NamedFunction` rows,
    /// which join like direct calls.
    pub indirect_forms_by_producer: BTreeMap<IndirectProducer, usize>,
    /// Indirect operands spelling a workspace function symbol that neither
    /// `fn_value_symbols` nor `hoisted_fn_globals` recorded: a mint route that
    /// forgot to record would otherwise pass as a runtime value.
    pub indirect_unrecorded_symbol: usize,
    pub examined_by_op: BTreeMap<&'static str, usize>,
    pub joined_by_op: BTreeMap<&'static str, usize>,
    pub unjoined: BTreeMap<Unjoined, usize>,
    pub unjoined_by_op: BTreeMap<&'static str, usize>,
    /// Up to three unjoined examples per cause, for diagnosis.
    pub examples: Vec<String>,
}

impl SiteJoinCensus {
    pub fn unjoined_total(&self) -> usize {
        self.unjoined.values().sum()
    }

    /// Every accounting identity, as named checks. A single
    /// `joined + unjoined == examined` would pass counters that file a call in
    /// two buckets or balance globally while individual operations do not.
    pub fn accounting_failures(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut check = |ok: bool, what: String| {
            if !ok {
                out.push(what);
            }
        };
        let unjoined = self.unjoined_total();
        check(
            self.joined + self.admitted_indirect + unjoined == self.obligations_examined,
            format!(
                "joined {} + admitted-indirect {} + unjoined {unjoined} != obligations examined {}",
                self.joined, self.admitted_indirect, self.obligations_examined
            ),
        );
        let verified =
            self.joined_instance_verified + self.joined_non_generic + self.joined_boundary_verified;
        check(
            verified == self.joined,
            format!("verified {verified} != joined {}", self.joined),
        );
        // The callee arm and the check's verdict must agree on boundaries.
        check(
            self.joined_boundary_verified == self.joined_boundary,
            format!(
                "boundary-verified {} != boundary callees {}",
                self.joined_boundary_verified, self.joined_boundary
            ),
        );
        check(
            self.joined_rock + self.joined_boundary == self.joined,
            format!(
                "rock {} + boundary {} != joined {}",
                self.joined_rock, self.joined_boundary, self.joined
            ),
        );
        // Global sums catch an operation missing from every histogram, which
        // the pointwise check below has no key for.
        let examined_ops: usize = self.examined_by_op.values().sum();
        check(
            examined_ops == self.obligations_examined,
            format!(
                "examined-by-op {examined_ops} != obligations examined {}",
                self.obligations_examined
            ),
        );
        let joined_ops: usize = self.joined_by_op.values().sum();
        check(
            joined_ops == self.joined,
            format!("joined-by-op {joined_ops} != joined {}", self.joined),
        );
        let unjoined_ops: usize = self.unjoined_by_op.values().sum();
        check(
            unjoined_ops == unjoined,
            format!("unjoined-by-op {unjoined_ops} != unjoined {unjoined}"),
        );
        let admitted_producers: usize = self
            .indirect_forms_by_producer
            .iter()
            .filter(|(k, _)| **k != IndirectProducer::NamedFunction)
            .map(|(_, v)| *v)
            .sum();
        check(
            admitted_producers == self.admitted_indirect,
            format!(
                "indirect forms minus named-function {admitted_producers} != admitted-indirect {}",
                self.admitted_indirect
            ),
        );
        check(
            self.indirect_unrecorded_symbol == 0,
            format!(
                "{} indirect operands spell a workspace function symbol neither record names",
                self.indirect_unrecorded_symbol
            ),
        );
        let indirect_ops: usize = self.admitted_indirect_by_op.values().sum();
        check(
            indirect_ops == self.admitted_indirect,
            format!(
                "indirect-by-op {indirect_ops} != indirect {}",
                self.admitted_indirect
            ),
        );
        // Pointwise catches a call filed under the wrong operation, which every
        // global sum misses.
        let ops: std::collections::BTreeSet<&'static str> = [
            &self.examined_by_op,
            &self.joined_by_op,
            &self.unjoined_by_op,
            &self.admitted_indirect_by_op,
        ]
        .iter()
        .flat_map(|m| m.keys().copied())
        .collect();
        let at = |m: &BTreeMap<&'static str, usize>, op| m.get(op).copied().unwrap_or(0);
        for op in ops {
            let (e, j, u, a) = (
                at(&self.examined_by_op, op),
                at(&self.joined_by_op, op),
                at(&self.unjoined_by_op, op),
                at(&self.admitted_indirect_by_op, op),
            );
            check(
                e == j + u + a,
                format!(
                    "operation `{op}`: examined {e} != joined {j} + unjoined {u} + indirect {a}"
                ),
            );
        }
        out
    }

    pub fn accounts(&self) -> bool {
        self.accounting_failures().is_empty()
    }
}

/// What produced the function value an indirect call calls.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum IndirectProducer {
    /// A minted workspace-function symbol: emission spells it, so it joins.
    NamedFunction,
    /// A nested fn this body hoisted; its symbol is this instance's own output.
    HoistedNestedFn,
    /// A runtime value (cell, field, element, global): nothing is spelled.
    RuntimeValue,
    /// `Inst::CallBoundMulti`, a `for` `next` witness the inliner substitutes
    /// per instantiation; it names no callee by construction.
    BoundPlaceholder,
}

fn indirect_producer(mf: &MirFunc, callee: Idx<Inst>) -> (IndirectProducer, Option<FuncRef>) {
    if let Some(fref) = mf.fn_value_symbols.get(&callee) {
        return (IndirectProducer::NamedFunction, Some(*fref));
    }
    match &mf.insts[callee] {
        Inst::Global(n) if mf.hoisted_fn_globals.contains(n) => {
            (IndirectProducer::HoistedNestedFn, None)
        }
        _ => (IndirectProducer::RuntimeValue, None),
    }
}

/// Indirect operands whose emitted NAME is a workspace function symbol that no
/// record names: the second, name-based view of [`indirect_producer`].
fn unrecorded_symbol_operands(mf: &MirFunc, mangled: &HashSet<Ustr>) -> usize {
    let mut n = 0;
    for b in skull_mir::cfg::reachable_blocks_from(mf, mf.entry) {
        for &id in &mf.blocks[b].insts {
            let callee = match &mf.insts[id] {
                Inst::CallValue { callee, .. } | Inst::CallValueMulti { callee, .. } => *callee,
                _ => continue,
            };
            if mf.fn_value_symbols.contains_key(&callee) {
                continue;
            }
            if let Inst::Global(sym) = &mf.insts[callee]
                && !mf.hoisted_fn_globals.contains(sym)
                && mangled.contains(sym)
            {
                n += 1;
            }
        }
    }
    n + mf
        .nested_fns
        .iter()
        .map(|nf| unrecorded_symbol_operands(nf, mangled))
        .sum::<usize>()
}

/// One reachable callee obligation. `target` is `None` for an indirect call
/// through a value and for a bound placeholder.
struct CallRow {
    site: Option<CallSite>,
    target: Option<FuncRef>,
    /// The call's own concrete instance claim (`MirFunc::callee_instances`).
    callee_inst: Option<Instance>,
    producer: Option<IndirectProducer>,
    is_mint: bool,
}

/// Every reachable call and fn-value mint in `mf` and its hoisted nested
/// bodies, which are emitted as its siblings.
fn reachable_call_rows(mf: &MirFunc, out: &mut Vec<CallRow>) {
    for b in skull_mir::cfg::reachable_blocks_from(mf, mf.entry) {
        for &id in &mf.blocks[b].insts {
            // A mint passed or stored rather than called in place would be
            // verified nowhere if it were not its own obligation.
            let (target, producer, is_mint) = if let Some(fref) = mf.fn_value_symbols.get(&id) {
                (Some(*fref), None, true)
            } else {
                match &mf.insts[id] {
                    Inst::Call { func, .. } | Inst::CallMulti { func, .. } => {
                        (Some(*func), None, false)
                    }
                    Inst::CallValue { callee, .. } | Inst::CallValueMulti { callee, .. } => {
                        let (p, fref) = indirect_producer(mf, *callee);
                        (fref, Some(p), false)
                    }
                    Inst::CallBoundMulti { .. } => {
                        (None, Some(IndirectProducer::BoundPlaceholder), false)
                    }
                    _ => continue,
                }
            };
            out.push(CallRow {
                site: mf.call_sites.get(&id).cloned(),
                target,
                callee_inst: mf.callee_instances.get(&id).cloned(),
                producer,
                is_mint,
            });
        }
    }
    for nf in &mf.nested_fns {
        reachable_call_rows(nf, out);
    }
}

fn mangled_symbols(ws: &Workspace) -> HashSet<Ustr> {
    ws.modules
        .iter()
        .flat_map(|m| m.functions.iter().map(|(_, f)| f))
        .map(|f| Ustr::from(&skull_mir::mangle::mangle_fn(ws, f)))
        .collect()
}

/// Graph instances by declaration, from the graph's own substitution record.
type OwnerIndex = HashMap<FuncRef, Vec<Instance>>;

fn owner_index(g: &InstanceGraph) -> OwnerIndex {
    let mut ix = OwnerIndex::new();
    for (inst, _) in g.substitutions() {
        ix.entry(inst.func).or_default().push(inst.clone());
    }
    ix
}

/// The census context shared by every measured body.
struct Join<'a> {
    ws: &'a Workspace,
    backend: FnBackend,
    g: &'a InstanceGraph,
    bodies: &'a ConcreteBodies,
    ix: OwnerIndex,
    mangled: HashSet<Ustr>,
}

impl<'a> Join<'a> {
    fn new(
        ws: &'a Workspace,
        scx: &ScalarCtx,
        g: &'a InstanceGraph,
        bodies: &'a ConcreteBodies,
    ) -> Self {
        Join {
            ws,
            backend: scx.target(),
            g,
            bodies,
            ix: owner_index(g),
            mangled: mangled_symbols(ws),
        }
    }

    fn measure(&self, c: &mut SiteJoinCensus, inst: &Instance, mf: &MirFunc) {
        c.bodies_measured += 1;
        let mut rows = Vec::new();
        reachable_call_rows(mf, &mut rows);
        c.indirect_unrecorded_symbol += unrecorded_symbol_operands(mf, &self.mangled);
        for row in rows {
            c.obligations_examined += 1;
            c.symbol_mints += usize::from(row.is_mint);
            let (cause, op) = self.classify(c, mf, inst, &row);
            *c.examined_by_op.entry(op).or_insert(0) += 1;
            let Some(cause) = cause else { continue };
            *c.unjoined.entry(cause).or_insert(0) += 1;
            *c.unjoined_by_op.entry(op).or_insert(0) += 1;
            let per_cause = c
                .examples
                .iter()
                .filter(|e| e.starts_with(cause.label()))
                .count();
            if per_cause < 3 {
                c.examples.push(self.example(cause, mf, inst, &row));
            }
        }
    }

    fn example(&self, cause: Unjoined, mf: &MirFunc, inst: &Instance, row: &CallRow) -> String {
        let ws = self.ws;
        let target = row
            .target
            .map(|t| ws.module(t.module).functions[t.func].name.to_string())
            .unwrap_or_else(|| "<indirect>".into());
        let at = match &row.site {
            Some(cs) => {
                let owner = mf.site_owners.get(cs.owner.0 as usize).map(|o| {
                    let f = &ws.module(o.func.module).functions[o.func.func];
                    format!("owner `{}` args {}", f.name, o.args.len())
                });
                format!(
                    "{:?} op {} ({})",
                    cs.site.expr,
                    cs.site.op.name(),
                    owner.unwrap_or_else(|| "<owner missing>".into())
                )
            }
            None => "<no site>".to_string(),
        };
        format!(
            "{} in `{}`: {at} -> target `{target}`",
            cause.label(),
            self.g.identity(ws, inst)
        )
    }

    /// Resolve one obligation, counting a join into `c`; returns the unjoined
    /// cause, if any, and the operation label.
    fn classify(
        &self,
        c: &mut SiteJoinCensus,
        mf: &MirFunc,
        measured: &Instance,
        row: &CallRow,
    ) -> (Option<Unjoined>, &'static str) {
        if let Some(producer) = row.producer {
            *c.indirect_forms_by_producer.entry(producer).or_insert(0) += 1;
        }
        // Indirect calls are decided before any lookup: what makes them safe is
        // the operand emission renders, not whether the graph recorded a site.
        let Some(target) = row.target else {
            let op = row.site.as_ref().map_or("<none>", |cs| cs.site.op.name());
            c.admitted_indirect += 1;
            *c.admitted_indirect_by_op.entry(op).or_insert(0) += 1;
            return (None, op);
        };
        let Some(cs) = &row.site else {
            return (Some(Unjoined::NoSite), "<none>");
        };
        let op = cs.site.op.name();
        let Some(owner) = mf.site_owners.get(cs.owner.0 as usize) else {
            return (Some(Unjoined::OwnerNotInTable), op);
        };
        let Some(owner_inst) = self
            .ix
            .get(&owner.func)
            .and_then(|v| v.iter().find(|i| *i == owner))
        else {
            return (Some(Unjoined::OwnerNotAnInstance), op);
        };
        if self.bodies.plan_for(owner_inst).is_none() {
            return (Some(Unjoined::NoBody), op);
        }
        let synthesized = match cs.site.op {
            skull_mir::site::SiteOp::Synthesized(name)
                if skull_mir::lower::SYNTHESIZED_ENTRIES.contains(&name) =>
            {
                skull::instance_graph::stdlib_entry_representative(self.ws, name)
                    .map(Instance::base)
                    .filter(|instance| self.g.instances.contains(instance))
            }
            _ => None,
        };
        let resolved = self
            .g
            .sites_of(owner_inst)
            .and_then(|sites| sites.get(&cs.site))
            .and_then(|record| match &record.outcome {
                skull::instance_graph::SiteOutcome::Resolved(inst) => Some(inst),
                skull::instance_graph::SiteOutcome::Failed(_) => None,
            });
        let Some(callee) = synthesized.as_ref().or(resolved) else {
            return (Some(Unjoined::SiteNotInBody), op);
        };
        let verdict = check_callee(
            self.ws,
            self.g,
            callee,
            target,
            row.callee_inst.as_ref(),
            self.backend,
        );
        let kind = match verdict.outcome() {
            Ok(kind) => kind,
            Err(cause) => return (Some(cause), op),
        };
        c.joined += 1;
        *c.joined_by_op.entry(op).or_insert(0) += 1;
        match kind {
            Verified::Instance => c.joined_instance_verified += 1,
            Verified::NonGeneric => c.joined_non_generic += 1,
            Verified::Boundary => c.joined_boundary_verified += 1,
        }
        if matches!(kind, Verified::Boundary) {
            c.joined_boundary += 1;
        } else {
            c.joined_rock += 1;
        }
        if owner_inst != measured {
            c.joined_spliced += 1;
        }
        (None, op)
    }
}

/// Whether skull lowers a standalone body for this declaration, by the
/// definition loop's own admission rule.
fn has_lowered_body(ws: &Workspace, fref: FuncRef) -> bool {
    let f = &ws.module(fref.module).functions[fref.func];
    skull::mir_emit::is_cached_body(f)
        && ws
            .module(fref.module)
            .bodies
            .contains_key(&bone_semantic::hir::BodyOwner::Function(fref.func))
}

/// Measure one already-prepared body, so a test can perturb its recorded facts
/// and watch them travel through the same classifier the census runs.
pub fn measure_prepared_body(
    ws: &Workspace,
    scx: &ScalarCtx,
    g: &InstanceGraph,
    bodies: &ConcreteBodies,
    inst: &Instance,
    mf: &MirFunc,
) -> SiteJoinCensus {
    let mut c = SiteJoinCensus::default();
    Join::new(ws, scx, g, bodies).measure(&mut c, inst, mf);
    c
}

/// Measure the join over `g`'s expanded instances, at the phase emission
/// consumes.
pub fn census(
    ws: &Workspace,
    scx: &ScalarCtx,
    g: &InstanceGraph,
    bodies: &ConcreteBodies,
) -> SiteJoinCensus {
    let mut c = SiteJoinCensus::default();
    let join = Join::new(ws, scx, g, bodies);
    let mut cache = skull_transform::InlineCache::default();
    for inst in g.expanded() {
        let f = &ws.module(inst.func.module).functions[inst.func.func];
        let skip = if f.is_coroutine {
            Some(BodySkip::Coroutine)
        } else if g.substitution(inst).is_none() {
            Some(BodySkip::NoSubstitution)
        } else if !has_lowered_body(ws, inst.func) {
            Some(BodySkip::NoLoweredBody)
        } else {
            None
        };
        let lowered = match skip {
            Some(why) => Err(why),
            None => bodies
                .plan_for(inst)
                .and_then(|plan| {
                    skull_mir::lower::lower_instance_with_plans(ws, scx, plan, &bodies.plans).ok()
                })
                .ok_or(BodySkip::LowerFailed)
                .and_then(|mut mf| {
                    let name = skull_mir::mangle::mangle_fn(ws, f);
                    skull::mir_emit::prepare_instance_body_with_plans(
                        ws,
                        scx,
                        inst.func,
                        &name,
                        &mut mf,
                        &mut cache,
                        &bodies.plans,
                    )
                    .map(|_| mf)
                    .map_err(|_| BodySkip::PrepareFailed)
                }),
        };
        match lowered {
            Ok(mf) => join.measure(&mut c, inst, &mf),
            Err(why) => *c.bodies_skipped.entry(why).or_insert(0) += 1,
        }
    }
    c
}

/// The result of comparing a resolved callee against the call that targets it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CalleeCheck {
    /// Declaration and concrete instantiation agree.
    Instance,
    /// A boundary: identified by declaration and class, not a Rock
    /// instantiation, even when generic.
    Boundary(skull::instance_graph::BoundaryClass),
    /// Non-generic callee: the declaration is the instance.
    NonGeneric,
    DeclMismatch,
    InstanceMismatch,
    NoInstance,
    ClaimDeclMismatch,
    ResolvedUnrecorded,
}

impl CalleeCheck {
    pub fn outcome(self) -> Result<Verified, Unjoined> {
        match self {
            Self::Instance => Ok(Verified::Instance),
            Self::NonGeneric => Ok(Verified::NonGeneric),
            Self::Boundary(_) => Ok(Verified::Boundary),
            Self::DeclMismatch => Err(Unjoined::CalleeMismatch),
            Self::InstanceMismatch => Err(Unjoined::CalleeInstanceMismatch),
            Self::NoInstance => Err(Unjoined::CalleeNoInstance),
            Self::ClaimDeclMismatch => Err(Unjoined::CalleeClaimDeclMismatch),
            Self::ResolvedUnrecorded => Err(Unjoined::CalleeResolvedUnrecorded),
        }
    }
}

/// A verdict that counts as a join.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verified {
    Instance,
    NonGeneric,
    Boundary,
}

/// The callee comparison. Declaration equality is too weak: two instantiations
/// of one generic compare equal under it, so generic arguments participate and
/// a missing instance claim is reported rather than treated as agreement.
pub fn check_callee(
    ws: &Workspace,
    g: &InstanceGraph,
    callee_inst: &Instance,
    target: FuncRef,
    call_inst: Option<&Instance>,
    target_backend: FnBackend,
) -> CalleeCheck {
    // The graph records a backend variant group's representative while MIR
    // calls the sibling serving this backend, so ask the emitter's own
    // selection rule rather than compare `FuncRef`s.
    if callee_inst.func != target {
        let name = ws.module(callee_inst.func.module).functions[callee_inst.func.func].name;
        let selected =
            skull_mir::lower::backend_variant(ws, name, callee_inst.func, target_backend);
        if !matches!(selected, Ok(f) if f == target) {
            return CalleeCheck::DeclMismatch;
        }
    }
    // An opaque body is emitted once per declaration and target, so its
    // generic arguments are not part of what emission spells.
    if let Some(class) = skull::instance_graph::boundary_class_for(
        &ws.module(callee_inst.func.module).functions[callee_inst.func.func],
    ) {
        return CalleeCheck::Boundary(class);
    }
    if ws.module(target.module).functions[target.func]
        .generics
        .is_empty()
    {
        return CalleeCheck::NonGeneric;
    }
    let Some(claim) = call_inst else {
        return CalleeCheck::NoInstance;
    };
    if claim.func != target {
        return CalleeCheck::ClaimDeclMismatch;
    }
    match g.substitution(callee_inst) {
        Some(_) if callee_inst.args == claim.args => CalleeCheck::Instance,
        Some(_) => CalleeCheck::InstanceMismatch,
        None => CalleeCheck::ResolvedUnrecorded,
    }
}
