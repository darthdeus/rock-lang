//! The structured walk the relooper backends (GLSL, JS) share: straight-line
//! code, `if` diamonds, range-`for` loops and general `while (true)` loops over
//! the [`structure`](super::structure) analysis. A backend implements
//! [`StructuredEmit`] to render statements and to decide the shapes it treats
//! differently (folded diamonds, labeled jumps, shared-tail duplication).

use super::structure::{CountedLoop, LoopRegion, Structure};
use crate::mir::{Block, Inst, MirFunc, Term};
use comfy_la_arena::Idx;
use skull_parser::CompilerError;

/// The enclosing loop's jump targets, mirroring the lowering's loop targets:
/// `Goto(cont)` -> `continue`, `Goto(brk)` -> `break`.
#[derive(Clone, Copy, Default)]
pub struct LoopCtx {
    pub cont: Option<Idx<Block>>,
    pub brk: Option<Idx<Block>>,
}

/// One loop being emitted: its `continue` and `break` targets and its header.
#[derive(Clone, Copy)]
pub struct LoopFrame {
    pub cont: Idx<Block>,
    pub brk: Idx<Block>,
    pub header: Idx<Block>,
}

/// A `while cond`/`for` loop's condition test, split out of its header region.
#[derive(Clone, Copy)]
pub struct LoopTest {
    /// The `Branch` block holding the condition.
    pub block: Idx<Block>,
    pub cond: Idx<Inst>,
    /// The arm that stays in the loop.
    pub body: Idx<Block>,
    /// Whether the loop exits when `cond` holds.
    pub break_when_true: bool,
}

/// A backend's side of the structured walk. The provided methods are the walk;
/// the required ones are the backend's state and rendering.
/// What one more level of nested control flow costs the target's parser.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NestingKind {
    /// An `if` whose arms open blocks.
    Branch,
    /// A `for`/`while` loop.
    Loop,
}

pub trait StructuredEmit {
    /// Whether entering the enclosing loop's exit block from a branch arm is
    /// itself a `break` (a `loop {}` whose `break` arm targets the exit).
    const BREAK_ON_EXIT_ENTRY: bool;
    /// Whether an empty `if` arm may forward only to the branch's real merge
    /// (or diverge), never to an intermediate block or the active header.
    const STRICT_EMPTY_FORWARD: bool;

    fn mir(&self) -> &MirFunc;
    fn structure(&self) -> &Structure;
    fn counted_at(&self, b: Idx<Block>) -> Option<CountedLoop>;
    fn general_at(&self, b: Idx<Block>) -> Option<LoopRegion>;
    fn is_active(&self, b: Idx<Block>) -> bool;
    fn set_active(&mut self, b: Idx<Block>, active: bool);
    /// Mark `b` emitted; false if it already was.
    fn mark_visited(&mut self, b: Idx<Block>) -> bool;
    fn out(&mut self) -> &mut String;
    fn indent(&mut self) -> &mut usize;

    fn render_cond(&mut self, cond: Idx<Inst>) -> Result<String, CompilerError>;
    /// Emit `b`'s own statements.
    fn block_effects(&mut self, b: Idx<Block>) -> Result<(), CompilerError>;
    fn return_values(&mut self, vs: &[Idx<Inst>]) -> Result<(), CompilerError>;
    fn unreachable(&mut self) {}
    /// Whether `b` emits any statement of its own.
    fn block_has_statement(&self, b: Idx<Block>) -> bool;
    /// `b` was reached a second time outside a loop: duplicate it or reject.
    fn on_revisit(
        &mut self,
        b: Idx<Block>,
        stop: Option<Idx<Block>>,
        ctx: LoopCtx,
    ) -> Result<(), CompilerError>;
    /// The error for a terminator the backend cannot structure.
    fn unsupported_term(&self, term: &Term) -> CompilerError;
    /// The statement for a `Goto(t)` to a non-innermost loop's target.
    fn outer_jump(&mut self, _t: Idx<Block>) -> Option<String> {
        None
    }
    /// A branch the backend consumes without an `if`: `Some(next)` continues
    /// at `next` (`None` = no fall-through).
    fn branch_shortcut(
        &mut self,
        _b: Idx<Block>,
        _cond: Idx<Inst>,
        _else_bb: Idx<Block>,
    ) -> Option<Option<Idx<Block>>> {
        None
    }
    /// Fold a chain of nested branches into one condition, returning the
    /// condition text and the arms that remain.
    fn fold_branch(
        &mut self,
        cond: String,
        then_bb: Idx<Block>,
        else_bb: Idx<Block>,
    ) -> Result<(String, Idx<Block>, Idx<Block>), CompilerError> {
        Ok((cond, then_bb, else_bb))
    }
    /// The condition test of a general loop; `None` for a test-less `loop {}`.
    fn loop_test(&mut self, region: &LoopRegion) -> Result<Option<LoopTest>, CompilerError>;
    /// A range-`for`'s slot and bound, rendered.
    fn counted_slot_bound(&mut self, li: &CountedLoop) -> Result<(String, String), CompilerError>;
    /// The prefix a loop header line carries (a label marker).
    fn loop_label(&mut self, _header: Idx<Block>) -> String {
        String::new()
    }
    /// Whether the single `if` that `b` emits may be written directly after
    /// `else` with no block of its own: `b` has no statement and no other
    /// reason for the `if` to carry a preamble.
    fn chains_after_else(&self, _b: Idx<Block>) -> bool {
        false
    }
    /// The next [`Self::enter_nesting`] is a link of an `else if` chain.
    fn mark_next_branch_chained(&mut self) {}
    /// Account for one more emitted control-flow level, or reject the body.
    fn enter_nesting(&mut self, _b: Idx<Block>, _kind: NestingKind) -> Result<(), CompilerError> {
        Ok(())
    }
    fn exit_nesting(&mut self) {}

    fn enter_loop(&mut self, _frame: LoopFrame) {}
    fn exit_loop(&mut self) {}

    fn push(&mut self, line: &str) {
        let indent = *self.indent();
        let out = self.out();
        for _ in 0..indent {
            out.push_str("  ");
        }
        out.push_str(line);
        out.push('\n');
    }

    /// Emit the blocks from `start`, stopping before `stop` (its continuation).
    /// `ctx` carries the enclosing loop's break/continue targets.
    fn emit_seq(
        &mut self,
        start: Idx<Block>,
        stop: Option<Idx<Block>>,
        ctx: LoopCtx,
    ) -> Result<(), CompilerError> {
        let mut cur = start;
        loop {
            if Some(cur) == stop {
                return Ok(());
            }
            // Checked before the visited mark so the exit stays available for
            // the post-loop emission the header's dispatcher resumes at.
            if Self::BREAK_ON_EXIT_ENTRY && Some(cur) == ctx.brk {
                self.push("break;");
                return Ok(());
            }
            // Loop headers are dispatched structurally (before the visited
            // guard): `emit_general` re-enters `emit_seq` on the header for its
            // condition region, so the header must not already be marked.
            if !self.is_active(cur) {
                if let Some(li) = self.counted_at(cur) {
                    self.emit_counted(cur, li)?;
                    cur = li.exit;
                    continue;
                }
                if let Some(region) = self.general_at(cur) {
                    self.emit_general(&region, ctx)?;
                    cur = region.exit;
                    continue;
                }
            }
            if !self.mark_visited(cur) {
                return self.on_revisit(cur, stop, ctx);
            }
            self.block_effects(cur)?;
            match self.mir().blocks[cur].term.clone() {
                Term::Return(vs) => {
                    self.return_values(&vs)?;
                    return Ok(());
                }
                Term::Unreachable => {
                    self.unreachable();
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
                    if let Some(stmt) = self.outer_jump(t) {
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
                    let next = match self.branch_shortcut(cur, cond, else_bb) {
                        Some(next) => next,
                        None => self.emit_branch(cur, cond, then_bb, else_bb, stop, ctx)?,
                    };
                    match next {
                        Some(m) => cur = m,
                        None => return Ok(()),
                    }
                }
                term @ (Term::Switch { .. } | Term::Yield { .. }) => {
                    return Err(self.unsupported_term(&term));
                }
            }
        }
    }

    /// Whether `b` emits no statement and just forwards to another block
    /// (`Some(target)`): an empty `if` arm, or a bare `break`/`continue` block.
    fn empty_forward(&self, b: Idx<Block>) -> Option<Idx<Block>> {
        match self.mir().blocks[b].term {
            Term::Goto(t) if !self.block_has_statement(b) => Some(t),
            _ => None,
        }
    }

    /// Emit an `if` diamond for a `Branch`. Returns the block to continue at
    /// (the merge), or `None` when both arms diverge and have absorbed the
    /// continuation.
    fn emit_branch(
        &mut self,
        blk: Idx<Block>,
        cond: Idx<Inst>,
        then_bb: Idx<Block>,
        else_bb: Idx<Block>,
        stop: Option<Idx<Block>>,
        ctx: LoopCtx,
    ) -> Result<Option<Idx<Block>>, CompilerError> {
        self.enter_nesting(blk, NestingKind::Branch)?;
        let next = self.emit_branch_arms(blk, cond, then_bb, else_bb, stop, ctx);
        self.exit_nesting();
        next
    }

    /// Whether the else arm `b` is nothing but an `if` that can follow `else`
    /// directly: the arm must emit no statement of its own and every path
    /// through that `if` must end at the same place the arm would.
    fn else_arm_chains(&self, b: Idx<Block>, arm_stop: Option<Idx<Block>>, ctx: LoopCtx) -> bool {
        // The arm can open on empty forwarding blocks, which emit nothing.
        let mut b = b;
        loop {
            if Some(b) == arm_stop || Some(b) == ctx.brk || Some(b) == ctx.cont {
                return false;
            }
            match self.empty_forward(b) {
                Some(next) if !self.is_active(next) => b = next,
                _ => break,
            }
        }
        if !self.chains_after_else(b) {
            return false;
        }
        let Term::Branch {
            then_bb, else_bb, ..
        } = self.mir().blocks[b].term
        else {
            return false;
        };
        let merge = self
            .structure()
            .ipdom(b)
            .filter(|&m| Some(m) != ctx.cont && Some(m) != ctx.brk);
        match merge {
            Some(m) => self.forwards_to(m, arm_stop, ctx),
            None => self.empty_forward(then_bb).is_none() && self.empty_forward(else_bb).is_none(),
        }
    }

    /// Whether control entering `m` reaches `target` through only empty
    /// forwarding blocks, so nothing is emitted on the way there.
    fn forwards_to(&self, mut m: Idx<Block>, target: Option<Idx<Block>>, ctx: LoopCtx) -> bool {
        loop {
            if Some(m) == target {
                return true;
            }
            match self.empty_forward(m) {
                Some(next) if Some(next) != ctx.cont && Some(next) != ctx.brk => m = next,
                _ => return false,
            }
        }
    }

    fn emit_branch_arms(
        &mut self,
        blk: Idx<Block>,
        cond: Idx<Inst>,
        then_bb: Idx<Block>,
        else_bb: Idx<Block>,
        stop: Option<Idx<Block>>,
        ctx: LoopCtx,
    ) -> Result<Option<Idx<Block>>, CompilerError> {
        let cond_s = self.render_cond(cond)?;
        let (cond_s, then_bb, else_bb) = self.fold_branch(cond_s, then_bb, else_bb)?;
        // The merge is ipdom, unless an arm diverges (ipdom collapses to the
        // virtual exit or a loop target) -- then the arms absorb the
        // continuation up to the enclosing stop and there is no fall-through.
        let merge = self
            .structure()
            .ipdom(blk)
            .filter(|&m| Some(m) != ctx.cont && Some(m) != ctx.brk);
        // An empty arm lets the live arm stop at the empty arm's target `j`
        // and resume there. Under the strict rule that is sound only when both
        // arms rejoin at `j` or both diverge, and never at the active header.
        let forward_ok = |s: &Self, j: Idx<Block>| {
            !Self::STRICT_EMPTY_FORWARD
                || ((merge == Some(j) || merge.is_none()) && !s.is_active(j))
        };
        // Empty else -> then-only `if`; continue at the else target.
        if let Some(j) = self.empty_forward(else_bb).filter(|&j| forward_ok(self, j)) {
            self.push(&format!("if ({cond_s}) {{"));
            *self.indent() += 1;
            self.emit_seq(then_bb, Some(j), ctx)?;
            *self.indent() -= 1;
            self.push("}");
            return Ok(Some(j));
        }
        // Empty then -> invert and emit the else arm only.
        if let Some(j) = self.empty_forward(then_bb).filter(|&j| forward_ok(self, j)) {
            self.push(&format!("if (!({cond_s})) {{"));
            *self.indent() += 1;
            self.emit_seq(else_bb, Some(j), ctx)?;
            *self.indent() -= 1;
            self.push("}");
            return Ok(Some(j));
        }
        let arm_stop = merge.or(stop);
        self.push(&format!("if ({cond_s}) {{"));
        *self.indent() += 1;
        self.emit_seq(then_bb, arm_stop, ctx)?;
        *self.indent() -= 1;
        // No block and no indent for an `else if`: nesting costs parse stack on
        // both sides of the emitted text, and indenting by chain length makes
        // the text quadratic in the number of links.
        if self.else_arm_chains(else_bb, arm_stop, ctx) {
            self.push("} else");
            self.mark_next_branch_chained();
            self.emit_seq(else_bb, arm_stop, ctx)?;
            return Ok(merge);
        }
        self.push("} else {");
        *self.indent() += 1;
        self.emit_seq(else_bb, arm_stop, ctx)?;
        *self.indent() -= 1;
        self.push("}");
        Ok(merge)
    }

    /// A range-`for`: `for (; slot < bound; slot = slot + 1) { body }`. The
    /// slot and bound were initialized in the preheader, so the init clause is
    /// empty. `continue` targets the latch (the increment runs it); `break`
    /// targets the exit.
    fn emit_counted(&mut self, header: Idx<Block>, li: CountedLoop) -> Result<(), CompilerError> {
        self.enter_nesting(header, NestingKind::Loop)?;
        let (slot, bound) = self.counted_slot_bound(&li)?;
        let cmp = if li.inclusive { "<=" } else { "<" };
        let label = self.loop_label(header);
        self.push(&format!(
            "{label}for (; {slot} {cmp} {bound}; {slot} = {slot} + 1) {{"
        ));
        *self.indent() += 1;
        let ctx = LoopCtx {
            cont: Some(li.latch),
            brk: Some(li.exit),
        };
        self.enter_loop(LoopFrame {
            cont: li.latch,
            brk: li.exit,
            header,
        });
        let body_len = self.out().len();
        self.emit_seq(li.body_entry, None, ctx)?;
        self.trim_trailing_continue(body_len);
        self.exit_loop();
        *self.indent() -= 1;
        self.push("}");
        self.exit_nesting();
        Ok(())
    }

    /// A general loop. With a condition test (`while cond`/`for`): `while
    /// (true) { <cond region>; if (!(cond)) break; <body> }`, where the
    /// condition region may be a multi-block diamond (`while a && b`). Without
    /// one (`loop {}`): `while (true) { <body> }`, whose exits are the body's
    /// own `break`s.
    fn emit_general(&mut self, region: &LoopRegion, outer: LoopCtx) -> Result<(), CompilerError> {
        let h = region.header;
        let exit = region.exit;
        let ctx = LoopCtx {
            cont: Some(h),
            brk: Some(exit),
        };
        let test = self.loop_test(region)?;

        self.enter_nesting(h, NestingKind::Loop)?;
        self.set_active(h, true);
        let label = self.loop_label(h);
        self.push(&format!("{label}while (true) {{"));
        *self.indent() += 1;
        self.enter_loop(LoopFrame {
            cont: h,
            brk: exit,
            header: h,
        });

        let body_start = match test {
            Some(test) => {
                self.emit_seq(h, Some(test.block), outer)?;
                self.block_effects(test.block)?;
                self.mark_visited(test.block);
                let cond_s = self.render_cond(test.cond)?;
                if test.break_when_true {
                    self.push(&format!("if ({cond_s}) {{ break; }}"));
                } else {
                    self.push(&format!("if (!({cond_s})) {{ break; }}"));
                }
                test.body
            }
            None => h,
        };
        let body_len = self.out().len();
        self.emit_seq(body_start, None, ctx)?;
        self.trim_trailing_continue(body_len);

        self.exit_loop();
        *self.indent() -= 1;
        self.push("}");
        self.set_active(h, false);
        self.exit_nesting();
        Ok(())
    }

    /// Drop a redundant trailing `continue;` that is the loop body's natural
    /// back-edge fall-through (indented exactly at the body level).
    fn trim_trailing_continue(&mut self, start: usize) {
        let line = format!("{}continue;\n", "  ".repeat(*self.indent()));
        let out = self.out();
        if out.len() >= start + line.len() && out.ends_with(&line) {
            let cut = out.len() - line.len();
            if cut >= start && (cut == 0 || out.as_bytes()[cut - 1] == b'\n') {
                out.truncate(cut);
            }
        }
    }
}
