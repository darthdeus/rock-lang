//! JavaScript-specific statements used by the placement walker.

use super::*;
use skull_mir::cfg::structure::CountedLoop;

impl Emitter<'_> {
    pub(super) fn push(&mut self, line: &str) {
        for _ in 0..self.indent {
            self.out.push_str("  ");
        }
        self.out.push_str(line);
        self.out.push('\n');
    }

    pub(super) fn emit_unreachable(&mut self) {
        self.push("throw new Error(\"rock: reached an unreachable block\");");
    }

    pub(super) fn is_runtime_value(&self, v: Idx<Inst>) -> bool {
        matches!(&self.mf.insts[v], Inst::Global(n) if is_runtime_global(n))
    }

    /// The rejection for a body whose control flow is too deep for V8.
    pub(super) fn nesting_error(&self, b: Idx<Block>) -> CompilerError {
        let term = &self.mf.blocks[b].term;
        let mut cond = None;
        term.uses_each(&mut |v| cond = cond.or(Some(v)));
        let span = cond
            .into_iter()
            .chain(self.mf.blocks[b].insts.iter().copied())
            .find_map(|i| self.mf.inst_spans.get(i).copied())
            .unwrap_or_default();
        self.nesting_error_at(span)
    }

    pub(super) fn nesting_error_inst(&self, inst: Idx<Inst>) -> CompilerError {
        self.nesting_error_at(self.mf.inst_spans.get(inst).copied().unwrap_or_default())
    }

    fn nesting_error_at(&self, span: Span) -> CompilerError {
        let mut error = CompilerError::new_with_label(
            span,
            format!(
                "`{}` nests control flow too deeply for the JS backend: node's parser runs out \
                 of stack on the emitted function. Split it into smaller functions or flatten \
                 the nesting",
                self.mf.name
            ),
        );
        error.kind = skull_parser::CompilerErrorKind::TargetLimit;
        error
    }

    /// Whether inst `iid` produces a statement in [`Self::emit_block_effects`]:
    /// a store, a materialized (bound) value, or a statement-position call.
    /// Inlinable and dead-pure insts emit nothing.
    pub(super) fn inst_emits(&self, iid: Idx<Inst>) -> bool {
        let inst = &self.mf.insts[iid];
        if matches!(inst, Inst::Store { .. } | Inst::StoreIndex { .. }) {
            return true;
        }
        let k = raw(iid) as usize;
        if inst.produces_value() {
            (!self.inlinable[k] && self.use_count[k] > 0)
                || (self.use_count[k] == 0 && effects::evaluated_when_unused(inst))
        } else {
            true
        }
    }

    pub(super) fn block_has_statement(&self, b: Idx<Block>) -> bool {
        self.mf.blocks[b].insts.iter().any(|&i| self.inst_emits(i))
    }

    pub(super) fn raises(&self, b: Idx<Block>) -> bool {
        matches!(self.mf.blocks[b].term, Term::Unreachable)
            && self.mf.blocks[b].insts.last().is_some_and(|&last| {
                matches!(&self.mf.insts[last], Inst::Call { func, .. }
                if skull_mir::intrinsics::never_returns(
                    &self.ws.module(func.module).functions[func.func]
                ))
            })
    }

    pub(super) fn is_nil(&self, iid: Idx<Inst>) -> bool {
        matches!(self.mf.insts[iid], Inst::Const(Lit::Nil))
    }

    /// The bound is a `Load` of a hoisted preheader cell (or a const), rendered
    /// inline so it never depends on a header-block temp the loop skips.
    pub(super) fn counted_slot_bound(
        &mut self,
        li: &CountedLoop,
    ) -> Result<(String, String), CompilerError> {
        Ok((
            cell_name(self.mf, li.slot),
            self.render_inst_expr(li.bound)?,
        ))
    }

    /// JS access paths of an intrinsic call's result lanes. `None` means the
    /// callee is a real function, which packs its result into the return bank;
    /// a mapped intrinsic with an unprojectable multi-lane result is an error,
    /// because no intrinsic writes that bank.
    pub(super) fn intrinsic_lane_paths(
        &self,
        iid: Idx<Inst>,
        func: FuncRef,
        lanes: usize,
    ) -> Result<Option<Vec<String>>, CompilerError> {
        if lanes < 2 {
            return Ok(None);
        }
        let cf = &self.ws.module(func.module).functions[func.func];
        if js_supply(self.ws, cf).is_none() {
            return Ok(None);
        }
        let Some(plan) = self.mf.call_abi_plans.get(&iid) else {
            return Err(err(format!(
                "JS subset: intrinsic `{}` has no call ABI plan for its multi-lane result",
                cf.name
            )));
        };
        let Some(result) = plan.representation().result() else {
            return Err(err(format!(
                "JS subset: intrinsic `{}` has no ABI result for its multi-lane call",
                cf.name
            )));
        };
        let mut out = Vec::new();
        if !self.collect_lane_paths(plan, result, String::new(), &mut out) || out.len() != lanes {
            return Err(err(format!(
                "JS subset: intrinsic `{}` has a multi-lane result that cannot be projected from one JS value",
                cf.name
            )));
        }
        Ok(Some(out))
    }

    /// Depth-first lane paths of one representation, in the scalar-ABI lane order
    /// (`RepArena::ret_slot` flattens the same way). `false` when a lane cannot be
    /// addressed on a JS value: a nullable tuple threads a presence lane, and a
    /// dotted leaf path has no single-key spelling the box agrees on.
    fn collect_lane_paths(
        &self,
        plan: &skull_mir::lower::BackendFnAbiPlan,
        rep: skull_mir::lower::RepId,
        prefix: String,
        out: &mut Vec<String>,
    ) -> bool {
        let rep_plan = plan.rep_plan(rep);
        match rep_plan.layout() {
            skull_mir::lower::RepLayout::Tuple { slots, nullable } => {
                if *nullable {
                    return false;
                }
                for (i, slot) in slots.iter().enumerate() {
                    if !self.collect_lane_paths(plan, *slot, format!("{prefix}[{i}]"), out) {
                        return false;
                    }
                }
                true
            }
            skull_mir::lower::RepLayout::Leaves(_) => {
                let Some(paths) = rep_plan.leaf_paths() else {
                    return false;
                };
                if paths.iter().any(|path| path.as_str().contains('.')) {
                    return false;
                }
                for path in paths {
                    let Ok(key) = js_string_lit(path.as_str()) else {
                        return false;
                    };
                    out.push(format!("{prefix}[{key}]"));
                }
                true
            }
            skull_mir::lower::RepLayout::Opaque => {
                out.push(prefix);
                true
            }
        }
    }

    /// A multi-value capture: `d0 = <call>; d1 = __rock_ret1; ...` -- leaf 0 from
    /// the call's own return value, leaves 1..N from the scratch bank
    /// ([`Self::emit_return`] is the publishing half). A single dest is a plain
    /// `d0 = <call>;`; the ABI returns one leaf as a bare scalar.
    ///
    /// The bank is caller-saved-by-immediacy: this emits the call and EVERY slot
    /// read as one indivisible run of statements, so nothing can execute between
    /// publish and capture. That is the whole safety argument, and it holds only
    /// while this stays a single unit -- never split the reads out, and never let
    /// a multi-value call render as an inlined subexpression (it cannot today:
    /// `CallMulti`/`CallValueMulti` have dests and always emit as statements).
    pub(super) fn multi_assign(&self, dests: &[Idx<Cell>], call: &str) -> String {
        match dests {
            // A zero-leaf result (`Storage`) is still called for its effects.
            [] => format!("{call};"),
            [d] => format!("{} = {call};", cell_name(self.mf, *d)),
            _ => {
                let mut s = format!("{} = {call};", cell_name(self.mf, dests[0]));
                for (i, d) in dests.iter().enumerate().skip(1) {
                    s.push_str(&format!(" {} = {};", cell_name(self.mf, *d), ret_slot(i)));
                }
                s
            }
        }
    }
}
