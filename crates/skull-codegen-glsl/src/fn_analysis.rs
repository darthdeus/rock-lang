use super::*;

impl FnEmit<'_> {
    pub(crate) fn assign_names(&mut self, rpo: &[Idx<Block>]) -> Result<(), CompilerError> {
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
        for &b in rpo {
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
            self.rendered_uses_each(iid, &mut |v| pending.push(v));
        }
        for (iid, _) in self.f.insts.iter() {
            if !retained.contains(&iid) {
                continue;
            }
            let name = claim(&mut used, "call_result".to_string());
            self.value_name.insert(iid, name);
        }
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

    fn trivial_value(&self, iid: Idx<Inst>, memo: &mut HashMap<Idx<Inst>, bool>) -> bool {
        if let Some(&trivial) = memo.get(&iid) {
            return trivial;
        }
        if let Inst::Call { func, .. } = self.f.insts[iid] {
            let cf = &self.ws.module(func.module).functions[func.func];
            let name = declared_name(cf.name.as_str());
            let constructor = matches!(name, "v" | "bvec3") && intrinsic_template(cf).is_some();
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

    pub(crate) fn is_uniform_base(&self, iid: Idx<Inst>) -> bool {
        matches!(self.f.insts[iid], Inst::Load(c) if self.global_cells.contains(&c))
    }

    pub(crate) fn compute_cell_types(&mut self, rpo: &[Idx<Block>]) -> Result<(), CompilerError> {
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
                let id = self.concrete_plan.types.ty_type(ty).ok_or_else(|| {
                    err(
                        self.span(),
                        "shader profile: local cell is missing from the concrete type plan",
                    )
                })?;
                let g = concrete_glsl_ty(self.interner, id, self.span(), "local")?;
                self.cell_ty.insert(cid, g);
            }
        }
        for &b in rpo {
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

    pub(crate) fn compute_folds(&mut self, rpo: &[Idx<Block>]) {
        let reach: HashSet<Idx<Block>> = rpo.iter().copied().collect();
        let mut load_count: HashMap<Idx<Cell>, usize> = HashMap::default();
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
            // The diamond arms must be the cell's only writers: a nested ladder folds its inner
            // diamond while an outer arm also stores the cell, which would drop that case.
            if store_count.get(&rcell).copied().unwrap_or(0) != 2 {
                continue;
            }
            let Some(&result_ty) = self.cell_ty.get(&rcell) else {
                continue;
            };
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

        // Fold a single-use bound only when it is loop-invariant. Keeping it in
        // the loop header also avoids a driver miscompile of the hoisted temp.
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

    // The fold moves the expression to the use, so its inputs must not be mutated in between:
    // require the use in `join` with no store before it.
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
