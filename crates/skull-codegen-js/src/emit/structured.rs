//! The relooper's structured walk over one body, plus block duplication and labeled jumps.

use super::*;

// V8's parser recurses per nesting level, and these costs are its stack use
// relative to each other (in half-units of one `else if` link), measured on
// node 22 at its default stack size: 3700 `else if` links, 1500 nested blocks
// or 680 loop-and-`if` pairs exhaust it. The budget leaves a small margin below
// that; a body that gets past the estimate still fails loudly at load.
const NESTING_COST_ELSE_IF: u32 = 2;
const NESTING_COST_BRANCH: u32 = 5;
const NESTING_COST_LOOP: u32 = 6;
const MAX_NESTING: u32 = 7200;

impl StructuredEmit for Emitter<'_> {
    const BREAK_ON_EXIT_ENTRY: bool = true;
    // The empty-arm shortcut resumes the live arm at the empty arm's target,
    // which is wrong when that target is an intermediate block the live arm
    // skips past to reach the real merge (the inlined bounds-check
    // `if oob { r = nil } else { r = data[i] }` tail: both arms would reach
    // the merge and double-visit it), or a loop header whose `Goto` must lower
    // to `continue;`.
    const STRICT_EMPTY_FORWARD: bool = true;

    fn mir(&self) -> &MirFunc {
        self.mf
    }

    fn structure(&self) -> &Structure {
        &self.structure
    }

    fn counted_at(&self, b: Idx<Block>) -> Option<CountedLoop> {
        self.counted.get(&b).copied()
    }

    fn general_at(&self, b: Idx<Block>) -> Option<LoopRegion> {
        self.general.get(&b).cloned()
    }

    fn is_active(&self, b: Idx<Block>) -> bool {
        self.active.contains(&b)
    }

    fn set_active(&mut self, b: Idx<Block>, active: bool) {
        if active {
            self.active.insert(b);
        } else {
            self.active.remove(&b);
        }
    }

    fn mark_visited(&mut self, b: Idx<Block>) -> bool {
        self.visited.insert(b)
    }

    fn out(&mut self) -> &mut String {
        &mut self.out
    }

    fn indent(&mut self) -> &mut usize {
        &mut self.indent
    }

    fn render_cond(&mut self, cond: Idx<Inst>) -> Result<String, CompilerError> {
        self.render_val(cond)
    }

    fn block_effects(&mut self, b: Idx<Block>) -> Result<(), CompilerError> {
        self.emit_block_effects(b)
    }

    fn return_values(&mut self, vs: &[Idx<Inst>]) -> Result<(), CompilerError> {
        self.emit_return(vs)
    }

    fn unreachable(&mut self) {
        self.emit_unreachable();
    }

    fn block_has_statement(&self, b: Idx<Block>) -> bool {
        self.mf.blocks[b].insts.iter().any(|&i| self.inst_emits(i))
    }

    /// Natural loops are dispatched before the visited guard, so a revisit is
    /// a reducible-but-unstructured shape. A FORWARD shared tail -- a block
    /// reached from 2+ branch arms that is not the branch's ipdom (an
    /// or-pattern success arm, a multiple-`continue` latch tail) -- has no
    /// back-edge through it, so re-emitting it inline (tail DUPLICATION)
    /// terminates and is semantically identical: each dynamic execution
    /// follows exactly one copy. Bounded by `stop`, small, and lambda-/loop-free
    /// (`dup_region`). An enclosing active loop header (a multi-level jump) is
    /// NOT a forward tail -- that is the labeled-`continue` path, and if it
    /// reaches here it is genuinely unstructured. Anything else rejects.
    fn on_revisit(
        &mut self,
        b: Idx<Block>,
        stop: Option<Idx<Block>>,
        ctx: LoopCtx,
    ) -> Result<(), CompilerError> {
        if !self.active.contains(&b) {
            if let Some(region) = self.dup_region(b, stop, ctx) {
                return self.emit_dup(b, stop, ctx, &region);
            }
        }
        self.shared_tail_reject = true;
        Err(err(
            "JS subset: unstructured control flow (shared branch tail)",
        ))
    }

    fn unsupported_term(&self, term: &Term) -> CompilerError {
        if matches!(term, Term::Yield { .. }) {
            err("JS subset: `yield` not supported")
        } else {
            err("JS subset: `match`/switch not supported")
        }
    }

    /// A `Goto` to an ENCLOSING (non-innermost) loop's header/exit: a labeled
    /// `continue L<h>;`/`break L<h>;` (the inlined iterator next() skip-loop
    /// that continues the outer for-loop).
    fn outer_jump(&mut self, t: Idx<Block>) -> Option<String> {
        self.labeled_jump(t)
    }

    /// Short-circuit fold: a `match` range/tuple pattern lowers to nested
    /// branches whose failure arms share one tail block (`if A { if B { X }
    /// else { S } } else { S }`) -- reducible but not a natural loop, so the
    /// relooper cannot structure the doubly-reached `S`. Recognizing the chain
    /// (`A && B`; the `||` dual for a shared success arm) collapses it to a
    /// clean diamond that rejoins at the ipost-dominator. A folded inner
    /// block's pure reads hoist before the `if`.
    fn fold_branch(
        &mut self,
        mut cond_s: String,
        mut then_bb: Idx<Block>,
        mut else_bb: Idx<Block>,
    ) -> Result<(String, Idx<Block>, Idx<Block>), CompilerError> {
        loop {
            if let Some((c2, t2, e2)) = self.foldable_branch(then_bb) {
                if e2 == else_bb {
                    self.emit_block_effects(then_bb)?;
                    self.visited.insert(then_bb);
                    cond_s = format!("({cond_s} && {})", self.render_val(c2)?);
                    then_bb = t2;
                    continue;
                }
            }
            if let Some((c2, t2, e2)) = self.foldable_branch(else_bb) {
                if t2 == then_bb {
                    self.emit_block_effects(else_bb)?;
                    self.visited.insert(else_bb);
                    cond_s = format!("({cond_s} || {})", self.render_val(c2)?);
                    else_bb = e2;
                    continue;
                }
            }
            return Ok((cond_s, then_bb, else_bb));
        }
    }

    /// A `while cond`/`for` loop's header test (a `Branch` with one arm
    /// directly to the exit); none for a `loop {}`, whose `break`s and
    /// `continue`s fall out of the body's own branch structure (a branch arm
    /// ENTERING the exit block breaks via [`Self::BREAK_ON_EXIT_ENTRY`]).
    fn loop_test(&mut self, region: &LoopRegion) -> Result<Option<LoopTest>, CompilerError> {
        let Some(test) = region.test else {
            return Ok(None);
        };
        if let Some(test) = LoopTest::with_exit_arm(self.mf, test, region.exit) {
            return Ok(Some(test));
        }
        Err(err(
            "JS subset: unstructured loop (condition test has no direct exit arm)",
        ))
    }

    /// The bound is a `Load` of a hoisted preheader cell (or a const), rendered
    /// inline so it never depends on a header-block temp the loop skips.
    fn counted_slot_bound(&mut self, li: &CountedLoop) -> Result<(String, String), CompilerError> {
        Ok((
            cell_name(self.mf, li.slot),
            self.render_inst_expr(li.bound)?,
        ))
    }

    fn loop_label(&mut self, header: Idx<Block>) -> String {
        self.loop_markers.push(header);
        loop_marker(self.mf, header)
    }

    fn chains_after_else(&self, b: Idx<Block>) -> bool {
        let Term::Branch {
            then_bb, else_bb, ..
        } = self.mf.blocks[b].term
        else {
            return false;
        };
        if self.block_has_statement(b)
            || self.visited.contains(&b)
            || self.active.contains(&b)
            || self.counted.contains_key(&b)
            || self.general.contains_key(&b)
        {
            return false;
        }
        // A fold emits the folded block's reads ahead of the `if`.
        !self.fold_fires(then_bb, else_bb)
    }

    fn mark_next_branch_chained(&mut self) {
        self.next_branch_chained = true;
    }

    fn enter_nesting(&mut self, b: Idx<Block>, kind: NestingKind) -> Result<(), CompilerError> {
        let cost = match kind {
            NestingKind::Branch if std::mem::take(&mut self.next_branch_chained) => {
                NESTING_COST_ELSE_IF
            }
            NestingKind::Branch => NESTING_COST_BRANCH,
            NestingKind::Loop => NESTING_COST_LOOP,
        };
        self.nesting_costs.push(cost);
        self.nesting += cost;
        if self.nesting > MAX_NESTING {
            return Err(self.nesting_error(b));
        }
        Ok(())
    }

    fn exit_nesting(&mut self) {
        self.nesting -= self.nesting_costs.pop().expect("balanced enter_nesting");
    }

    fn enter_loop(&mut self, frame: LoopFrame) {
        self.loop_stack.push(frame);
    }

    fn exit_loop(&mut self) {
        self.loop_stack.pop();
    }
}

impl Emitter<'_> {
    pub(super) fn emit_unreachable(&mut self) {
        self.push("throw new Error(\"rock: reached an unreachable block\");");
    }

    pub(super) fn is_runtime_value(&self, v: Idx<Inst>) -> bool {
        matches!(&self.mf.insts[v], Inst::Global(n) if is_runtime_global(n))
    }

    fn fold_fires(&self, then_bb: Idx<Block>, else_bb: Idx<Block>) -> bool {
        self.foldable_branch(then_bb)
            .is_some_and(|(_, _, e2)| e2 == else_bb)
            || self
                .foldable_branch(else_bb)
                .is_some_and(|(_, t2, _)| t2 == then_bb)
    }

    /// The rejection for a body whose control flow is too deep for V8.
    fn nesting_error(&self, b: Idx<Block>) -> CompilerError {
        let term = &self.mf.blocks[b].term;
        let mut cond = None;
        term.uses_each(&mut |v| cond = cond.or(Some(v)));
        let span = cond
            .into_iter()
            .chain(self.mf.blocks[b].insts.iter().copied())
            .find_map(|i| self.mf.inst_spans.get(i).copied())
            .unwrap_or_default();
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
    fn inst_emits(&self, iid: Idx<Inst>) -> bool {
        let inst = &self.mf.insts[iid];
        if matches!(inst, Inst::Store { .. } | Inst::StoreIndex { .. }) {
            return true;
        }
        let k = raw(iid) as usize;
        if inst.produces_value() {
            (!self.inlinable[k] && self.use_count[k] > 0)
                || (matches!(inst, Inst::Call { .. } | Inst::CallValue { .. })
                    && self.use_count[k] == 0)
        } else {
            true // a non-value effect inst (rejected at emit if unsupported)
        }
    }

    pub(super) fn is_nil(&self, iid: Idx<Inst>) -> bool {
        matches!(self.mf.insts[iid], Inst::Const(Lit::Nil))
    }

    /// The set of blocks a forward shared tail `start` spans up to `stop` (the
    /// merge the duplicated copies rejoin at), or `None` if duplicating it would
    /// be unsound or too large. Boundaries: `stop`, and the enclosing loop's
    /// `cont`/`brk` (a `Goto` there is a `continue`/`break`, not a region block).
    /// Rejected regions (kept as an honest shared-tail reject):
    /// - larger than 8 blocks / 32 insts (a pathological blow-up),
    /// - a block that is a registered loop header (a back-edge -> non-terminating
    ///   re-walk),
    /// - a materialized (`let v<N>`) region value read from OUTSIDE the region
    ///   (a temp that would be undefined in the second copy's scope).
    fn dup_region(
        &self,
        start: Idx<Block>,
        stop: Option<Idx<Block>>,
        ctx: LoopCtx,
    ) -> Option<HashSet<Idx<Block>>> {
        const MAX_BLOCKS: usize = 8;
        const MAX_INSTS: usize = 32;
        let boundary = |b: Idx<Block>| Some(b) == stop || Some(b) == ctx.cont || Some(b) == ctx.brk;
        let mut region: HashSet<Idx<Block>> = HashSet::new();
        let mut insts = 0usize;
        let mut stack = vec![start];
        while let Some(b) = stack.pop() {
            if boundary(b) || !region.insert(b) {
                continue;
            }
            if region.len() > MAX_BLOCKS {
                return None;
            }
            if self.counted.contains_key(&b) || self.general.contains_key(&b) {
                return None;
            }
            insts += self.mf.blocks[b].insts.len();
            if insts > MAX_INSTS {
                return None;
            }
            skull_mir::cfg::successors(&self.mf.blocks[b].term, |t| {
                if !boundary(t) {
                    stack.push(t);
                }
            });
        }
        if self.region_temp_escapes(&region) {
            return None;
        }
        Some(region)
    }

    /// Whether a `let v<N>` materialized inside `region` is used by any inst or
    /// terminator OUTSIDE it. Cross-block values normally route through hoisted
    /// cells (the `sign` shape), so this should never fire -- but a temp that DID
    /// escape would be undefined in the duplicated copy's block scope, so the guard
    /// makes duplication sound regardless of the value model's guarantees.
    fn region_temp_escapes(&self, region: &HashSet<Idx<Block>>) -> bool {
        let mut mat: HashSet<Idx<Inst>> = HashSet::new();
        for &b in region {
            for &iid in &self.mf.blocks[b].insts {
                let k = raw(iid) as usize;
                if self.mf.insts[iid].produces_value()
                    && !self.inlinable[k]
                    && self.use_count[k] > 0
                {
                    mat.insert(iid);
                }
            }
        }
        if mat.is_empty() {
            return false;
        }
        for (b, blk) in self.mf.blocks.iter() {
            if region.contains(&b) {
                continue;
            }
            let mut hit = false;
            for &iid in &blk.insts {
                for_each_operand(&self.mf.insts[iid], &mut |op| hit |= mat.contains(&op));
            }
            blk.term.uses_each(&mut |op| hit |= mat.contains(&op));
            if hit {
                return true;
            }
        }
        false
    }

    /// Re-emit the forward shared-tail region `start..stop` inline (tail
    /// duplication). A self-contained re-walk that does NOT consult/mark
    /// `visited` (the whole point is to duplicate already-emitted blocks); it
    /// terminates because `dup_region` proved the region loop-free and bounded by
    /// `stop`. Its `let v<N>` temps live in the caller's fresh arm scope, so a
    /// same-named temp from the first copy does not collide.
    fn emit_dup(
        &mut self,
        start: Idx<Block>,
        stop: Option<Idx<Block>>,
        ctx: LoopCtx,
        region: &HashSet<Idx<Block>>,
    ) -> Result<(), CompilerError> {
        let mut cur = start;
        loop {
            if Some(cur) == stop {
                return Ok(());
            }
            if Some(cur) == ctx.brk {
                self.push("break;");
                return Ok(());
            }
            self.emit_block_effects(cur)?;
            match self.mf.blocks[cur].term.clone() {
                Term::Return(vs) => {
                    self.emit_return(&vs)?;
                    return Ok(());
                }
                Term::Unreachable => {
                    self.emit_unreachable();
                    return Ok(());
                }
                Term::Goto(t) => {
                    if Some(t) == ctx.cont {
                        self.push("continue;");
                        return Ok(());
                    }
                    if Some(t) == ctx.brk {
                        self.push("break;");
                        return Ok(());
                    }
                    if Some(t) == stop {
                        return Ok(());
                    }
                    if let Some(stmt) = self.labeled_jump(t) {
                        self.push(&stmt);
                        return Ok(());
                    }
                    cur = t;
                }
                Term::Branch {
                    cond,
                    then_bb,
                    else_bb,
                } => {
                    let cond_s = self.render_val(cond)?;
                    let merge = self.structure.ipdom(cur).filter(|&m| {
                        Some(m) != ctx.cont && Some(m) != ctx.brk && region.contains(&m)
                    });
                    let arm_stop = merge.map(Some).unwrap_or(stop);
                    self.push(&format!("if ({cond_s}) {{"));
                    self.indent += 1;
                    self.emit_dup(then_bb, arm_stop, ctx, region)?;
                    self.indent -= 1;
                    self.push("} else {");
                    self.indent += 1;
                    self.emit_dup(else_bb, arm_stop, ctx, region)?;
                    self.indent -= 1;
                    self.push("}");
                    match merge {
                        Some(m) => cur = m,
                        None => return Ok(()),
                    }
                }
                Term::Switch { .. } => return Err(err("JS subset: `match`/switch not supported")),
                Term::Yield { .. } => return Err(err("JS subset: `yield` not supported")),
            }
        }
    }

    /// A `Branch` block whose insts are all pure, non-erroring, side-effect-free
    /// (`Store`/`Call`/`Index`-bounds-check excluded), so its condition can be
    /// folded into an enclosing `&&`/`||` -- any statement it materializes is
    /// hoisted before the `if` and computed unconditionally, which is
    /// observably identical for a pure read. `Field`/`TupleIndex`/`Len` fault
    /// on null/undefined, so they stay out: hoisting one above the null check
    /// that guards it miscompiles safe-nav chains. Returns the branch's
    /// `(cond, then, else)`.
    fn foldable_branch(&self, b: Idx<Block>) -> Option<(Idx<Inst>, Idx<Block>, Idx<Block>)> {
        let Term::Branch {
            cond,
            then_bb,
            else_bb,
        } = self.mf.blocks[b].term
        else {
            return None;
        };
        let pure = self.mf.blocks[b].insts.iter().all(|&i| {
            matches!(&self.mf.insts[i], Inst::Load(_)) || always_inlinable(&self.mf.insts[i])
        });
        pure.then_some((cond, then_bb, else_bb))
    }

    /// The JS statement for a `Goto(t)` that is a `continue`/`break` of an
    /// enclosing loop: bare for the innermost, labeled (`continue L<h>;`) for an
    /// outer one (a multi-level jump), recording that header so `finalize_labels`
    /// materializes its `L<h>:`. `None` if `t` is no enclosing loop target.
    pub(super) fn labeled_jump(&mut self, t: Idx<Block>) -> Option<String> {
        let innermost = self.loop_stack.last().map(|f| f.header);
        let hit = self.loop_stack.iter().rev().find_map(|f| {
            (t == f.cont)
                .then_some((f.header, true))
                .or((t == f.brk).then_some((f.header, false)))
        })?;
        let (header, is_cont) = hit;
        let bare = Some(header) == innermost;
        if bare {
            return Some(if is_cont {
                "continue;".to_string()
            } else {
                "break;".to_string()
            });
        }
        self.labels_used.insert(header);
        Some(if is_cont {
            format!("continue L{};", self.mf.emit_block_id(header))
        } else {
            format!("break L{};", self.mf.emit_block_id(header))
        })
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
            skull_mir::lower::RepLayout::Opaque | skull_mir::lower::RepLayout::Foreign { .. } => {
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
