//! Per-function preparation: names, cell types and folded diamonds.

use super::*;

impl FnEmit<'_> {
    /// Assign unique GLSL identifiers to cells and retained vector calls.
    ///
    /// In entry mode the hoisted locals share the function scope with the
    /// program's bare globals, so a local named like a uniform lane (or an
    /// attribute, varying, sampler, array member or output) would shadow it
    /// with no diagnostic. A local named like a GLSL builtin callable (`cos`,
    /// `mix`, `float`, ...) breaks the builtin call the same way, in EVERY fn
    /// -- helpers emit with an empty interface reserved set. Global-reference
    /// cells (sampler/array params) claim their exact spellings first -- they
    /// ARE the global -- and every other cell dedups apart from the reserved
    /// globals, lexical keywords, builtin callables, and its siblings.
    pub(crate) fn assign_names(&mut self) -> Result<(), CompilerError> {
        let mut used: HashSet<String> = HashSet::default();
        let claim = |used: &mut HashSet<String>, base: String| -> String {
            let mut name = base.clone();
            let mut k = 0;
            while used.contains(&name) {
                k += 1;
                name = format!("{base}_{k}");
            }
            used.insert(name.clone());
            name
        };
        // Global references first, in cell order (deterministic): a sampler
        // keeps `t` while a same-named local becomes `t_1`, never the reverse.
        let mut refs: Vec<Idx<Cell>> = self.global_ref_cells.iter().copied().collect();
        refs.sort_by_key(|c| c.into_raw().into_u32());
        for cid in refs {
            let base = cell_base_name(&self.f.cells[cid], cid);
            let name = claim(&mut used, base);
            self.cell_name.insert(cid, name);
        }
        used.extend(self.reserved_names.iter().cloned());
        used.extend(GLSL_KEYWORDS.split_whitespace().map(str::to_string));
        used.extend(glsl_builtin_callables().into_iter().map(str::to_string));
        for (cid, cell) in self.f.cells.iter() {
            if self.cell_name.contains_key(&cid) {
                continue;
            }
            let base = cell_base_name(cell, cid);
            let name = claim(&mut used, base);
            self.cell_name.insert(cid, name);
        }
        let mut uses: HashMap<Idx<Inst>, usize> = HashMap::default();
        for b in reverse_postorder(self.f, self.f.entry) {
            for &iid in &self.f.blocks[b].insts {
                self.f.insts[iid].uses_each(&mut |v| *uses.entry(v).or_default() += 1);
            }
            self.f.blocks[b]
                .term
                .uses_each(&mut |v| *uses.entry(v).or_default() += 1);
        }
        let mut trivial = HashMap::default();
        let mut pending: Vec<_> = uses
            .iter()
            .filter(|(_, count)| **count > 1)
            .map(|(&v, _)| v)
            .collect();
        let mut repeated = HashSet::default();
        let mut retained = HashSet::default();
        while let Some(iid) = pending.pop() {
            if !repeated.insert(iid) {
                continue;
            }
            if matches!(self.f.insts[iid], Inst::Call { .. })
                && matches!(
                    self.ty_of(iid)?,
                    GlslTy::Vec2 | GlslTy::Vec3 | GlslTy::Vec4 | GlslTy::Bvec3
                )
                && !self.trivial_value(iid, &mut trivial)
            {
                retained.insert(iid);
                continue;
            }
            // Inlining a shared component repeats its source too. A retained
            // call stops propagation: its arguments render only in its binding.
            self.rendered_uses_each(iid, &mut |v| pending.push(v));
        }
        for (iid, _) in self.f.insts.iter() {
            if !retained.contains(&iid) {
                continue;
            }
            let name = claim(&mut used, "call_result".to_string());
            self.value_name.insert(iid, name);
        }
        // A retained call is a statement in its original arm, so that arm can
        // no longer be absorbed into a folded expression.
        let mut unfold = Vec::new();
        for (&block, &cell) in &self.folded_block {
            let Term::Branch {
                then_bb, else_bb, ..
            } = self.f.blocks[block].term
            else {
                unreachable!("a folded diamond starts at a branch")
            };
            if [then_bb, else_bb].iter().any(|&b| {
                self.f.blocks[b]
                    .insts
                    .iter()
                    .any(|i| self.value_name.contains_key(i))
            }) {
                unfold.push((block, cell));
            }
        }
        for (block, cell) in unfold {
            self.folded_block.remove(&block);
            self.folded_cond.remove(&cell);
        }
        Ok(())
    }

    fn rendered_uses_each(&self, iid: Idx<Inst>, push: &mut impl FnMut(Idx<Inst>)) {
        if let Inst::Load(cell) = self.f.insts[iid] {
            if let Some(fd) = self.folded_cond.get(&cell) {
                push(fd.cond);
                push(fd.then_val);
                push(fd.else_val);
                return;
            }
            if let Some(&value) = self.folded_bound.get(&cell) {
                push(value);
                return;
            }
        }
        self.f.insts[iid].uses_each(push);
    }

    /// Constructors and arithmetic over existing values can be repeated; an
    /// actual function evaluation inside them must retain single evaluation.
    fn trivial_value(&self, iid: Idx<Inst>, memo: &mut HashMap<Idx<Inst>, bool>) -> bool {
        if let Some(&trivial) = memo.get(&iid) {
            return trivial;
        }
        if let Inst::Call { func, .. } = self.f.insts[iid] {
            let cf = &self.ws.module(func.module).functions[func.func];
            let name = declared_name(cf.name.as_str());
            let constructor = matches!(name, "v" | "bvec3") && GlslIntrinsics.lookup(cf).is_some();
            if !constructor && vec_op_symbol(name).is_none() {
                memo.insert(iid, false);
                return false;
            }
        }
        let mut trivial = true;
        self.rendered_uses_each(iid, &mut |v| trivial &= self.trivial_value(v, memo));
        memo.insert(iid, trivial);
        trivial
    }

    /// Whether `iid` is a `Load` of an entry-mode global-base struct param cell
    /// (attribute/uniform/varyings) -- a field read off it renders as a bare
    /// global, not a struct member.
    pub(crate) fn is_uniform_base(&self, iid: Idx<Inst>) -> bool {
        matches!(self.f.insts[iid], Inst::Load(c) if self.global_cells.contains(&c))
    }

    /// Type every non-param cell: prefer bone's binding type ([`Cell::ty`]) where
    /// it maps to a tier-0 GLSL type (fixes `let y: float = 0`, whose first store's
    /// rep is int); fall back to first-store inference (RPO) for compiler-minted
    /// temps (if-result, loop counter) that carry no binding type.
    pub(crate) fn compute_cell_types(&mut self) -> Result<(), CompilerError> {
        for (cid, cell) in self.f.cells.iter() {
            if self.cell_ty.contains_key(&cid) {
                continue;
            }
            if self.ignored_type_cells.contains(&cid) {
                continue;
            }
            if self
                .single_store_value(cid)
                .is_some_and(|value| matches!(self.f.insts[value], Inst::Load(base) if self.global_cells.contains(&base)))
            {
                continue;
            }
            if let Some(ty) = cell.ty {
                let g = if let Some(plan) = self.concrete_plan {
                    let id = plan.types.ty_type(ty).ok_or_else(|| {
                        err(
                            self.span(),
                            "shader profile: local cell is missing from the concrete type plan",
                        )
                    })?;
                    concrete_glsl_ty(self.interner, id, self.span(), "local")?
                } else {
                    glsl_ty(
                        self.ws,
                        self.subst
                            .as_ref()
                            .expect("source GLSL emission has a substitution"),
                        ty,
                        self.span(),
                        "local",
                    )?
                };
                self.cell_ty.insert(cid, g);
            }
        }
        let rpo = reverse_postorder(self.f, self.f.entry);
        for &b in &rpo {
            for &iid in &self.f.blocks[b].insts {
                if let Inst::Store { cell, value } = self.f.insts[iid] {
                    if !self.cell_ty.contains_key(&cell)
                        && !matches!(self.f.insts[value], Inst::Load(base) if self.global_cells.contains(&base))
                    {
                        let ty = self.ty_of(value)?;
                        self.cell_ty.insert(cell, ty);
                    }
                }
            }
        }
        // GLSL samplers are opaque: only uniforms and parameters may hold one,
        // so a sampler local or a sampler-valued `if` would fail in the driver.
        for (cid, cell) in self.f.cells.iter() {
            if self.f.params.contains(&cid) {
                continue;
            }
            if let Some(ty) = self.cell_ty.get(&cid).filter(|ty| ty.is_sampler()) {
                let what = cell
                    .name
                    .map_or_else(|| "a temporary".to_string(), |n| format!("local `{n}`"));
                return Err(err(
                    self.span(),
                    format!(
                        "shader profile: {what} holds a `{}`; GLSL samplers are opaque -- pass the sampler parameter or texture field directly",
                        ty.name()
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Detect the cosmetic-tier folds: `&&`/`||`
    /// short-circuit diamonds collapse back to infix boolean expressions.
    pub(crate) fn compute_folds(&mut self) {
        let reach: HashSet<Idx<Block>> = reverse_postorder(self.f, self.f.entry)
            .into_iter()
            .collect();
        // Cell load-counts over reachable code -- a fold is only sound when the
        // diamond's result cell has exactly one use.
        let mut load_count: HashMap<Idx<Cell>, usize> = HashMap::default();
        // Stores too: the fold renders the diamond at the cell's use, so the
        // diamond's two arms must be the cell's ONLY writers. A nested ladder
        // (`match n { 1 => .., 2 => .., _ => .. }`) folds its inner diamond while
        // an OUTER arm also stores the same single-use cell -- suppressing that
        // arm's store and silently dropping the case.
        let mut store_count: HashMap<Idx<Cell>, usize> = HashMap::default();
        for &b in &reach {
            for &iid in &self.f.blocks[b].insts {
                match self.f.insts[iid] {
                    Inst::Load(c) => *load_count.entry(c).or_default() += 1,
                    Inst::Store { cell, .. } => *store_count.entry(cell).or_default() += 1,
                    _ => {}
                }
            }
        }

        // Short-circuit diamonds: a `Branch` whose two arms each single-store the
        // same single-use bool cell, one arm storing the `&&`/`||` const identity.
        // `a && b` lowers to `if a then b else false`; `a || b` to
        // `if a then true else b` (`lower_and`/`lower_or`, no And/Or inst).
        for b in reach.iter().copied() {
            let Term::Branch {
                cond,
                then_bb,
                else_bb,
            } = self.f.blocks[b].term
            else {
                continue;
            };
            let Some(join) = self.structure.ipdom(b) else {
                continue;
            };
            let Some((tcell, tval)) = self.single_store_arm(then_bb, join) else {
                continue;
            };
            let Some((ecell, eval)) = self.single_store_arm(else_bb, join) else {
                continue;
            };
            if tcell != ecell {
                continue;
            }
            let rcell = tcell;
            if load_count.get(&rcell).copied().unwrap_or(0) != 1 {
                continue;
            }
            if store_count.get(&rcell).copied().unwrap_or(0) != 2 {
                continue;
            }
            let Some(&result_ty) = self.cell_ty.get(&rcell) else {
                continue;
            };
            // Soundness: the fold moves the expression to the cell's use site, so
            // its inputs must be unchanged there. The arms go straight to `join`
            // and store only `rcell`, so the only place an input could be mutated
            // between the diamond and the use is `join` before the use -- require
            // the use to be in `join` with no store preceding it.
            if !self.use_hoistable_to(rcell, join) {
                continue;
            }
            let kind = if result_ty == GlslTy::Bool && self.is_const_bool(eval, false) {
                FoldKind::And
            } else if result_ty == GlslTy::Bool && self.is_const_bool(tval, true) {
                FoldKind::Or
            } else {
                FoldKind::Ternary
            };
            self.folded_cond.insert(
                rcell,
                FoldedDiamond {
                    cond,
                    then_val: tval,
                    else_val: eval,
                    kind,
                    result_ty,
                },
            );
            self.folded_block.insert(b, rcell);
        }

        // Bound temps (item 3): a counted loop's bound `t = <expr>; for (; i < t;
        // ..)` folds to `for (; i < <expr>; ..)`, dropping the single-use temp.
        // The bound temp is a range-for's `hi`, evaluated once in the preheader
        // (match_counted_loop only matches range-fors), so inlining is sound when
        // `<expr>` is loop-invariant -- a const, or pure int arithmetic over cells
        // that are stored exactly once (their store dominates the preheader, so
        // re-reading in the folded condition yields the same value). Folding it
        // is not just a cleanup: a GL driver miscompiles `t = radius + 1; for
        // (; y < t; ..)` for a nested runtime-bounded kernel (the outline_detect
        // classify loop found no neighbors), while the inlined `y < (radius + 1)`
        // is correct; the counted-loop regression covers the emitted bound.
        let bounds: Vec<Idx<Inst>> = self.counted.values().map(|li| li.bound).collect();
        for bound in bounds {
            let Inst::Load(bcell) = self.f.insts[bound] else {
                continue;
            };
            if load_count.get(&bcell).copied().unwrap_or(0) != 1 {
                continue;
            }
            let Some(v) = self.single_store_value(bcell) else {
                continue;
            };
            if self.bound_value_is_invariant(v) {
                self.folded_bound.insert(bcell, v);
            }
        }
    }

    /// Whether a counted-loop bound value is loop-invariant enough to inline into
    /// the `for` header (see the fold in `precompute`). Conservative: a const, or
    /// pure int arithmetic / int<->float casts over cells that are stored exactly
    /// once in the fn (so re-reading in the folded condition can't change). The
    /// loop counter is multi-store, so it can never sneak in. Anything else (a
    /// call like `min(n, 16)`, a texture read) stays a temp -- the safe default.
    fn bound_value_is_invariant(&self, v: Idx<Inst>) -> bool {
        match self.f.insts[v] {
            Inst::Const(_) => true,
            Inst::Load(c) => {
                !self.global_cells.contains(&c) && self.single_store_value(c).is_some()
            }
            Inst::IntAdd(a, b) | Inst::IntSub(a, b) | Inst::IntMul(a, b) => {
                self.bound_value_is_invariant(a) && self.bound_value_is_invariant(b)
            }
            Inst::IntToFloat(x) | Inst::FloatToInt(x) => self.bound_value_is_invariant(x),
            _ => false,
        }
    }

    /// The single value stored to `cell`, or `None` if it is stored zero or more
    /// than once anywhere in the fn.
    fn single_store_value(&self, cell: Idx<Cell>) -> Option<Idx<Inst>> {
        let mut found = None;
        for (_, blk) in self.f.blocks.iter() {
            for &iid in &blk.insts {
                if let Inst::Store { cell: c, value } = self.f.insts[iid] {
                    if c == cell {
                        if found.is_some() {
                            return None;
                        }
                        found = Some(value);
                    }
                }
            }
        }
        found
    }

    /// Whether `rcell`'s single use is in `join` with no `Store` preceding it
    /// there -- the condition that makes hoisting the folded expression to the
    /// use site sound (no input can be mutated between the diamond and the use).
    fn use_hoistable_to(&self, rcell: Idx<Cell>, join: Idx<Block>) -> bool {
        for &iid in &self.f.blocks[join].insts {
            match self.f.insts[iid] {
                Inst::Load(c) if c == rcell => return true,
                Inst::Store { .. } => return false,
                _ => {}
            }
        }
        false
    }

    /// If `arm` is a single-store block (exactly one `Store`, any number of pure
    /// value insts) ending `Goto(join)`, return its `(cell, stored value)`.
    fn single_store_arm(
        &self,
        arm: Idx<Block>,
        join: Idx<Block>,
    ) -> Option<(Idx<Cell>, Idx<Inst>)> {
        if self.f.blocks[arm].term != Term::Goto(join) {
            return None;
        }
        let mut store = None;
        for &iid in &self.f.blocks[arm].insts {
            if let Inst::Store { cell, value } = self.f.insts[iid] {
                if store.is_some() {
                    return None;
                }
                store = Some((cell, value));
            }
        }
        store
    }

    fn is_const_bool(&self, iid: Idx<Inst>, want: bool) -> bool {
        matches!(self.f.insts[iid], Inst::Const(Lit::Bool(b)) if b == want)
    }

    /// Render a folded diamond as one GLSL expression.
    pub(crate) fn render_folded(&self, fd: &FoldedDiamond) -> Result<String, CompilerError> {
        let cond = self.render_val(fd.cond)?;
        Ok(match fd.kind {
            FoldKind::And => format!("({} && {})", cond, self.render_val(fd.then_val)?),
            FoldKind::Or => format!("({} || {})", cond, self.render_val(fd.else_val)?),
            FoldKind::Ternary => format!(
                "({} ? {} : {})",
                cond,
                self.render_val_t(fd.then_val, fd.result_ty)?,
                self.render_val_t(fd.else_val, fd.result_ty)?
            ),
        })
    }
}
