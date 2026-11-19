use super::structure::{CountedLoop, LoopRegion, Structure};
use crate::mir::{Block, Inst, MirFunc, Term};
use comfy_la_arena::Idx;
use skull_parser::CompilerError;

/// The enclosing loop's jump targets: `Goto(cont)` -> `continue`, `Goto(brk)`
/// -> `break`.
#[derive(Clone, Copy, Default)]
pub struct LoopCtx {
    pub cont: Option<Idx<Block>>,
    pub brk: Option<Idx<Block>>,
}

#[derive(Clone, Copy)]
pub struct LoopTest {
    pub block: Idx<Block>,
    pub cond: Idx<Inst>,
    /// The arm that stays in the loop.
    pub body: Idx<Block>,
    pub break_when_true: bool,
}

impl LoopTest {
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
    /// False if `b` was already emitted.
    fn mark_visited(&mut self, b: Idx<Block>) -> bool;
    fn out(&mut self) -> &mut String;
    fn indent(&mut self) -> &mut usize;

    fn render_cond(&mut self, cond: Idx<Inst>) -> Result<String, CompilerError>;
    fn block_effects(&mut self, b: Idx<Block>) -> Result<(), CompilerError>;
    fn return_values(&mut self, vs: &[Idx<Inst>]) -> Result<(), CompilerError>;
    fn unreachable(&mut self) {}
    fn block_has_statement(&self, b: Idx<Block>) -> bool;
    fn on_revisit(
        &mut self,
        b: Idx<Block>,
        stop: Option<Idx<Block>>,
        ctx: LoopCtx,
    ) -> Result<(), CompilerError>;
    fn unsupported_term(&self, term: &Term) -> CompilerError;
    /// A branch consumed without an `if`: `Some(next)` continues at `next`, `None`
    /// has no fall-through.
    fn branch_shortcut(
        &mut self,
        _b: Idx<Block>,
        _cond: Idx<Inst>,
        _else_bb: Idx<Block>,
    ) -> Option<Option<Idx<Block>>> {
        None
    }
    fn loop_test(&mut self, region: &LoopRegion) -> Result<LoopTest, CompilerError>;
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

    /// Stops before `stop`, its continuation.
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
            // Loop headers dispatch before the visited guard: `emit_general` re-enters
            // `emit_seq` on the header for its condition region.
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
                term @ Term::Yield { .. } => {
                    return Err(self.unsupported_term(&term));
                }
            }
        }
    }

    fn empty_forward(&self, b: Idx<Block>) -> Option<Idx<Block>> {
        match self.mir().blocks[b].term {
            Term::Goto(t) if !self.block_has_statement(b) => Some(t),
            _ => None,
        }
    }

    /// Returns the merge, or `None` when both arms diverge.
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
        // When an arm diverges, ipdom collapses to the virtual exit or a loop target;
        // the arms then absorb the continuation and there is no fall-through.
        let merge = self
            .structure()
            .ipdom(blk)
            .filter(|&m| Some(m) != ctx.cont && Some(m) != ctx.brk);
        if let Some(j) = self.empty_forward(else_bb) {
            self.push(&format!("if ({cond_s}) {{"));
            *self.indent() += 1;
            self.emit_seq(then_bb, Some(j), ctx)?;
            *self.indent() -= 1;
            self.push("}");
            return Ok(Some(j));
        }
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

    /// The slot and bound are initialized in the preheader, so the init clause is
    /// empty. `continue` targets the latch; `break` the exit.
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

    /// `while (true) { <condition region>; if (!c) break; <body> }`; the condition
    /// region may be a multi-block diamond.
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

    /// Only a `continue;` indented exactly at the body level is the natural
    /// back-edge fall-through.
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
