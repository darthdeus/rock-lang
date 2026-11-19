//! Placement-based structuring for JavaScript bodies.

use super::*;
use skull_mir::cfg::placement::{self as walk, BlockFlow, NativeLoop, PlacementEmit};
use skull_mir::cfg::structure::{self as cfg_structure, CountedBound, CountedLoop, Placement};

const NESTING_COST_ELSE_IF: u32 = 2;
const NESTING_COST_BRANCH: u32 = 5;
const NESTING_COST_LOOP: u32 = 6;
const MAX_NESTING: u32 = 7200;

type CountedLoops = rustc_hash::FxHashMap<Idx<Block>, CountedLoop>;

pub(super) struct Shape {
    pub(super) placement: Placement,
    forward: Vec<u32>,
    counted: CountedLoops,
}

impl Shape {
    pub(super) fn analyze(em: &Emitter<'_>, entry: Idx<Block>) -> Option<Self> {
        let func = em.mf;
        let rpo = skull_mir::cfg::reverse_postorder(func, entry);
        let mut counted = CountedLoops::default();
        for &header in &rpo {
            if let Some(loop_info) =
                cfg_structure::counted_loop(func, header, CountedBound::InlineOnly)
            {
                counted.insert(header, loop_info);
            }
        }
        let forward = walk::thread_forwarders(func, &rpo, |b| {
            !counted.contains_key(&b) && !em.block_has_statement(b)
        });

        let succ = walk::threaded_successors(func, &forward);

        let mut unplaced = Vec::with_capacity(counted.len());
        for (&header, loop_info) in &counted {
            if loop_info.latch != header {
                unplaced.push(loop_info.latch);
            }
        }
        let raising: Vec<_> = rpo.iter().copied().filter(|&b| em.raises(b)).collect();
        let placement = cfg_structure::place(entry, &succ, &unplaced, &raising)?;

        for (&header, loop_info) in &counted {
            let mut latches = placement.latches(header);
            if latches.next() != Some(loop_info.latch) || latches.next().is_some() {
                return None;
            }
            let body = Idx::from_raw(RawIdx::from_u32(
                forward[raw(loop_info.body_entry) as usize],
            ));
            let exit = Idx::from_raw(RawIdx::from_u32(forward[raw(loop_info.exit) as usize]));
            if !placement.in_loop(header, body) || placement.in_loop(header, exit) {
                return None;
            }
        }

        Some(Self {
            placement,
            forward,
            counted,
        })
    }

    fn thread(&self, block: Idx<Block>) -> Idx<Block> {
        Idx::from_raw(RawIdx::from_u32(self.forward[raw(block) as usize]))
    }
}

struct Ladder {
    open: bool,
    conditions: u32,
    nesting_cost: u32,
}

struct LoopToken {
    nesting_cost: u32,
}

struct JsPlacement<'a, 'e> {
    em: &'a mut Emitter<'e>,
    shape: &'a Shape,
    nesting: u32,
}

impl JsPlacement<'_, '_> {
    fn enter_nesting(
        &mut self,
        cost: u32,
        block: Option<Idx<Block>>,
        inst: Option<Idx<Inst>>,
    ) -> Result<(), CompilerError> {
        self.nesting += cost;
        if self.nesting <= MAX_NESTING {
            return Ok(());
        }
        let error = if let Some(block) = block {
            self.em.nesting_error(block)
        } else if let Some(inst) = inst {
            self.em.nesting_error_inst(inst)
        } else {
            self.em.nesting_error(self.em.mf.entry)
        };
        Err(error)
    }
}

impl PlacementEmit for JsPlacement<'_, '_> {
    type Error = CompilerError;
    type Ladder = Ladder;
    type Loop = LoopToken;

    fn thread(&self, block: Idx<Block>) -> Idx<Block> {
        self.shape.thread(block)
    }

    fn native_loop(&self, header: Idx<Block>) -> Option<NativeLoop> {
        self.shape.counted.get(&header).map(|loop_info| NativeLoop {
            latch: loop_info.latch,
            body: loop_info.body_entry,
            exit: loop_info.exit,
        })
    }

    fn block(&mut self, block: Idx<Block>) -> Result<BlockFlow, Self::Error> {
        self.em.emit_block_effects(block)?;
        Ok(BlockFlow::Terminator)
    }

    fn terminal(&mut self, _block: Idx<Block>, term: &Term) -> Result<(), Self::Error> {
        match term {
            Term::Return(values) => self.em.emit_return(values),
            Term::Unreachable => {
                self.em.emit_unreachable();
                Ok(())
            }
            Term::Yield { .. } => Err(err("JS subset: `yield` not supported")),
            Term::Goto(_) | Term::Branch { .. } => unreachable!("walk handles block edges"),
        }
    }

    fn chainable(&self, block: Idx<Block>) -> bool {
        !self.em.block_has_statement(block)
    }

    fn condition_effect(&self, _cond: Idx<Inst>) -> bool {
        false
    }

    fn ladder(&mut self) -> Self::Ladder {
        Ladder {
            open: false,
            conditions: 0,
            nesting_cost: 0,
        }
    }

    fn condition(
        &mut self,
        ladder: &mut Self::Ladder,
        cond: Idx<Inst>,
        negate: bool,
    ) -> Result<(), Self::Error> {
        let cost = if ladder.conditions == 0 {
            NESTING_COST_BRANCH
        } else {
            NESTING_COST_ELSE_IF
        };
        self.enter_nesting(cost, None, Some(cond))?;
        ladder.nesting_cost += cost;
        let condition = self.em.render_val(cond)?;
        let condition = if negate {
            format!("!({condition})")
        } else {
            condition
        };
        if ladder.open {
            self.em.indent -= 1;
            self.em.push(&format!("}} else if ({condition}) {{"));
        } else {
            self.em.push(&format!("if ({condition}) {{"));
        }
        self.em.indent += 1;
        ladder.open = true;
        ladder.conditions += 1;
        Ok(())
    }

    fn otherwise(&mut self, ladder: &mut Self::Ladder) -> Result<(), Self::Error> {
        if ladder.open {
            self.em.indent -= 1;
            self.em.push("} else {");
            self.em.indent += 1;
        }
        Ok(())
    }

    fn end_ladder(&mut self, ladder: Self::Ladder) -> Result<(), Self::Error> {
        if ladder.open {
            self.em.indent -= 1;
            self.em.push("}");
        }
        self.nesting -= ladder.nesting_cost;
        Ok(())
    }

    fn open_follower(&mut self, block: Idx<Block>) -> Result<(), Self::Error> {
        self.enter_nesting(NESTING_COST_BRANCH, Some(block), None)?;
        self.em
            .push(&format!("F{}: {{", self.em.mf.emit_block_id(block)));
        self.em.indent += 1;
        Ok(())
    }

    fn close_follower(&mut self, _block: Idx<Block>, _referenced: bool) -> Result<(), Self::Error> {
        self.em.indent -= 1;
        self.em.push("}");
        self.nesting -= NESTING_COST_BRANCH;
        Ok(())
    }

    fn follower_jump(&mut self, block: Idx<Block>) -> Result<(), Self::Error> {
        self.em
            .push(&format!("break F{};", self.em.mf.emit_block_id(block)));
        Ok(())
    }

    fn begin_loop(
        &mut self,
        header: Idx<Block>,
        native: Option<NativeLoop>,
    ) -> Result<Self::Loop, Self::Error> {
        self.enter_nesting(NESTING_COST_LOOP, Some(header), None)?;
        let label = format!("L{}", self.em.mf.emit_block_id(header));
        if native.is_some() {
            let loop_info = self
                .shape
                .counted
                .get(&header)
                .copied()
                .ok_or_else(|| err("JS subset: invalid counted-loop placement"))?;
            let (slot, bound) = self.em.counted_slot_bound(&loop_info)?;
            let cmp = if loop_info.inclusive { "<=" } else { "<" };
            self.em.push(&format!(
                "{label}: for (; {slot} {cmp} {bound}; {slot} = {slot} + 1) {{"
            ));
        } else {
            self.em.push(&format!("{label}: while (true) {{"));
        }
        self.em.indent += 1;
        Ok(LoopToken {
            nesting_cost: NESTING_COST_LOOP,
        })
    }

    fn end_loop(
        &mut self,
        _header: Idx<Block>,
        _native: Option<NativeLoop>,
        token: Self::Loop,
        _reaches_end: bool,
        _continued: bool,
    ) -> Result<(), Self::Error> {
        self.em.indent -= 1;
        self.em.push("}");
        self.nesting -= token.nesting_cost;
        Ok(())
    }

    fn continue_loop(
        &mut self,
        header: Idx<Block>,
        _native: Option<NativeLoop>,
        innermost: bool,
    ) -> Result<(), Self::Error> {
        let statement = if innermost {
            "continue;".to_owned()
        } else {
            format!("continue L{};", self.em.mf.emit_block_id(header))
        };
        self.em.push(&statement);
        Ok(())
    }

    fn break_loop(&mut self, header: Idx<Block>, innermost: bool) -> Result<bool, Self::Error> {
        let statement = if innermost {
            "break;".to_owned()
        } else {
            format!("break L{};", self.em.mf.emit_block_id(header))
        };
        self.em.push(&statement);
        Ok(true)
    }
}

pub(super) fn emit_body(
    em: &mut Emitter<'_>,
    shape: &Shape,
    entry: Idx<Block>,
) -> Result<(), CompilerError> {
    let func = em.mf;
    let placement = &shape.placement;
    let mut adapter = JsPlacement {
        em,
        shape,
        nesting: 0,
    };
    walk::emit(func, placement, entry, &mut adapter)
}
