//! **The MIR-call -> `ConcreteBody` join, measured** (plan S 9, Stage 3).
//!
//! Instance-driven emission has to answer "what does THIS call instruction
//! call" from the graph's own resolution rather than by resolving again. That
//! needs a join key, and `MirFunc::call_sites` is it: lowering stamps a call
//! form with the [`skull_mir::site::CallSite`] it came from, and the walk
//! recorded what each site resolved to -- every call FORM, `CallBoundMulti`
//! included, plus the fn-value symbol MINTS, which are callee obligations even
//! though they are not calls.
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
    /// The call was lowered while the expression arena was NOT the one this
    /// body's sites are keyed in (`MirFunc::foreign_arena_calls`) -- a
    /// struct-default initializer, whose `BodyOwner::StructDefaults` arena is
    /// installed while `site_owner` still names the enclosing function.
    ///
    /// Stamping nothing is FAIL-CLOSED, not final: an index from the other arena
    /// could join through whatever expression sits at it here. The real identity
    /// needs a body/arena DOMAIN in `Site` -- `BodyOwner` plus expression plus
    /// operation -- shared by the graph and lowering, and it must exist before
    /// emission consumes the join.
    ForeignArena,
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
    /// Generic Rock callee for which the call recorded NO concrete instance.
    /// The identity record covers every COMPLETE instantiation, so this is a
    /// call whose bindings do not all reify here -- a caller-generic slot inside
    /// a body that is itself still generic.
    CalleeNoInstance,
    /// The recorded instance names a different DECLARATION than the call
    /// targets. An invariant break in the recorder, kept apart from a missing
    /// record because it cannot be a consequence of policy.
    CalleeClaimDeclMismatch,
    /// The resolved instance itself has no substitution recorded in the graph,
    /// so there is nothing to compare the reconstructed one against. An
    /// invariant finding about the GRAPH, not about the call.
    CalleeResolvedUnrecorded,
}

impl Unjoined {
    pub fn label(self) -> &'static str {
        match self {
            Self::NoSite => "no-site",
            Self::ForeignArena => "foreign-arena",
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
    /// REACHABLE callee obligations examined. THE DENOMINATOR, and it is not
    /// call instructions alone: a fn-value symbol MINT is an obligation too, and
    /// naming this `obligations_examined` while counting mints in it was a number that
    /// described itself wrongly.
    pub obligations_examined: usize,
    /// How many of [`Self::obligations_examined`] are fn-value symbol MINTS. The
    /// rest are call instructions, so the composition is stated rather than
    /// inferred.
    pub symbol_mints: usize,
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
    /// Joined calls to a BOUNDARY, verified as a `BoundaryId`: declaration plus
    /// class, which is the whole of a boundary's identity on this side. Counted
    /// apart because it says nothing about Rock generic arguments -- a boundary
    /// has none to say anything about.
    pub joined_boundary_verified: usize,
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
        if total != self.obligations_examined {
            out.push(format!(
                "joined {} + admitted-indirect {} + unjoined {} != obligations examined {}",
                self.joined,
                self.admitted_indirect,
                self.unjoined_total(),
                self.obligations_examined
            ));
        }
        let verified =
            self.joined_instance_verified + self.joined_non_generic + self.joined_boundary_verified;
        if verified != self.joined {
            out.push(format!(
                "instance-verified {} + non-generic {} + boundary {} != joined {}",
                self.joined_instance_verified,
                self.joined_non_generic,
                self.joined_boundary_verified,
                self.joined
            ));
        }
        // The boundary count taken from the CALLEE ARM and the one taken from
        // the check's VERDICT must agree: a `check_callee` that answered
        // `NonGeneric` for a boundary would still balance every sum above.
        if self.joined_boundary_verified != self.joined_boundary {
            out.push(format!(
                "boundary-verified {} != boundary callees {}",
                self.joined_boundary_verified, self.joined_boundary
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
        if examined_ops != self.obligations_examined {
            out.push(format!("examined-by-op {examined_ops} != obligations examined {}", self.obligations_examined));
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
        self.obligations_examined += other.obligations_examined;
        self.joined += other.joined;
        self.joined_rock += other.joined_rock;
        self.joined_boundary += other.joined_boundary;
        self.joined_spliced += other.joined_spliced;
        self.joined_instance_verified += other.joined_instance_verified;
        self.joined_non_generic += other.joined_non_generic;
        self.symbol_mints += other.symbol_mints;
        self.joined_boundary_verified += other.joined_boundary_verified;
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
    /// Not an operand at all: an `Inst::CallBoundMulti`, the `for` protocol's
    /// `next` witness on the caller-generic path. It names no callee BY
    /// CONSTRUCTION -- the concrete one is known only per instantiation, and the
    /// inliner substitutes it from the site's `BoundTree` at the splice. A body
    /// that still carries one may not be emitted at all, which is a separate
    /// rejection and is why this can never reach a spelled symbol.
    BoundPlaceholder,
}

impl IndirectProducer {
    pub fn label(self) -> &'static str {
        match self {
            Self::NamedFunction => "named-function",
            Self::HoistedNestedFn => "hoisted-nested-fn",
            Self::RuntimeValue => "runtime-value",
            Self::BoundPlaceholder => "bound-placeholder",
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
    /// The call's own claim about WHICH concrete instance it targets
    /// (`MirFunc::callee_instances`). This is the identity record, recorded for
    /// every complete instantiation regardless of what the specialization
    /// POLICY decided, so it is what the comparison reads.
    callee_inst: Option<OwnerInstance>,
    /// For an indirect call, what produced the callee operand. `None` for a
    /// direct call.
    producer: Option<IndirectProducer>,
    /// Whether this row is a fn-value SYMBOL MINT rather than a call.
    is_mint: bool,
    /// Whether lowering deliberately withheld a site because the expression
    /// arena was not this body's.
    foreign_arena: bool,
}

/// Every REACHABLE call instruction in `mf` and its nested (hoisted) bodies.
///
/// Nested fns are included because closure conversion splits them out of this
/// body and they are emitted as its siblings, so their calls are part of the
/// same instance's emitted output.
fn reachable_call_rows(mf: &MirFunc, out: &mut Vec<CallRow>) {
    for b in skull_mir::cfg::reachable_blocks_from(mf, mf.entry) {
        for &id in &mf.blocks[b].insts {
            // A fn-value MINT is a callee obligation in the denominator, not a
            // call: it is the one place a declaration becomes a spelled value,
            // and an indirect call through it spells nothing of its own. If it
            // did not join here, a mint that is PASSED or STORED rather than
            // called in place would be verified nowhere at all.
            if let Some(fref) = mf.fn_value_symbols.get(&id) {
                out.push(CallRow {
                    inst: id,
                    site: mf.call_sites.get(&id).cloned(),
                    target: Some(*fref),
                    spec_key: None,
                    callee_inst: mf.callee_instances.get(&id).cloned(),
                    producer: None,
                    is_mint: true,
                    foreign_arena: mf.foreign_arena_calls.contains(&id),
                });
                continue;
            }
            let mut producer = None;
            let target = match &mf.insts[id] {
                Inst::Call { func, .. } | Inst::CallMulti { func, .. } => Some(*func),
                Inst::CallValue { callee, .. } | Inst::CallValueMulti { callee, .. } => {
                    let (p, fref) = indirect_producer(mf, *callee);
                    producer = Some(p);
                    // A minted workspace symbol IS a target: it is spelled, so
                    // it joins on the ordinary direct-call path rather than
                    // being admitted on the strength of the call FORM.
                    fref
                }
                // The `for` protocol's `next` witness on the caller-generic
                // path. It NAMES no callee -- the concrete one is only known per
                // instantiation, and the inliner substitutes it at the splice --
                // so it carries a site but no target, exactly like an indirect
                // call, and is admitted under its own producer class.
                Inst::CallBoundMulti { .. } => {
                    producer = Some(IndirectProducer::BoundPlaceholder);
                    None
                }
                _ => continue,
            };
            out.push(CallRow {
                inst: id,
                site: mf.call_sites.get(&id).cloned(),
                target,
                spec_key: mf.spec_bindings.get(&id).cloned(),
                callee_inst: mf.callee_instances.get(&id).cloned(),
                producer,
                is_mint: false,
                foreign_arena: mf.foreign_arena_calls.contains(&id),
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
/// Every symbol the workspace mints for a function -- the second view
/// `unrecorded_symbol_operands` compares an indirect operand's NAME against.
fn mangled_symbols(ws: &Workspace) -> std::collections::HashSet<ustr::Ustr> {
    ws.modules
        .iter()
        .flat_map(|m| m.functions.iter().map(|(_, f)| f))
        .map(|f| ustr::Ustr::from(&skull_mir::mangle::mangle_fn(ws, f)))
        .collect()
}

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

/// Measure ONE already-prepared body, over the same classifier production runs.
///
/// The census lowers its bodies internally, so a test cannot otherwise put a
/// body whose recorded facts have been PERTURBED in front of it -- and a
/// mutation that stops at [`check_callee`] proves the comparison rather than
/// that the corrupted record travels through `reachable_call_rows` and lands in
/// a census bucket. `mf` must already be through
/// [`crate::mir_emit::prepare_instance_body`], which is the phase the join is
/// measured at.
pub fn measure_prepared_body(
    ws: &Workspace,
    g: &InstanceGraph,
    bodies: &ConcreteBodies,
    inst: &Instance,
    mf: &MirFunc,
) -> SiteJoinCensus {
    let mut c = SiteJoinCensus::default();
    let ix = owner_index(g);
    let mangled = mangled_symbols(ws);
    measure_body_into(ws, &mut c, g, &ix, bodies, &mangled, inst, mf, true);
    c
}

/// Measure ONE already-prepared body into `c`, exactly as [`census_detailed`]
/// measures each of its own.
///
/// Extracted because the census LOWERS its bodies internally, so nothing could
/// otherwise observe what the classifier does to a body whose recorded facts
/// have been perturbed. A mutation that stops at [`check_callee`] proves the
/// comparison and not that the corrupted record reaches it through
/// `reachable_call_rows`.
fn measure_body_into(
    ws: &Workspace,
    c: &mut SiteJoinCensus,
    g: &InstanceGraph,
    ix: &HashMap<skull_mir::mir::FuncRef, Vec<(MonoSubst, Instance)>>,
    bodies: &ConcreteBodies,
    mangled: &std::collections::HashSet<ustr::Ustr>,
    inst: &Instance,
    mf: &MirFunc,
    detail: bool,
) {
    c.bodies_measured += 1;

    let mut rows: Vec<CallRow> = Vec::new();
    reachable_call_rows(mf, &mut rows);
    c.indirect_unrecorded_symbol += unrecorded_symbol_operands(mf, mangled);
    for row in rows {
        c.obligations_examined += 1;
        if row.is_mint {
            c.symbol_mints += 1;
        }
        let (cause, op, note) = classify(ws, c, g, mf, ix, bodies, inst, &row, detail);
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
            let key = match (&row.spec_key, &row.callee_inst) {
                (None, None) => "nokey/noinst".to_string(),
                (Some(k), None) => format!("key{}/noinst", k.len()),
                (None, Some(i)) => format!("nokey/inst{}", i.subst.len()),
                (Some(k), Some(i)) => format!("key{}/inst{}", k.len(), i.subst.len()),
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
    let mangled = mangled_symbols(ws);
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
        measure_body_into(ws, &mut c, g, &ix, bodies, mangled, inst, &mf, detail);
    }
    c
}

/// The result of comparing a resolved callee against the call that targets it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CalleeCheck {
    /// Declaration AND concrete instantiation agree.
    Instance,
    /// The callee is a BOUNDARY: the declaration agrees and the boundary's
    /// identity is its declaration, class, target and spelling rather than a
    /// Rock instantiation. Counted apart from `NonGeneric` because a boundary
    /// CAN be generic -- what it cannot have is a Rock body per instantiation.
    Boundary(crate::instance_graph::BoundaryClass),
    /// Declaration agrees and the callee is non-generic, so it has exactly one
    /// instance and there are no arguments to compare.
    NonGeneric,
    /// The declarations disagree.
    DeclMismatch,
    /// Declarations agree, the callee is generic, and the instantiations differ.
    InstanceMismatch,
    /// Generic callee, and the call recorded no concrete instance at all.
    NoInstance,
    /// The recorded instance names a DIFFERENT declaration than the call
    /// targets.
    ClaimDeclMismatch,
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
    /// Generic arguments were compared.
    Instance,
    /// Non-generic callee: one instance, nothing to compare.
    NonGeneric,
    /// A boundary: verified as a `BoundaryId`, whose identity carries no Rock
    /// instantiation.
    Boundary,
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
    callee: &Callee,
    target: skull_mir::mir::FuncRef,
    call_inst: Option<&OwnerInstance>,
) -> CalleeCheck {
    let callee_inst = callee.instance();
    if callee_inst.func != target {
        return CalleeCheck::DeclMismatch;
    }
    // A BOUNDARY is verified as a boundary, not by reconstructing a Rock
    // instantiation it does not have. An opaque body -- `inline(lua)`,
    // `inline(js)`, `extern` -- is emitted once per declaration and target, so
    // its generic arguments do not participate in the symbol emission spells;
    // demanding a `SpecKey` for one asked a Rock question of a non-Rock callee
    // and reported the absence as `callee-no-key` (bitgun: 215 sites over 18
    // declarations, `__vec_data_get`, `transmute`, `phantom` among them).
    //
    // The identity that IS load-bearing for it -- declaration, class, target,
    // spelling -- is the `BoundaryId` the ARTIFACT mints, and the declaration
    // and class are what this side can see.
    if let Callee::Boundary(_, class) = callee {
        return CalleeCheck::Boundary(*class);
    }
    let generics = ws.module(target.module).functions[target.func].generics.len();
    if generics == 0 {
        return CalleeCheck::NonGeneric;
    }
    // A complete key has one reified entry per declared generic. Each way of
    // falling short is its own answer: they have different causes and only one
    // of them is conditional specialization.
    // The call's OWN claim, in the graph's vocabulary. Not the `SpecKey`: that
    // is the delivery record, present only where the policy chose to specialize,
    // so reading it as identity made a DECLINED specialization look exactly like
    // an uninferred callee.
    let Some(claim) = call_inst else { return CalleeCheck::NoInstance };
    // A claim for another declaration is an invariant break, not a policy
    // consequence -- the record is written beside the call it belongs to.
    if claim.func != target {
        return CalleeCheck::ClaimDeclMismatch;
    }
    match g.substitutions.get(callee_inst) {
        Some(resolved) if *resolved == claim.subst => CalleeCheck::Instance,
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
/// - the callee is a BOUNDARY -> verified as a `BoundaryId`, whose identity is
///   its declaration, class, target and spelling rather than a Rock
///   instantiation;
/// - declaration agrees, is generic, and the call RECORDED its concrete
///   instance (`MirFunc::callee_instances`) -> compare that substitution against
///   the resolved instance's own;
/// - declaration agrees, is generic, and no instance was recorded, or the
///   record names another declaration -> UNVERIFIED, never a silent success,
///   under the cause that applies. The record is the IDENTITY one, not the
///   `SpecKey` delivery record: reading delivery as identity made a declined
///   specialization indistinguishable from an uninferred callee.
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
    let Some(cs) = &row.site else {
        let cause = if row.foreign_arena { Unjoined::ForeignArena } else { Unjoined::NoSite };
        return (Some(cause), "<none>", none);
    };
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

    match check_callee(ws, g, callee, target, row.callee_inst.as_ref()).outcome() {
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
                Verified::Boundary => c.joined_boundary_verified += 1,
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
