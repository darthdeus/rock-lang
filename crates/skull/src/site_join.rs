//! **The MIR-call -> `ConcreteBody` join, measured** (plan S 9, Stage 3).
//!
//! Instance-driven emission has to answer "what does THIS call instruction
//! call" from the graph's own resolution rather than by resolving again. That
//! needs a join key, and `MirFunc::call_sites` is it: lowering stamps a call
//! form with the [`skull_mir::site::CallSite`] it came from, and the walk
//! recorded what each site resolved to. Not EVERY form -- `CallBoundMulti` is
//! not stamped at all, and is counted here under `unstamped-form` rather than
//! left outside the population.
//!
//! **The phase is POST-TRANSFORM, and that is load-bearing.** Emission renders
//! the body that comes out of [`crate::mir_emit::prepare_instance_body`] --
//! inlined, cleaned, closure-converted, FORL-recovered -- so that is the only
//! body whose calls are the ones actually emitted. Measuring the raw lowering
//! output would count calls inlining deletes and miss calls it splices in, and
//! the spliced ones are exactly the hard case: they belong to the CALLEE
//! instance and join through the callee's `ConcreteBody`.
//!
//! **Reachability is part of the population.** Cleanup leaves unreachable
//! blocks in the arena, and their instructions are never emitted. Counting them
//! would inflate the denominator with calls no backend renders.
//!
//! Every exclusion is reported BY CAUSE with a real count, never as a silent
//! `filter_map`: an exclusion with an unstated denominator is indistinguishable
//! from a join that quietly does not work.
//!
//! **The join FAILS CLOSED, and verifies the CALLEE.** [`resolve_owner`]
//! demands an exact `(declaration, substitution)` match against the graph's own
//! record, so an owner the graph does not have is reported rather than matched
//! to another instance of the same declaration. And a site resolving is not
//! enough: the resolved callee must equal the function the MIR call actually
//! targets ([`Unjoined::CalleeMismatch`]), because a right site pointing at a
//! wrong callee is the one failure instance-driven emission cannot tolerate.
//!
//! It is NOT yet an oracle. Current populations, the mutation results and the
//! structural blocker are recorded in `docs/parked/mono.md` -- a live number in
//! a source comment goes stale silently.

use std::collections::{BTreeMap, HashMap};

use bone_semantic::inference::Workspace;
use skull_mir::lower::ScalarCtx;
use skull_mir::mir::{Inst, MirFunc};
use skull_mir::monoty::MonoSubst;
use skull_mir::site::{CallSite, OwnerInstance, Site};

use crate::concrete_body::{Callee, ConcreteBodies};
use crate::instance_graph::{Instance, InstanceGraph};

/// Why a call instruction is not joined. Every arm is a real population with a
/// count, so "0 unjoined" is a claim about a stated space rather than about
/// whatever happened to survive a filter.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Unjoined {
    /// The call carries no site: it was emitted outside any expression.
    NoSite,
    /// The call's site names an owner index the body's owner table does not
    /// have. A splice that failed to re-intern its owner lands here rather than
    /// silently reading the caller's entry 0.
    OwnerNotInTable,
    /// The owner instance corresponds to no `Instance` in the graph -- the
    /// substitution a body was lowered under is not one the walk recorded.
    OwnerNotAnInstance,
    /// The owning instance has no `ConcreteBody` (the walk did not expand it).
    NoBody,
    /// The body has no callee recorded at this site. This is the arm that goes
    /// non-zero when lowering synthesizes a call the graph walk does not model.
    SiteNotInBody,
    /// The site resolved, but to a DIFFERENT function than the call actually
    /// targets. Without this arm a wrong callee carrying a right site counts as
    /// a join, which is the one failure mode instance-driven emission cannot
    /// tolerate: the site is only useful if it selects the callee the emitted
    /// code calls.
    CalleeMismatch,
    /// The declaration agrees but the INSTANCE does not: the call's own
    /// instantiation key reconstructs a substitution the resolved instance does
    /// not have. Two instantiations of one generic declaration are exactly the
    /// distinction Stage 3 needs, and a declaration-level check cannot see it.
    CalleeInstanceMismatch,
    /// Generic callee, and the call carries NO instantiation key at all. The
    /// lowerer records one only when it specializes, so this is the population
    /// conditional specialization is responsible for.
    CalleeNoKey,
    /// Generic callee whose key has the wrong number of slots for the
    /// declaration's generics. Key arity is POSITIONAL, so a wrong-length key
    /// names another function's instantiation -- kept apart from a missing key
    /// because it is an invariant break, not a consequence of policy.
    CalleeKeyArity,
    /// Generic callee whose key is the right length but holds a `None` slot, so
    /// it names no instance.
    CalleePartialKey,
    /// The resolved instance itself has no substitution recorded in the graph,
    /// so there is nothing to compare the reconstructed one against. An
    /// invariant finding about the GRAPH, not about the call.
    CalleeResolvedUnrecorded,
    /// A call form `Builder::emit_cur` does not stamp (`CallBoundMulti`), so it
    /// can carry no site at all.
    UnstampedForm,
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
            Self::CalleeNoKey => "callee-no-key",
            Self::CalleeKeyArity => "callee-key-arity",
            Self::CalleePartialKey => "callee-partial-key",
            Self::CalleeResolvedUnrecorded => "callee-resolved-unrecorded",
            Self::UnstampedForm => "unstamped-form",
        }
    }
}

/// Why an expanded instance contributed no calls at all.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum BodySkip {
    /// No substitution recorded, so there is nothing to lower under.
    NoSubstitution,
    /// Lowering rejected the body.
    LowerFailed,
    /// The transform sequence rejected the body.
    PrepareFailed,
    /// A coroutine. Its emission path is a different sequence
    /// (`emit_coroutine`, no inlining) and this chunk does not claim it.
    Coroutine,
    /// A declaration skull does not lower a body for at all (extern, an opaque
    /// `inline(lua)`/`inline(js)` body, or an `Iterator`-bounded fn that exists
    /// only inlined at its call sites).
    NoLoweredBody,
}

impl BodySkip {
    pub fn label(self) -> &'static str {
        match self {
            Self::NoSubstitution => "no-substitution",
            Self::LowerFailed => "lower-failed",
            Self::PrepareFailed => "prepare-failed",
            Self::Coroutine => "coroutine",
            Self::NoLoweredBody => "no-lowered-body",
        }
    }
}

#[derive(Clone, Default, Debug)]
pub struct SiteJoinCensus {
    /// Instances whose body was lowered AND prepared -- the space the call
    /// counts below are over.
    pub bodies_measured: usize,
    /// Expanded instances that contributed no calls, by cause. Reported rather
    /// than skipped: a shrinking denominator flatters every ratio.
    pub bodies_skipped: BTreeMap<BodySkip, usize>,
    /// REACHABLE call instructions examined. The denominator.
    pub call_insts: usize,
    /// Call instructions that joined to exactly one callee.
    pub joined: usize,
    /// Joined calls by what they resolved to, so the join is not satisfied
    /// entirely by one arm.
    pub joined_rock: usize,
    pub joined_boundary: usize,
    /// Joined calls whose owner is NOT the body being measured -- i.e. spliced
    /// in by inlining and joined through the callee instance's own body. A zero
    /// here over a corpus that inlines means the hard case is untested.
    pub joined_spliced: usize,
    /// Joined calls verified to the full concrete INSTANCE: the declaration
    /// agreed AND the call's own instantiation key reconstructs the resolved
    /// instance's substitution. This is the only bucket that is evidence for
    /// instance-driven emission.
    pub joined_instance_verified: usize,
    /// Joined calls where the callee declaration is NON-GENERIC, so it has one
    /// instance and the declaration match IS the instance match. Verified, but
    /// counted apart because it says nothing about generic arguments.
    pub joined_non_generic: usize,
    /// INDIRECT calls: admitted, but NOT joined. `Inst::CallValue` names a
    /// function value, so there is no declaration to compare the resolved
    /// instance against. Their own population, so "every join verifies the
    /// concrete instance" stays true of `joined`.
    pub admitted_indirect: usize,
    pub admitted_indirect_by_op: BTreeMap<&'static str, usize>,
    /// EVERY indirect call FORM by what produced its callee operand -- including
    /// the `named-function` rows, which are not admitted but joined like direct
    /// calls. So this is not the same population as `admitted_indirect`, and the
    /// difference is an asserted identity rather than a discrepancy.
    pub indirect_forms_by_producer: BTreeMap<IndirectProducer, usize>,
    /// Indirect calls whose operand is an `Inst::Global` naming a symbol the
    /// workspace mints for a function, yet which neither
    /// `MirFunc::fn_value_symbols` nor `MirFunc::hoisted_fn_globals` recorded.
    ///
    /// The classifier reads the RECORDS; this reads the emitted NAME. Two
    /// independent views of one fact, so a mint route that forgets to record
    /// shows up here instead of silently becoming a `runtime-value` -- which is
    /// precisely the fail-open the permanent class would otherwise hide.
    pub indirect_unrecorded_symbol: usize,
    /// Reachable calls examined, by site operation -- the denominator each
    /// per-operation claim is against.
    pub examined_by_op: BTreeMap<&'static str, usize>,
    /// Joined calls by site operation.
    pub joined_by_op: BTreeMap<&'static str, usize>,
    /// Unjoined calls, by cause.
    pub unjoined: BTreeMap<Unjoined, usize>,
    /// Unjoined calls broken down by the OPERATION they carry, so an unjoined
    /// population cannot hide behind one summary number.
    pub unjoined_by_op: BTreeMap<&'static str, usize>,
    /// A few unjoined examples, for diagnosis. Never the whole finding.
    pub examples: Vec<String>,
    /// EVERY non-joined call, one tab-separated row, when the caller asked for
    /// detail. The capped `examples` above cannot attribute a population: three
    /// rows of 1,102 say nothing about the other 1,099, and a hand count of the
    /// emitted rows is not reproducible. Empty unless [`census_detailed`] ran.
    pub rows: Vec<String>,
}

impl SiteJoinCensus {
    pub fn unjoined_total(&self) -> usize {
        self.unjoined.values().sum()
    }

    /// Every accounting identity, as named checks. A single
    /// `joined + unjoined == examined` is satisfied by counters that are
    /// internally inconsistent -- a call counted in two verification buckets,
    /// or an operation histogram that balances globally while individual
    /// operations do not.
    pub fn accounting_failures(&self) -> Vec<String> {
        let mut out = Vec::new();
        let total = self.joined + self.admitted_indirect + self.unjoined_total();
        if total != self.call_insts {
            out.push(format!(
                "joined {} + admitted-indirect {} + unjoined {} != examined {}",
                self.joined,
                self.admitted_indirect,
                self.unjoined_total(),
                self.call_insts
            ));
        }
        let verified = self.joined_instance_verified + self.joined_non_generic;
        if verified != self.joined {
            out.push(format!(
                "instance-verified {} + non-generic {} != joined {}",
                self.joined_instance_verified, self.joined_non_generic, self.joined
            ));
        }
        if self.joined_rock + self.joined_boundary != self.joined {
            out.push(format!(
                "rock {} + boundary {} != joined {}",
                self.joined_rock, self.joined_boundary, self.joined
            ));
        }
        // GLOBAL sums, kept ALONGSIDE the pointwise check below rather than
        // replaced by it. Pointwise iterates the union of keys the histograms
        // mention, so an operation that vanishes from ALL of them contributes
        // no key, and its absence is invisible to a pointwise check while the
        // main counters stay non-zero.
        let examined_ops: usize = self.examined_by_op.values().sum();
        if examined_ops != self.call_insts {
            out.push(format!("examined-by-op {examined_ops} != examined {}", self.call_insts));
        }
        let joined_ops: usize = self.joined_by_op.values().sum();
        if joined_ops != self.joined {
            out.push(format!("joined-by-op {joined_ops} != joined {}", self.joined));
        }
        let unjoined_ops: usize = self.unjoined_by_op.values().sum();
        if unjoined_ops != self.unjoined_total() {
            out.push(format!(
                "unjoined-by-op {unjoined_ops} != unjoined {}",
                self.unjoined_total()
            ));
        }
        // The producer census covers every indirect FORM, admitted or not, so it
        // is deliberately a different population from `admitted_indirect` and
        // the relationship between them is asserted rather than left to the
        // reader. `named-function` rows carry a target and joined.
        let admitted_producers: usize = self
            .indirect_forms_by_producer
            .iter()
            .filter(|(k, _)| **k != IndirectProducer::NamedFunction)
            .map(|(_, v)| *v)
            .sum();
        if admitted_producers != self.admitted_indirect {
            out.push(format!(
                "indirect forms minus named-function {admitted_producers} != admitted-indirect {}",
                self.admitted_indirect
            ));
        }
        if self.indirect_unrecorded_symbol != 0 {
            out.push(format!(
                "{} indirect operands spell a workspace function symbol neither record names",
                self.indirect_unrecorded_symbol
            ));
        }
        let indirect_ops: usize = self.admitted_indirect_by_op.values().sum();
        if indirect_ops != self.admitted_indirect {
            out.push(format!(
                "indirect-by-op {indirect_ops} != indirect {}",
                self.admitted_indirect
            ));
        }

        // POINTWISE, over the union of every operation key any histogram
        // mentions. The global sums above cannot see a call filed under the
        // WRONG operation -- one bucket over, another under -- and this cannot
        // see an operation missing from every histogram. Both are needed.
        let mut ops: std::collections::BTreeSet<&'static str> = std::collections::BTreeSet::new();
        ops.extend(self.examined_by_op.keys().copied());
        ops.extend(self.joined_by_op.keys().copied());
        ops.extend(self.unjoined_by_op.keys().copied());
        ops.extend(self.admitted_indirect_by_op.keys().copied());
        for op in ops {
            let e = self.examined_by_op.get(op).copied().unwrap_or(0);
            let j = self.joined_by_op.get(op).copied().unwrap_or(0);
            let u = self.unjoined_by_op.get(op).copied().unwrap_or(0);
            let a = self.admitted_indirect_by_op.get(op).copied().unwrap_or(0);
            if e != j + u + a {
                out.push(format!(
                    "operation `{op}`: examined {e} != joined {j} + unjoined {u} + indirect {a}"
                ));
            }
        }
        out
    }

    pub fn accounts(&self) -> bool {
        self.accounting_failures().is_empty()
    }

    pub fn merge(&mut self, other: &SiteJoinCensus) {
        self.bodies_measured += other.bodies_measured;
        self.call_insts += other.call_insts;
        self.joined += other.joined;
        self.joined_rock += other.joined_rock;
        self.joined_boundary += other.joined_boundary;
        self.joined_spliced += other.joined_spliced;
        self.joined_instance_verified += other.joined_instance_verified;
        self.joined_non_generic += other.joined_non_generic;
        self.admitted_indirect += other.admitted_indirect;
        for (k, v) in &other.admitted_indirect_by_op {
            *self.admitted_indirect_by_op.entry(k).or_insert(0) += v;
        }
        for (k, v) in &other.indirect_forms_by_producer {
            *self.indirect_forms_by_producer.entry(*k).or_insert(0) += v;
        }
        self.indirect_unrecorded_symbol += other.indirect_unrecorded_symbol;
        for (k, v) in &other.examined_by_op {
            *self.examined_by_op.entry(k).or_insert(0) += v;
        }
        for (k, v) in &other.joined_by_op {
            *self.joined_by_op.entry(k).or_insert(0) += v;
        }
        for (k, v) in &other.bodies_skipped {
            *self.bodies_skipped.entry(*k).or_insert(0) += v;
        }
        for (k, v) in &other.unjoined {
            *self.unjoined.entry(*k).or_insert(0) += v;
        }
        for (k, v) in &other.unjoined_by_op {
            *self.unjoined_by_op.entry(k).or_insert(0) += v;
        }
        for e in &other.examples {
            if self.examples.len() < 12 {
                self.examples.push(e.clone());
            }
        }
        self.rows.extend(other.rows.iter().cloned());
    }
}

/// What produced the function VALUE an indirect call calls.
///
/// The distinction that matters is whether emission SPELLS anything. A direct
/// call renders a symbol, so naming the wrong instance is possible and must be
/// refused. An indirect call renders its callee operand -- `__rock_call(f, ..)`
/// in JS, `f(..)` in Lua -- so the only way it can name a function at all is if
/// the operand itself is a spelled one.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum IndirectProducer {
    /// The operand is a minted workspace-function symbol
    /// (`MirFunc::fn_value_symbols`). Emission spells that function's name, so
    /// this call CAN name a wrong target and is NOT admitted here -- it joins.
    NamedFunction,
    /// The operand names a nested fn THIS body hoisted to top level
    /// (`MirFunc::hoisted_fn_globals`). The symbol is the owning instance's own
    /// output, minted by the same emission, so there is no callee declaration
    /// for it to disagree with.
    HoistedNestedFn,
    /// The operand is a runtime value -- a cell load, a field, an element, a
    /// fn-typed module global. It carries no declaration at all, so emission
    /// renders the value and spells nothing. This is the permanent non-direct
    /// class.
    RuntimeValue,
}

impl IndirectProducer {
    pub fn label(self) -> &'static str {
        match self {
            Self::NamedFunction => "named-function",
            Self::HoistedNestedFn => "hoisted-nested-fn",
            Self::RuntimeValue => "runtime-value",
        }
    }
}

/// Classify what an indirect call's callee operand was produced by.
///
/// Reads the MIR operand rather than the site record: the question is what
/// EMISSION will render, and emission renders the operand.
fn indirect_producer(
    mf: &MirFunc,
    callee: bone_semantic::prelude::Idx<Inst>,
) -> (IndirectProducer, Option<skull_mir::mir::FuncRef>) {
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

/// Indirect-call operands that SPELL a workspace function symbol without either
/// record naming them.
///
/// The classifier above reads the records; this reads the emitted name against
/// the set of symbols the workspace mints. A mint route that forgets to record
/// is invisible to the first view -- it just becomes another `runtime-value` --
/// and this is the second view that makes it visible. It is a CHECK, never the
/// classifier: a name is what emission writes, not what identity is.
fn unrecorded_symbol_operands(
    mf: &MirFunc,
    mangled: &std::collections::HashSet<ustr::Ustr>,
) -> usize {
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
            let Inst::Global(sym) = &mf.insts[callee] else { continue };
            if mf.hoisted_fn_globals.contains(sym) {
                continue;
            }
            if mangled.contains(sym) {
                n += 1;
            }
        }
    }
    for nf in &mf.nested_fns {
        n += unrecorded_symbol_operands(nf, mangled);
    }
    n
}

/// What one reachable call instruction contributes to the census: the site
/// recorded for it (if any) and the FuncRef it actually targets.
///
/// `target` is `None` for an indirect call (`CallValue`), which names a value
/// rather than a declaration, and for `CallBoundMulti`, whose target is a
/// placeholder the inliner resolves.
struct CallRow {
    /// The instruction itself, so a row can carry the span lowering stamped on
    /// it. Attribution by declaration name alone cannot separate two calls to
    /// one function from two different source lines.
    inst: bone_semantic::prelude::Idx<Inst>,
    site: Option<CallSite>,
    target: Option<skull_mir::mir::FuncRef>,
    /// The call's OWN instantiation key (`MirFunc::spec_bindings`), which is
    /// what makes the comparison instance-level rather than declaration-level.
    /// `None` when the lowerer recorded none.
    spec_key: Option<skull_mir::lower::SpecKey>,
    /// For an indirect call, what produced the callee operand. `None` for a
    /// direct call.
    producer: Option<IndirectProducer>,
    /// A call form `Builder::emit_cur` does not stamp at all. Counted here so
    /// it is a NAMED exclusion rather than an invisible one -- it was outside
    /// the denominator entirely until it was measured.
    unstamped_form: bool,
}

/// Every REACHABLE call instruction in `mf` and its nested (hoisted) bodies.
///
/// Nested fns are included because closure conversion splits them out of this
/// body and they are emitted as its siblings, so their calls are part of the
/// same instance's emitted output.
fn reachable_call_rows(mf: &MirFunc, out: &mut Vec<CallRow>) {
    for b in skull_mir::cfg::reachable_blocks_from(mf, mf.entry) {
        for &id in &mf.blocks[b].insts {
            let mut producer = None;
            let (target, unstamped_form) = match &mf.insts[id] {
                Inst::Call { func, .. } | Inst::CallMulti { func, .. } => (Some(*func), false),
                Inst::CallValue { callee, .. } | Inst::CallValueMulti { callee, .. } => {
                    let (p, fref) = indirect_producer(mf, *callee);
                    producer = Some(p);
                    // A minted workspace symbol IS a target: it is spelled, so
                    // it joins on the ordinary direct-call path rather than
                    // being admitted on the strength of the call FORM.
                    (fref, false)
                }
                // Emitted by a bounded loop and NOT stamped by `emit_cur`, so
                // it can never carry a site. In the denominator regardless: a
                // call form outside the population is a silent hole, not a
                // clean measurement.
                Inst::CallBoundMulti { .. } => (None, true),
                _ => continue,
            };
            out.push(CallRow {
                inst: id,
                site: mf.call_sites.get(&id).cloned(),
                target,
                spec_key: mf.spec_bindings.get(&id).cloned(),
                producer,
                unstamped_form,
            });
        }
    }
    for nf in &mf.nested_fns {
        reachable_call_rows(nf, out);
    }
}

/// `(declaration, substitution) -> Instance`, so an owner recorded by lowering
/// can be resolved to the graph's instance identity.
///
/// Built from `InstanceGraph::substitutions`, which is the graph's own record,
/// so this is a lookup rather than a second derivation of instance identity.
fn owner_index(g: &InstanceGraph) -> HashMap<skull_mir::mir::FuncRef, Vec<(MonoSubst, Instance)>> {
    let mut ix: HashMap<skull_mir::mir::FuncRef, Vec<(MonoSubst, Instance)>> = HashMap::new();
    for (inst, subst) in &g.substitutions {
        ix.entry(inst.func).or_default().push((subst.clone(), inst.clone()));
    }
    ix
}

fn resolve_owner<'a>(
    ix: &'a HashMap<skull_mir::mir::FuncRef, Vec<(MonoSubst, Instance)>>,
    owner: &OwnerInstance,
) -> Option<&'a Instance> {
    ix.get(&owner.func)?.iter().find(|(s, _)| *s == owner.subst).map(|(_, i)| i)
}

/// Whether skull lowers a standalone body for this declaration at all.
///
/// Delegates to [`crate::mir_emit::is_cached_body`], the definition loop's OWN
/// admission rule, plus the presence of a body to lower. A restatement of that
/// rule here would be free to be weaker and silently drift, which is how the
/// census would end up measuring a population emission does not have.
fn has_lowered_body(ws: &Workspace, fref: skull_mir::mir::FuncRef) -> bool {
    let f = &ws.module(fref.module).functions[fref.func];
    crate::mir_emit::is_cached_body(f)
        && ws
            .module(fref.module)
            .bodies
            .contains_key(&bone_semantic::hir::BodyOwner::Function(fref.func))
}

/// Measure the join over `g`'s expanded instances, at the phase emission
/// consumes.
pub fn census(
    ws: &Workspace,
    scx: &ScalarCtx,
    g: &InstanceGraph,
    bodies: &ConcreteBodies,
) -> SiteJoinCensus {
    census_detailed(ws, scx, g, bodies, false)
}

/// [`census`], optionally recording one row per non-joined call in
/// [`SiteJoinCensus::rows`].
///
/// Detail is a parameter rather than always-on because the rows are per CALL
/// and the corpus has hundreds of thousands; the manifest writes them only when
/// asked for a dump directory.
pub fn census_detailed(
    ws: &Workspace,
    scx: &ScalarCtx,
    g: &InstanceGraph,
    bodies: &ConcreteBodies,
    detail: bool,
) -> SiteJoinCensus {
    let mut c = SiteJoinCensus::default();
    let ix = owner_index(g);
    // Every symbol the workspace mints for a function -- the second view the
    // fail-open check above compares against.
    let mangled: std::collections::HashSet<ustr::Ustr> = ws
        .modules
        .iter()
        .flat_map(|m| m.functions.iter().map(|(_, f)| f))
        .map(|f| ustr::Ustr::from(&skull_mir::mangle::mangle_fn(ws, f)))
        .collect();
    let mangled = &mangled;
    let mut cache = skull_transform::InlineCache::default();

    for inst in &g.expanded {
        let f = &ws.module(inst.func.module).functions[inst.func.func];
        let skip = |c: &mut SiteJoinCensus, why: BodySkip| {
            *c.bodies_skipped.entry(why).or_insert(0) += 1;
        };
        if f.is_coroutine {
            skip(&mut c, BodySkip::Coroutine);
            continue;
        }
        if !has_lowered_body(ws, inst.func) {
            skip(&mut c, BodySkip::NoLoweredBody);
            continue;
        }
        let Some(subst) = g.substitutions.get(inst) else {
            skip(&mut c, BodySkip::NoSubstitution);
            continue;
        };
        let mut mf = match skull_mir::lower::lower_function_spec(
            ws,
            scx,
            inst.func.module,
            inst.func.func,
            subst,
        ) {
            Ok(mf) => mf,
            Err(_) => {
                skip(&mut c, BodySkip::LowerFailed);
                continue;
            }
        };
        // THE emitted body: the same sequence `emit_specialized_callees` runs.
        let name = skull_mir::mangle::mangle_fn(ws, f);
        if crate::mir_emit::prepare_instance_body(
            ws,
            scx,
            inst.func,
            subst,
            &name,
            &mut mf,
            &mut cache,
        )
        .is_err()
        {
            skip(&mut c, BodySkip::PrepareFailed);
            continue;
        }
        c.bodies_measured += 1;

        let mut rows: Vec<CallRow> = Vec::new();
        reachable_call_rows(&mf, &mut rows);
        c.indirect_unrecorded_symbol += unrecorded_symbol_operands(&mf, mangled);
        for row in rows {
            c.call_insts += 1;
            let (cause, op, note) = classify(ws, &mut c, g, &mf, &ix, bodies, inst, &row, detail);
            *c.examined_by_op.entry(op).or_insert(0) += 1;
            if detail && (cause.is_some() || row.target.is_none()) {
                let span = mf
                    .inst_spans
                    .get(row.inst)
                    .map(|s| format!("{}:{}", s.file.as_str(), s.line_range.0 + 1))
                    .unwrap_or_else(|| "<no span>".into());
                let owner = row
                    .site
                    .as_ref()
                    .and_then(|cs| mf.site_owners.get(cs.owner.0 as usize))
                    .map(|o| {
                        format!(
                            "{}/{}",
                            ws.module(o.func.module).functions[o.func.func].name,
                            if o.subst.is_empty() {
                                "EMPTY".to_string()
                            } else {
                                o.subst.len().to_string()
                            }
                        )
                    })
                    .unwrap_or_else(|| "<none>".into());
                let target = row
                    .target
                    .map(|t| {
                        let f = &ws.module(t.module).functions[t.func];
                        format!("{}/{}g", f.name, f.generics.len())
                    })
                    .unwrap_or_else(|| "<indirect>".into());
                let key = match &row.spec_key {
                    None => "nokey".to_string(),
                    Some(k) => format!("key{}", k.len()),
                };
                c.rows.push(format!(
                    "{}\t{op}\t{span}\towner={owner}\ttarget={target}\t{key}\tin={}\t{note}",
                    cause.map(Unjoined::label).unwrap_or("admitted-indirect"),
                    g.identity(ws, inst),
                ));
            }
            let cs = &row.site;
            if let Some(cause) = cause {
                *c.unjoined.entry(cause).or_insert(0) += 1;
                *c.unjoined_by_op.entry(op).or_insert(0) += 1;
                let per_cause = c.examples.iter().filter(|e| e.starts_with(cause.label())).count();
                if per_cause < 3 {
                    let target_name = row
                        .target
                        .map(|t| ws.module(t.module).functions[t.func].name.to_string())
                        .unwrap_or_else(|| "<indirect>".into());
                    let detail = match &cs {
                        Some(cs) => {
                            let owner = mf.site_owners.get(cs.owner.0 as usize).map(|o| {
                                let f = &ws.module(o.func.module).functions[o.func.func];
                                format!(
                                    "owner `{}` subst {}",
                                    f.name,
                                    if o.subst.is_empty() {
                                        "EMPTY".to_string()
                                    } else {
                                        format!("{}", o.subst.len())
                                    }
                                )
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
                    c.examples.push(format!(
                        "{} in `{}`: {detail} -> target `{target_name}`",
                        cause.label(),
                        g.identity(ws, inst),
                    ));
                }
            }
        }
    }
    c
}

/// The result of comparing a resolved callee against the call that targets it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CalleeCheck {
    /// Declaration AND concrete instantiation agree.
    Instance,
    /// Declaration agrees and the callee is non-generic, so it has exactly one
    /// instance and there are no arguments to compare.
    NonGeneric,
    /// The declarations disagree.
    DeclMismatch,
    /// Declarations agree, the callee is generic, and the instantiations differ.
    InstanceMismatch,
    /// Generic callee, no instantiation key at all.
    NoKey,
    /// Generic callee, key length != the declaration's generic count.
    KeyArity,
    /// Generic callee, right-length key holding a `None`.
    PartialKey,
    /// The resolved instance has no substitution recorded in the graph.
    ResolvedUnrecorded,
}

impl CalleeCheck {
    /// The census bucket this verdict lands in. Total by construction: every
    /// variant maps to a verified kind or a named rejection, so the caller
    /// needs no catch-all.
    pub fn outcome(self) -> Result<Verified, Unjoined> {
        match self {
            Self::Instance => Ok(Verified::Instance),
            Self::NonGeneric => Ok(Verified::NonGeneric),
            Self::DeclMismatch => Err(Unjoined::CalleeMismatch),
            Self::InstanceMismatch => Err(Unjoined::CalleeInstanceMismatch),
            Self::NoKey => Err(Unjoined::CalleeNoKey),
            Self::KeyArity => Err(Unjoined::CalleeKeyArity),
            Self::PartialKey => Err(Unjoined::CalleePartialKey),
            Self::ResolvedUnrecorded => Err(Unjoined::CalleeResolvedUnrecorded),
        }
    }
}

/// A verdict that counts as a join.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verified {
    /// Generic arguments were compared.
    Instance,
    /// Non-generic callee: one instance, nothing to compare.
    NonGeneric,
}

/// **THE callee comparison**, extracted so it can be tested directly against a
/// deliberately wrong key rather than only observed through corpus totals.
///
/// Declaration equality is the WEAKER question: two instantiations of one
/// generic compare equal under it, which is precisely the distinction
/// instance-driven emission exists to make. So generic arguments participate
/// whenever the call carries a complete key, and the absence of one is reported
/// rather than treated as agreement.
pub fn check_callee(
    ws: &Workspace,
    g: &InstanceGraph,
    callee_inst: &Instance,
    target: skull_mir::mir::FuncRef,
    spec_key: Option<&skull_mir::lower::SpecKey>,
) -> CalleeCheck {
    if callee_inst.func != target {
        return CalleeCheck::DeclMismatch;
    }
    let generics = ws.module(target.module).functions[target.func].generics.len();
    if generics == 0 {
        return CalleeCheck::NonGeneric;
    }
    // A complete key has one reified entry per declared generic. Each way of
    // falling short is its own answer: they have different causes and only one
    // of them is conditional specialization.
    let Some(key) = spec_key else { return CalleeCheck::NoKey };
    if key.len() != generics {
        return CalleeCheck::KeyArity;
    }
    if key.iter().any(Option::is_none) {
        return CalleeCheck::PartialKey;
    }
    let call_subst = skull_mir::lower::spec_subst_from_key(ws, target, key);
    match g.substitutions.get(callee_inst) {
        Some(resolved) if *resolved == call_subst => CalleeCheck::Instance,
        Some(_) => CalleeCheck::InstanceMismatch,
        None => CalleeCheck::ResolvedUnrecorded,
    }
}

/// Resolve one call instruction, counting a success into `c` and returning the
/// cause plus the operation label when it does not join.
///
/// A join requires the resolved callee to be the same CONCRETE INSTANCE the
/// call targets, not merely the same declaration. Declaration equality is the
/// weaker question: two instantiations of one generic compare equal under it,
/// and telling them apart is exactly what instance-driven emission is for. So
/// the comparison is staged --
///
/// - declaration disagrees -> [`Unjoined::CalleeMismatch`];
/// - declaration agrees and is NON-GENERIC -> one instance exists, so the
///   declaration match IS the instance match (counted apart);
/// - declaration agrees, is generic, and the call carries a COMPLETE
///   instantiation key -> reconstruct its substitution and compare against the
///   resolved instance's own;
/// - declaration agrees, is generic, and the instance cannot be reconstructed
///   -> UNVERIFIED, never a silent success, under the cause that applies: no
///   key at all, a key of the wrong ARITY (positional, so a wrong length names
///   another function's instantiation), a right-length key holding a `None`,
///   or a resolved instance the GRAPH has no substitution for. Four causes,
///   because only one of them is conditional specialization.
fn classify(
    ws: &Workspace,
    c: &mut SiteJoinCensus,
    g: &InstanceGraph,
    mf: &MirFunc,
    ix: &HashMap<skull_mir::mir::FuncRef, Vec<(MonoSubst, Instance)>>,
    bodies: &ConcreteBodies,
    measured: &Instance,
    row: &CallRow,
    detail: bool,
) -> (Option<Unjoined>, &'static str, String) {
    let none = String::new();
    if row.unstamped_form {
        return (Some(Unjoined::UnstampedForm), "<unstamped>", none);
    }
    if let Some(producer) = row.producer {
        *c.indirect_forms_by_producer.entry(producer).or_insert(0) += 1;
    }
    // AN INDIRECT CALL IS DECIDED FIRST, before any site or owner lookup.
    // Nothing about it is looked up: emission renders the callee OPERAND, so
    // whether the graph happened to record an obligation at this expression is
    // not what makes the call safe -- what the operand is, is. Deciding it after
    // the lookup filed 62 of bitgun's indirect calls under `site-not-in-body`,
    // which named the wrong cause for all of them.
    //
    // A `named-function` operand is NOT here: it carries a `target`, so it left
    // through the direct-call path above and joins like any other spelled call.
    if row.target.is_none() {
        let producer = row.producer.expect("a call with no target is an indirect form");
        let op = row.site.as_ref().map(|cs| cs.site.op.name()).unwrap_or("<none>");
        c.admitted_indirect += 1;
        *c.admitted_indirect_by_op.entry(op).or_insert(0) += 1;
        let note = if detail { format!("producer={}", producer.label()) } else { none };
        return (None, op, note);
    }
    let Some(cs) = &row.site else { return (Some(Unjoined::NoSite), "<none>", none) };
    let op = cs.site.op.name();
    let Some(owner) = mf.site_owners.get(cs.owner.0 as usize) else {
        return (Some(Unjoined::OwnerNotInTable), op, none);
    };
    let Some(owner_inst) = resolve_owner(ix, owner) else {
        return (Some(Unjoined::OwnerNotAnInstance), op, none);
    };
    let Some(body) = bodies.bodies.get(owner_inst) else {
        return (Some(Unjoined::NoBody), op, none);
    };
    let resolved: BTreeMap<Site, &Callee> = body.callees().into_iter().collect();

    let Some(callee) = resolved.get(&cs.site) else {
        let note = if detail {
            let mut same_expr: Vec<String> = resolved
                .keys()
                .filter(|s| s.expr == cs.site.expr)
                .map(|s| s.op.name().to_string())
                .collect();
            same_expr.sort();
            same_expr.dedup();
            format!(
                "expr={:?} ops_at_expr=[{}] body_sites={}",
                cs.site.expr,
                same_expr.join(","),
                resolved.len()
            )
        } else {
            none
        };
        return (Some(Unjoined::SiteNotInBody), op, note);
    };
    let callee_inst = callee.instance();

    // INDIRECT CALLS ARE NOT JOINS. `Inst::CallValue` names a function VALUE,
    // not a declaration, so there is no target to compare the resolved instance
    // against. Counting them as joined made "every join verifies the concrete
    // instance" false for exactly those calls.
    //
    // They are admitted as their own population rather than rejected: the graph
    // DOES resolve the `FnValue` site to an instance, so the obligation is
    // recorded -- what is missing is a MIR-side target to confirm it. Verifying
    // them means tracing the value's producer back to a `Global`/`Lambda` and
    // checking ITS instance, which is real work and is named as such rather
    // than assumed away.
    // Every remaining row is a DIRECT call: the indirect branch returned above.
    let target = row.target.expect("indirect calls are decided before the site lookup");

    match check_callee(ws, g, callee_inst, target, row.spec_key.as_ref()).outcome() {
        Err(cause) => {
            let note = if detail {
                format!("resolved={}", g.identity(ws, callee_inst))
            } else {
                none
            };
            (Some(cause), op, note)
        }
        Ok(kind) => {
            c.joined += 1;
            *c.joined_by_op.entry(op).or_insert(0) += 1;
            match kind {
                Verified::Instance => c.joined_instance_verified += 1,
                Verified::NonGeneric => c.joined_non_generic += 1,
            }
            match callee {
                Callee::Rock(_) => c.joined_rock += 1,
                Callee::Boundary(..) => c.joined_boundary += 1,
            }
            if owner_inst != measured {
                c.joined_spliced += 1;
            }
            (None, op, none)
        }
    }
}
