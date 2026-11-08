//! Terminators, numeric selections and enum boxes.

use super::*;

impl Emitter<'_> {
    fn selection_arm(&self, bb: Idx<Block>) -> Option<SelectionArm> {
        let block = &self.func.blocks[bb];
        if let Term::Return(values) = &block.term {
            let [value] = values.as_slice() else {
                return None;
            };
            // A return destination can replace the old store/join sink, but
            // skipping its arm must obey the same no-effects restriction.
            if !block.insts.iter().all(|id| {
                *id == *value && matches!(self.func.insts[*id], Inst::Load(_) | Inst::Const(_))
            }) {
                return None;
            }
            return Some(SelectionArm::Return(*value));
        }
        let Term::Goto(join) = block.term else {
            return None;
        };
        let store = match block.insts.as_slice() {
            [] => None,
            [s] => match self.func.insts[*s] {
                Inst::Store { cell, value } => Some((cell, value)),
                _ => return None,
            },
            [l, s] if matches!(self.func.insts[*l], Inst::Load(_) | Inst::Const(_)) => {
                match self.func.insts[*s] {
                    Inst::Store { cell, value } if value == *l => Some((cell, value)),
                    _ => return None,
                }
            }
            _ => return None,
        };
        Some(SelectionArm::Join { store, join })
    }

    fn selection_cell_matches(
        &self,
        bb: Idx<Block>,
        cond: Idx<Inst>,
        cell: Idx<Cell>,
        operand: Idx<Inst>,
    ) -> bool {
        // A selected cell can still hold the captured comparison operand after
        // copy propagation. Do not carry that equivalence across effects.
        let insts = &self.func.blocks[bb].insts;
        for &id in insts.iter().rev() {
            match self.func.insts[id] {
                Inst::Store {
                    cell: target,
                    value,
                } if target == cell => return value == operand,
                Inst::Load(source) if id == operand && source == cell => return true,
                Inst::Load(_) | Inst::Const(_) => {}
                _ if id == cond => {}
                _ => return false,
            }
        }
        matches!(self.func.insts[operand], Inst::Load(source) if source == cell)
            && self.inlinable[raw(operand) as usize]
    }

    fn selection_matches(
        &self,
        bb: Idx<Block>,
        cond: Idx<Inst>,
        value: Idx<Inst>,
        operand: Idx<Inst>,
    ) -> bool {
        if value == operand {
            return true;
        }
        match (&self.func.insts[value], &self.func.insts[operand]) {
            (Inst::Const(Lit::Float(x)), Inst::Const(Lit::Float(y))) => x.to_bits() == y.to_bits(),
            (Inst::Const(x), Inst::Const(y)) => x == y,
            (Inst::Load(cell), _) => self.selection_cell_matches(bb, cond, *cell, operand),
            _ => false,
        }
    }

    fn numeric_selection(
        &self,
        bb: Idx<Block>,
        cond: Idx<Inst>,
        yes: Idx<Block>,
        no: Idx<Block>,
    ) -> Option<(SelectionTarget, Idx<Inst>, Idx<Inst>, &'static str)> {
        if !self.inlinable[raw(cond) as usize] {
            return None;
        }
        let (a, b, less) = match self.func.insts[cond] {
            Inst::FloatLt(a, b) | Inst::IntLt(a, b) => (a, b, true),
            Inst::FloatGt(a, b) | Inst::IntGt(a, b) => (a, b, false),
            _ => return None,
        };
        // Reversing a selection's operands must not reorder deferred effects.
        for id in [a, b] {
            if self.inlinable[raw(id) as usize]
                && !matches!(self.func.insts[id], Inst::Load(_) | Inst::Const(_))
                && !self.plain_int_to_float(id)
            {
                return None;
            }
        }
        let (target, yes_value, no_value) =
            match (self.selection_arm(yes)?, self.selection_arm(no)?) {
                (SelectionArm::Return(yes), SelectionArm::Return(no)) => {
                    (SelectionTarget::Return, Some(yes), Some(no))
                }
                (
                    SelectionArm::Join { store: yes, join },
                    SelectionArm::Join {
                        store: no,
                        join: no_join,
                    },
                ) => {
                    if join != no_join {
                        return None;
                    }
                    let cell = yes.or(no)?.0;
                    if yes.is_some_and(|s| s.0 != cell) || no.is_some_and(|s| s.0 != cell) {
                        return None;
                    }
                    (
                        SelectionTarget::Cell { cell, join },
                        yes.map(|s| s.1),
                        no.map(|s| s.1),
                    )
                }
                _ => return None,
            };
        let matches = |value: Option<Idx<Inst>>, operand: Idx<Inst>| match value {
            Some(value) => self.selection_matches(bb, cond, value, operand),
            None => match target {
                SelectionTarget::Cell { cell, .. } => {
                    self.selection_cell_matches(bb, cond, cell, operand)
                }
                SelectionTarget::Return => false,
            },
        };
        if matches(yes_value, a) && matches(no_value, b) {
            Some((target, a, b, if less { "min" } else { "max" }))
        } else if matches(yes_value, b) && matches(no_value, a) {
            // Native min/max choose the second operand on ties/unordered input.
            Some((target, b, a, if less { "max" } else { "min" }))
        } else {
            None
        }
    }

    fn numeric_clamp(
        &self,
        bb: Idx<Block>,
        cond: Idx<Inst>,
        yes: Idx<Block>,
        no: Idx<Block>,
    ) -> Option<(SelectionTarget, Idx<Inst>, f64, f64)> {
        if !self.inlinable[raw(cond) as usize] {
            return None;
        }
        let Inst::FloatLt(value, lower) = self.func.insts[cond] else {
            return None;
        };
        let (target, low_value) = self.selection_arm(yes)?.target_value()?;
        if !self.selection_matches(bb, cond, low_value, lower) {
            return None;
        }
        let block = &self.func.blocks[no];
        let Term::Branch {
            cond: inner,
            then_bb,
            else_bb,
        } = block.term
        else {
            return None;
        };
        if !block.insts.iter().all(|id| {
            *id == inner || matches!(self.func.insts[*id], Inst::Load(_) | Inst::Const(_))
        }) {
            return None;
        }
        let (inner_target, upper, inner_value, "min") =
            self.numeric_selection(no, inner, then_bb, else_bb)?
        else {
            return None;
        };
        let lo = self.selection_float_literal(bb, lower)?;
        let hi = self.selection_float_literal(no, upper)?;
        // The source tests the lower bound first; an inverted range would lose
        // that priority in a nested min/max expression. NaN bounds are unordered.
        if !matches!(
            lo.partial_cmp(&hi),
            Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal)
        ) || !self.selection_matches(bb, cond, inner_value, value)
        {
            return None;
        }
        if inner_target != target {
            let SelectionTarget::Cell {
                cell: inner_cell,
                join: tail,
            } = inner_target
            else {
                return None;
            };
            let (forwarded_target, forwarded) = self.selection_arm(tail)?.target_value()?;
            if forwarded_target != target
                || !matches!(self.func.insts[forwarded], Inst::Load(c) if c == inner_cell)
            {
                return None;
            }
        }
        if self.inlinable[raw(value) as usize]
            && !matches!(self.func.insts[value], Inst::Load(_) | Inst::Const(_))
        {
            return None;
        }
        Some((target, value, lo, hi))
    }

    fn selection_float_literal(&self, bb: Idx<Block>, operand: Idx<Inst>) -> Option<f64> {
        if let Inst::Const(Lit::Float(value)) = self.func.insts[operand] {
            return Some(value);
        }
        let Inst::Load(cell) = self.func.insts[operand] else {
            return None;
        };
        // Captured frame fields can change through callbacks without a cell def.
        if self.cell_is_frame_field(cell) {
            return None;
        }
        let preds = self
            .selection_preds
            .get_or_init(|| predecessors(self.func, self.rpo).0);
        let mut block = bb;
        let mut limit = self.func.blocks[bb]
            .insts
            .iter()
            .position(|id| *id == operand)?;
        let mut seen = HashSet::default();
        loop {
            if !seen.insert(block) {
                return None;
            }
            for &id in self.func.blocks[block].insts[..limit].iter().rev() {
                let mut writes = false;
                regalloc::cell_defs(&self.func.insts[id], &mut |target| writes |= target == cell);
                if writes {
                    return match self.func.insts[id] {
                        Inst::Store { value, .. } => match self.func.insts[value] {
                            Inst::Const(Lit::Float(value)) => Some(value),
                            _ => None,
                        },
                        _ => None,
                    };
                }
            }
            let [parent] = preds.get(&block)?.as_slice() else {
                return None;
            };
            block = *parent;
            limit = self.func.blocks[block].insts.len();
        }
    }

    fn write_selection_result(
        &self,
        out: &mut String,
        bb: Idx<Block>,
        target: SelectionTarget,
        value: impl FnOnce(&mut String),
    ) {
        out.push_str("  ");
        match target {
            SelectionTarget::Cell { cell, .. } => {
                write_cellname(out, self.func, self.frame, self.cell_colors, cell);
                out.push_str(" = ");
            }
            SelectionTarget::Return => out.push_str("do return "),
        }
        value(out);
        match target {
            SelectionTarget::Cell { join, .. } => {
                out.push('\n');
                // Selection skips arm entry kills; apply them at the join edge.
                self.write_kills(out, &self.kills.on_edge(bb, join));
                self.write_goto(out, join);
            }
            SelectionTarget::Return => out.push_str(" end\n"),
        }
    }

    pub(crate) fn emit_term(&self, out: &mut String, term: &Term, bb: Idx<Block>) {
        match term {
            Term::Return(vals) if !vals.is_empty() => self.mark_inline_operand(out, vals[0]),
            Term::Branch { cond, .. } => self.mark_inline_operand(out, *cond),
            Term::Switch { discr, .. } => self.mark_inline_operand(out, *discr),
            _ => {}
        }
        match term {
            // luajit rule 2: `return` wrapped `do ... end` so it is never the
            // block's mid-sequence statement (and is legal as the last one too).
            Term::Return(vals) if !vals.is_empty() => {
                out.push_str("  do return ");
                self.write_csv(out, vals);
                out.push_str(" end\n");
            }
            Term::Return(_) => {
                out.push_str("  do return end\n");
            }
            // Bone proves these blocks dead; reaching one is a compiler bug,
            // which a bare return would turn into a silent early exit.
            Term::Unreachable => {
                out.push_str("  error(\"rock: reached an unreachable block\")\n");
            }
            Term::Goto(b) => {
                self.write_goto(out, *b);
            }
            Term::Branch {
                cond,
                then_bb,
                else_bb,
            } => {
                if let Some((target, value, lo, hi)) =
                    self.numeric_clamp(bb, *cond, *then_bb, *else_bb)
                {
                    self.write_selection_result(out, bb, target, |out| {
                        write!(out, "math.max({lo:?}, math.min({hi:?}, ").unwrap();
                        self.write_opnd(out, value);
                        out.push_str("))");
                    });
                    return;
                }
                if let Some((target, a, b, op)) =
                    self.numeric_selection(bb, *cond, *then_bb, *else_bb)
                {
                    self.write_selection_result(out, bb, target, |out| {
                        out.push_str("math.");
                        out.push_str(op);
                        out.push('(');
                        self.write_opnd(out, a);
                        out.push_str(", ");
                        self.write_opnd(out, b);
                        out.push(')');
                    });
                    return;
                }
                out.push_str("  if ");
                self.write_opnd(out, *cond);
                out.push_str(" then goto ");
                self.write_label_ref(out, *then_bb);
                out.push_str(" else goto ");
                self.write_label_ref(out, *else_bb);
                out.push_str(" end\n");
            }
            Term::Switch {
                discr,
                cases,
                default,
            } => {
                // `discr` is compared in every case arm; render it once.
                let d = self.opnd_str(*discr);
                let mut first = true;
                for (l, b) in cases {
                    let kw = if first { "if" } else { "elseif" };
                    first = false;
                    let _ = write!(out, "  {kw} {d} == ");
                    write_lit(out, l);
                    out.push_str(" then goto ");
                    self.write_label_ref(out, *b);
                    out.push('\n');
                }
                if first {
                    self.write_goto(out, *default);
                } else {
                    out.push_str("  else goto ");
                    self.write_label_ref(out, *default);
                    out.push_str(" end\n");
                }
            }
            Term::Yield { .. } => self.reject_pass_residue("a `yield`", "coroutine transform"),
        }
    }

    pub(crate) fn write_dynamic_enum(
        &self,
        out: &mut String,
        tag: Idx<Inst>,
        ty: ustr::Ustr,
        positional: &[Idx<Inst>],
        named: &[(ustr::Ustr, Idx<Inst>)],
    ) {
        if matches!(self.func.insts[tag], Inst::Const(_)) {
            self.write_enum_box(out, tag, ty, positional, named);
            return;
        }
        let cached = self.enum_box_groups.borrow().get(&ty).cloned();
        let groups = if let Some(groups) = cached {
            groups
        } else {
            let plan = skull_mir::lower::EnumPlan::for_name(self.ws, ty)
                .expect("dynamic enum has a representation plan");
            let shape = plan.flat().expect("dynamic enum has a flat shape");
            let eid = self.ws.enums_by_name[&ty];
            let mut groups = EnumBoxGroups::new();
            for variant in &self.ws.modules[eid.module.index()].enums[eid.def].variants {
                if matches!(variant.payload, bone_core::hir::VariantPayload::Unit) {
                    continue;
                }
                let layout = shape.box_layout(self.ws, variant.name);
                if let Some((members, _)) = groups.iter_mut().find(|(_, key)| *key == layout) {
                    members.push(variant.name);
                } else {
                    groups.push((vec![variant.name], layout));
                }
            }
            let common = groups
                .iter()
                .enumerate()
                .max_by_key(|(_, (members, _))| members.len())
                .map(|(i, _)| i)
                .expect("flat enum has payload variants");
            let last = groups.remove(common);
            groups.push(last);
            let groups = std::rc::Rc::new(groups);
            self.enum_box_groups.borrow_mut().insert(ty, groups.clone());
            groups
        };
        if groups.len() > 1 {
            out.push('(');
        }
        for (i, (members, (width, fields))) in groups.iter().enumerate() {
            if i + 1 < groups.len() {
                out.push('(');
                for (j, variant) in members.iter().enumerate() {
                    if j > 0 {
                        out.push_str(" or ");
                    }
                    self.write_opnd(out, tag);
                    out.push_str(" == ");
                    write_lua_string(out, variant.as_str());
                }
                out.push_str(") and ");
            }
            let selected: Vec<_> = named
                .iter()
                .copied()
                .filter(|(name, _)| fields.contains(name))
                .collect();
            self.write_enum_box(out, tag, ty, &positional[..*width], &selected);
            if i + 1 < groups.len() {
                out.push_str(" or ");
            }
        }
        if groups.len() > 1 {
            out.push(')');
        }
    }

    fn write_enum_box(
        &self,
        out: &mut String,
        tag: Idx<Inst>,
        ty: ustr::Ustr,
        positional: &[Idx<Inst>],
        named: &[(ustr::Ustr, Idx<Inst>)],
    ) {
        out.push_str("{__variant = ");
        self.write_opnd(out, tag);
        out.push_str(", __ty = ");
        write_lua_string(out, skull_mir::mangle::type_symbol(self.ws, ty).as_str());
        for id in positional {
            out.push_str(", ");
            self.write_opnd(out, *id);
        }
        for (name, id) in named {
            let _ = write!(out, ", {name} = ");
            self.write_opnd(out, *id);
        }
        out.push('}');
    }

    pub(crate) fn write_csv(&self, out: &mut String, ids: &[Idx<Inst>]) {
        for (i, id) in ids.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            self.write_opnd(out, *id);
        }
    }
}
