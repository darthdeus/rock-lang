//! The relooper's structured walk over one GLSL function.

use super::*;

impl StructuredEmit for FnEmit<'_> {
    const BREAK_ON_EXIT_ENTRY: bool = false;
    const STRICT_EMPTY_FORWARD: bool = false;

    fn mir(&self) -> &MirFunc {
        self.f
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

    fn block_has_statement(&self, b: Idx<Block>) -> bool {
        self.f.blocks[b].insts.iter().any(|&i| {
            matches!(self.f.insts[i], Inst::Store { .. }) || self.value_name.contains_key(&i)
        })
    }

    fn on_revisit(
        &mut self,
        _b: Idx<Block>,
        _stop: Option<Idx<Block>>,
        _ctx: LoopCtx,
    ) -> Result<(), CompilerError> {
        Err(err(
            self.span(),
            "shader profile: unstructured control flow (unsupported loop/branch shape)",
        ))
    }

    fn unsupported_term(&self, term: &Term) -> CompilerError {
        if matches!(term, Term::Yield { .. }) {
            err(
                self.span(),
                "shader profile: `yield` (coroutine) is not tier 0",
            )
        } else {
            err(self.span(), "shader profile: `match` is not tier 0")
        }
    }

    fn branch_shortcut(
        &mut self,
        b: Idx<Block>,
        cond: Idx<Inst>,
        else_bb: Idx<Block>,
    ) -> Option<Option<Idx<Block>>> {
        // Direct shader calls materialize scalar literal defaults at the call
        // site. Other MIR defaults still need their nil path, which GLSL
        // cannot represent and must reject.
        let materialized_default = self
            .f
            .param_default_guards
            .get(&cond)
            .is_some_and(|index| self.f.param_default_materialized.contains(index));
        if materialized_default {
            return Some(Some(else_bb));
        }
        // A folded short-circuit / ternary diamond emits nothing here -- its
        // expression renders at the result cell's one use. Skip to the join
        // (ipdom); both arms are absorbed.
        self.folded_block
            .contains_key(&b)
            .then(|| self.structure.ipdom(b))
    }

    /// The loop's condition test: the `Branch` in the body with a direct
    /// successor to the exit.
    fn loop_test(&mut self, region: &LoopRegion) -> Result<Option<LoopTest>, CompilerError> {
        let test = region
            .body
            .iter()
            .find_map(|&b| LoopTest::with_exit_arm(self.f, b, region.exit));
        if test.is_some() {
            return Ok(test);
        }
        Err(err(
            self.span(),
            "shader profile: unstructured loop (no single condition test)",
        ))
    }

    fn counted_slot_bound(&mut self, li: &CountedLoop) -> Result<(String, String), CompilerError> {
        Ok((self.cell_name[&li.slot].clone(), self.render_val(li.bound)?))
    }
}

// --- public API --------------------------------------------------------------
