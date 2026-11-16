//! The GLSL structured walk: straight-line code, `if` diamonds, range-`for`
//! loops and tested general loops over the [`structure`](super::structure)
//! analysis. The backend implements [`StructuredEmit`] to render statements.

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

impl LoopTest {
    /// The test `block` holds when its terminator is a `Branch` with one arm
    /// directly to `exit`.
    pub fn with_exit_arm(func: &MirFunc, block: Idx<Block>, exit: Idx<Block>) -> Option<Self> {
        let Term::Branch {
            cond,
            then_bb,
            else_bb,
        } = func.blocks[block].term
        else {
            return None;
        };
        let break_when_true = then_bb == exit;
        (break_when_true || else_bb == exit).then_some(Self {
            block,
            cond,
            body: if break_when_true { else_bb } else { then_bb },
            break_when_true,
        })
    }
}

pub trait StructuredEmit {
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
    /// Reject a repeated block visit.
    fn on_revisit(
        &mut self,
        b: Idx<Block>,
        stop: Option<Idx<Block>>,
        ctx: LoopCtx,
    ) -> Result<(), CompilerError>;
    /// The error for a terminator the backend cannot structure.
    fn unsupported_term(&self, term: &Term) -> CompilerError;
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
    /// The condition test of a general loop.
    fn loop_test(&mut self, region: &LoopRegion) -> Result<LoopTest, CompilerError>;
    /// A range-`for`'s slot and bound, rendered.
    fn counted_slot_bound(&mut self, li: &CountedLoop) -> Result<(String, String), CompilerError>;

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
            // Loop headers are dispatched structurally (before the visited
            // guard): `emit_general` re-enters `emit_seq` on the header for its
            // condition region, so the header must not already be marked.
            if !self.is_active(cur) {
                if let Some(li) = self.counted_at(cur) {
                    self.emit_counted(li)?;
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

    /// Emit an `if` diamond and return its merge, or `None` when both arms diverge.
    fn emit_branch(
        &mut self,
        blk: Idx<Block>,
        cond: Idx<Inst>,
        then_bb: Idx<Block>,
        else_bb: Idx<Block>,
        stop: Option<Idx<Block>>,
        ctx: LoopCtx,
    ) -> Result<Option<Idx<Block>>, CompilerError> {
        let cond_s = self.render_cond(cond)?;
        // The merge is ipdom, unless an arm diverges (ipdom collapses to the
        // virtual exit or a loop target) -- then the arms absorb the
        // continuation up to the enclosing stop and there is no fall-through.
        let merge = self
            .structure()
            .ipdom(blk)
            .filter(|&m| Some(m) != ctx.cont && Some(m) != ctx.brk);
        // Empty else -> then-only `if`; continue at the else target.
        if let Some(j) = self.empty_forward(else_bb) {
            self.push(&format!("if ({cond_s}) {{"));
            *self.indent() += 1;
            self.emit_seq(then_bb, Some(j), ctx)?;
            *self.indent() -= 1;
            self.push("}");
            return Ok(Some(j));
        }
        // Empty then -> invert and emit the else arm only.
        if let Some(j) = self.empty_forward(then_bb) {
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
    fn emit_counted(&mut self, li: CountedLoop) -> Result<(), CompilerError> {
        let (slot, bound) = self.counted_slot_bound(&li)?;
        let cmp = if li.inclusive { "<=" } else { "<" };
        self.push(&format!(
            "for (; {slot} {cmp} {bound}; {slot} = {slot} + 1) {{"
        ));
        *self.indent() += 1;
        let ctx = LoopCtx {
            cont: Some(li.latch),
            brk: Some(li.exit),
        };
        let body_len = self.out().len();
        self.emit_seq(li.body_entry, None, ctx)?;
        self.trim_trailing_continue(body_len);
        *self.indent() -= 1;
        self.push("}");
        Ok(())
    }

    /// A general tested loop: `while (true) { <condition region>; if (!c) break;
    /// <body> }`, where the condition region may be a multi-block diamond.
    fn emit_general(&mut self, region: &LoopRegion, outer: LoopCtx) -> Result<(), CompilerError> {
        let h = region.header;
        let exit = region.exit;
        let ctx = LoopCtx {
            cont: Some(h),
            brk: Some(exit),
        };
        let test = self.loop_test(region)?;

        self.set_active(h, true);
        self.push("while (true) {");
        *self.indent() += 1;
        self.emit_seq(h, Some(test.block), outer)?;
        self.block_effects(test.block)?;
        self.mark_visited(test.block);
        let cond_s = self.render_cond(test.cond)?;
        if test.break_when_true {
            self.push(&format!("if ({cond_s}) {{ break; }}"));
        } else {
            self.push(&format!("if (!({cond_s})) {{ break; }}"));
        }
        let body_len = self.out().len();
        self.emit_seq(test.body, None, ctx)?;
        self.trim_trailing_continue(body_len);

        *self.indent() -= 1;
        self.push("}");
        self.set_active(h, false);
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
