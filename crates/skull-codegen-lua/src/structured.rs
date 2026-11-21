//! Recognized loops use a Lua `for` or `while`. Other loops keep a backward
//! goto in the label's scope so LuaJIT emits LOOP.

use super::*;
use skull_mir::cfg::structure::{Placement, place};

#[inline]
fn bx(b: Idx<Block>) -> usize {
    raw(b) as usize
}

const NONE: u32 = u32::MAX;

pub(crate) struct Shape {
    placement: Placement,
    /// Block -> the block an edge into it reaches once empty forwarding
    /// blocks are skipped.
    forward: Vec<u32>,
    /// Counted-loop header -> its latch, which the `for` line subsumes.
    latch_of: Vec<u32>,
}

impl Shape {
    /// `None` when the body cannot be structured; the caller prints it flat.
    /// Only blocks in `rpo` affect placement: a hoisted lambda's arena holds
    /// every block of its enclosing function.
    pub(crate) fn analyze(
        func: &MirFunc,
        entry: Idx<Block>,
        rpo: &[Idx<Block>],
        counted: &HashMap<Idx<Block>, ForLoop>,
        forwarder: impl Fn(Idx<Block>) -> bool,
        raises: impl Fn(Idx<Block>) -> bool,
    ) -> Option<Shape> {
        let n = func.blocks.len();
        let forward =
            placement::thread_forwarders(func, rpo, |b| !counted.contains_key(&b) && forwarder(b));
        if rpo
            .iter()
            .any(|&b| matches!(func.blocks[b].term, Term::Yield { .. }))
        {
            return None;
        }
        let succ = placement::threaded_successors(func, &forward);
        let mut latch_of = vec![NONE; n];
        let mut unplaced: Vec<Idx<Block>> = Vec::with_capacity(counted.len());
        for (&h, cl) in counted {
            latch_of[bx(h)] = raw(cl.latch);
            if cl.latch != h {
                unplaced.push(cl.latch);
            }
        }
        let raising: Vec<Idx<Block>> = func
            .blocks
            .iter()
            .map(|(b, _)| b)
            .filter(|&b| raises(b))
            .collect();
        let placement = place(entry, &succ, &unplaced, &raising)?;
        // The `for` subsumes header and latch, so its natural loop must lie in
        // the recognized region (threaded forwarders may drop out of it): it
        // holds header, latch and nothing outside the region.
        for (&h, cl) in counted {
            let mut latches = placement.latches(h);
            if cl.latch != h && (latches.next() != Some(cl.latch) || latches.next().is_some()) {
                return None;
            }
            let inside = cl
                .region
                .iter()
                .filter(|&&b| placement.in_loop(h, b))
                .count();
            if placement.loop_size(h) != inside + 1 + usize::from(cl.latch != h) {
                return None;
            }
        }
        Some(Shape {
            placement,
            forward,
            latch_of,
        })
    }

    fn thread(&self, t: Idx<Block>) -> Idx<Block> {
        Idx::from_raw(comfy_la_arena::RawIdx::from_u32(self.forward[bx(t)]))
    }

    fn counted_latch(&self, h: Idx<Block>) -> Option<Idx<Block>> {
        let l = self.latch_of[bx(h)];
        (l != NONE).then(|| Idx::from_raw(comfy_la_arena::RawIdx::from_u32(l)))
    }
}

use skull_mir::cfg::placement::{self, BlockFlow, NativeLoop, PlacementEmit};
use std::convert::Infallible;

struct Printer<'w, 'e> {
    em: &'w mut Emitter<'e>,
    sh: &'w Shape,
    out: &'w mut String,
    breaks: Vec<u32>,
    escapes: Vec<u32>,
}

struct Ladder {
    first: bool,
    pending: Option<(u32, u8)>,
    escapes: usize,
}

struct LoopLines {
    head: Option<u32>,
    breaks: usize,
}

impl Printer<'_, '_> {
    fn while_loop(&self, header: Idx<Block>) -> Option<NativeLoop> {
        if !self.sh.placement.is_header(header) {
            return None;
        }
        let Term::Branch {
            cond,
            then_bb,
            else_bb,
        } = self.em.func.blocks[header].term
        else {
            return None;
        };
        let mut body = self.sh.thread(then_bb);
        let mut exit = self.sh.thread(else_bb);
        let placement = &self.sh.placement;
        if placement.in_loop(header, exit) {
            std::mem::swap(&mut body, &mut exit);
        }
        let follower = placement
            .followers(header)
            .iter()
            .copied()
            .find(|&block| !placement.in_loop(header, block));
        (body != header
            && placement.in_loop(header, body)
            && !placement.is_follower(body)
            && follower == Some(exit)
            && !matches!(self.em.func.insts[cond], Inst::Const(_))
            && self.chainable(header))
        .then_some(NativeLoop {
            latch: header,
            body,
            exit,
        })
    }

    fn line(&mut self, s: &str, instructions: u8) -> u32 {
        let at = self.em.control.line(self.out.len(), instructions);
        self.out.push(' ');
        self.out.push_str(s);
        self.out.push('\n');
        at
    }

    fn jump(&mut self, from: (u32, u8), to: (u32, u8)) {
        self.em.control.jump(from, to);
    }

    fn rung(&mut self, lad: &mut Ladder, cond: Idx<Inst>) {
        let escape = u8::from(!lad.first);
        let test = if matches!(self.em.func.insts[cond], Inst::Const(_)) {
            0
        } else {
            2
        };
        let at = self.em.control.line(self.out.len(), escape + test);
        if !lad.first {
            self.escapes.push(at);
        }
        if let Some(p) = lad.pending.take() {
            self.jump(p, (at, escape));
        }
        lad.first = false;
        lad.pending = (test > 0).then_some((at, escape + 1));
    }

    fn ladder_else(&mut self, lad: &mut Ladder) {
        let at = self.line("else", 1);
        self.escapes.push(at);
        if let Some(p) = lad.pending.take() {
            self.jump(p, (at, 1));
        }
    }

    fn ladder_end(&mut self, lad: Ladder) {
        let at = self.line("end", 0);
        if let Some(p) = lad.pending {
            self.jump(p, (at, 0));
        }
        for i in lad.escapes..self.escapes.len() {
            let from = self.escapes[i];
            self.jump((from, 0), (at, 0));
        }
        self.escapes.truncate(lad.escapes);
    }
}

impl PlacementEmit for Printer<'_, '_> {
    type Error = Infallible;
    type Ladder = Ladder;
    type Loop = LoopLines;

    fn thread(&self, block: Idx<Block>) -> Idx<Block> {
        self.sh.thread(block)
    }

    fn native_loop(&self, header: Idx<Block>) -> Option<NativeLoop> {
        let Some(latch) = self.sh.counted_latch(header) else {
            return self.while_loop(header);
        };
        let lp = &self.em.loops[&header];
        Some(NativeLoop {
            latch,
            body: lp.body_entry,
            exit: lp.exit,
        })
    }

    fn block(&mut self, block: Idx<Block>) -> Result<BlockFlow, Infallible> {
        if self.em.emit_block_statements(self.out, block) {
            return Ok(BlockFlow::Done);
        }
        if let Term::Branch {
            cond,
            then_bb,
            else_bb,
        } = self.em.func.blocks[block].term
            && let Some((target, value)) = self.em.selection(block, cond, then_bb, else_bb)
        {
            self.em.mark_inline_operand(self.out, cond);
            self.em.write_selection(self.out, block, target, value);
            return Ok(match target {
                SelectionTarget::Cell { join, .. } => BlockFlow::Next(join),
                SelectionTarget::Return => BlockFlow::Done,
            });
        }
        Ok(BlockFlow::Terminator)
    }

    fn terminal(&mut self, block: Idx<Block>, term: &Term) -> Result<(), Infallible> {
        self.em.emit_term(self.out, term, block);
        Ok(())
    }

    fn chainable(&self, block: Idx<Block>) -> bool {
        if self.em.tail_forwards[bx(block)].is_some()
            || !self.em.kills.at_entry(block).is_empty()
            || self.em.func.blocks[block]
                .insts
                .iter()
                .any(|&id| self.em.emits_statement(id) || self.em.suppressed[raw(id) as usize])
        {
            return false;
        }
        match self.em.func.blocks[block].term {
            Term::Branch {
                cond,
                then_bb,
                else_bb,
            } => self.em.selection(block, cond, then_bb, else_bb).is_none(),
            _ => false,
        }
    }

    fn condition_effect(&self, cond: Idx<Inst>) -> bool {
        self.em.folds_effect(cond)
    }

    fn ladder(&mut self) -> Ladder {
        Ladder {
            first: true,
            pending: None,
            escapes: self.escapes.len(),
        }
    }

    fn condition(
        &mut self,
        ladder: &mut Ladder,
        cond: Idx<Inst>,
        negate: bool,
    ) -> Result<(), Infallible> {
        self.em.mark_inline_operand(self.out, cond);
        let keyword = if ladder.first { "if" } else { "elseif" };
        self.rung(ladder, cond);
        self.out.push(' ');
        self.out.push_str(keyword);
        self.out.push_str(if negate { " not " } else { " " });
        self.em.write_opnd(self.out, cond);
        self.out.push_str(" then\n");
        Ok(())
    }

    fn otherwise(&mut self, ladder: &mut Ladder) -> Result<(), Infallible> {
        self.ladder_else(ladder);
        Ok(())
    }

    fn end_ladder(&mut self, ladder: Ladder) -> Result<(), Infallible> {
        self.ladder_end(ladder);
        Ok(())
    }

    fn open_follower(&mut self, _block: Idx<Block>) -> Result<(), Infallible> {
        Ok(())
    }

    fn close_follower(&mut self, block: Idx<Block>, referenced: bool) -> Result<(), Infallible> {
        if referenced {
            self.em.write_label(self.out, Label::Follower, block);
        }
        Ok(())
    }

    fn follower_jump(&mut self, block: Idx<Block>) -> Result<(), Infallible> {
        self.em.write_goto(self.out, Label::Follower, block);
        Ok(())
    }

    fn begin_loop(
        &mut self,
        header: Idx<Block>,
        native: Option<NativeLoop>,
    ) -> Result<LoopLines, Infallible> {
        let head = if self.sh.counted_latch(header).is_some() {
            self.em
                .write_kills(self.out, self.em.kills.at_entry(header));
            let at = self.em.control.line(self.out.len(), 1);
            self.em.write_for_head(self.out, header);
            Some(at)
        } else if let Some(lp) = native {
            let Term::Branch { cond, then_bb, .. } = self.em.func.blocks[header].term else {
                unreachable!("while loop header must branch");
            };
            self.em.mark_inline_operand(self.out, cond);
            let at = self.em.control.line(self.out.len(), 3);
            self.out.push_str(" while ");
            if self.sh.thread(then_bb) != lp.body {
                self.out.push_str("not ");
            }
            self.em.write_opnd(self.out, cond);
            self.out.push_str(" do\n");
            Some(at)
        } else {
            self.em.write_label(self.out, Label::Header, header);
            None
        };
        Ok(LoopLines {
            head,
            breaks: self.breaks.len(),
        })
    }

    fn end_loop(
        &mut self,
        header: Idx<Block>,
        native: Option<NativeLoop>,
        token: LoopLines,
        reaches_end: bool,
        continued: bool,
    ) -> Result<(), Infallible> {
        let end = if let Some(lp) = native {
            if continued {
                self.em.write_label(self.out, Label::Continue, header);
            }
            if lp.latch != header {
                self.em.emit_for_latch(self.out, header, lp.latch);
            }
            let end = self.line("end", 1);
            let head = token.head.expect("native loop header");
            if self.sh.counted_latch(header).is_some() {
                scope_guard::close(self.em.func, header);
                self.jump((head, 0), (end, 1));
                self.jump((end, 0), (head, 1));
            } else {
                self.jump((head, 1), (end, 1));
                self.jump((end, 0), (head, 0));
            }
            end
        } else {
            if continued {
                self.em.write_label(self.out, Label::Continue, header);
            }
            if reaches_end || continued {
                self.em.write_goto(self.out, Label::Header, header);
            }
            0
        };
        for i in token.breaks..self.breaks.len() {
            self.jump((self.breaks[i], 0), (end, 1));
        }
        self.breaks.truncate(token.breaks);
        Ok(())
    }

    fn continue_loop(
        &mut self,
        header: Idx<Block>,
        _native: Option<NativeLoop>,
        _innermost: bool,
    ) -> Result<(), Infallible> {
        self.em.write_goto(self.out, Label::Continue, header);
        Ok(())
    }

    fn break_loop(&mut self, _header: Idx<Block>, innermost: bool) -> Result<bool, Infallible> {
        if !innermost {
            return Ok(false);
        }
        let at = self.line("break", 1);
        self.em.control.names += 1;
        self.breaks.push(at);
        Ok(true)
    }
}

pub(crate) fn emit_body(em: &mut Emitter<'_>, shape: &Shape, entry: Idx<Block>, out: &mut String) {
    let func = em.func;
    let mut printer = Printer {
        em,
        sh: shape,
        out,
        breaks: Vec::new(),
        escapes: Vec::new(),
    };
    placement::emit(func, &shape.placement, entry, &mut printer)
        .unwrap_or_else(|error| match error {});
}
