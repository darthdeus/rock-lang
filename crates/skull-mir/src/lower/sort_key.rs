//! Structured sort keys: `Vec<T>.sort<K>` where `K` is a value struct or a
//! tuple compares `K` leaf by leaf, first leaf first. The stdlib routes such a
//! sort through `__sort_key_wide` (a compile-time bool an `if` folds on) and
//! `__sort_key_lt` (the comparison), so a scalar key keeps its original path.
use super::*;
use crate::lower::shape::mono_value_shape;
use crate::monoty::MonoTy;

impl<'a> Builder<'a> {
    /// The key type `K` of a `fn(T) -> K` argument.
    fn sort_key_ty(&self, key_fn: Idx<Expr>) -> Result<MonoTy, CompilerError> {
        match self.concrete_expr_ty(key_fn) {
            Some(MonoTy::Function { ret, .. }) => Ok((**ret).clone()),
            _ => Err(self.gap_here("sort key function has no concrete function type")),
        }
    }

    /// Whether `K` is compared leaf by leaf rather than with one `<`.
    pub(crate) fn sort_key_is_structured(&self, key_fn: Idx<Expr>) -> Result<bool, CompilerError> {
        let ty = self.sort_key_ty(key_fn)?;
        Ok(matches!(ty.peel_nullable().0, MonoTy::Tuple(_))
            || mono_value_shape(self.ws, &ty).is_some())
    }

    /// `if __sort_key_wide(f) { .. } else { .. }`: the arm to lower, decided now.
    pub(crate) fn static_sort_key_condition(
        &self,
        cond: Idx<Expr>,
    ) -> Result<Option<bool>, CompilerError> {
        let Expr::Call {
            callee: Callee::Unresolved { name, .. },
            args,
            ..
        } = &self.body.expr_arena[cond]
        else {
            return Ok(None);
        };
        // Every `if` asks; only the one stdlib spelling pays for resolution.
        if name.as_str() != "__sort_key_wide" {
            return Ok(None);
        }
        let Ok(func) = self.resolve_funcref(cond, *name) else {
            return Ok(None);
        };
        if self.fn_expand_kind(func) != Some(ExpandKind::SortKeyWide) {
            return Ok(None);
        }
        let [key_fn] = (*args).into_iter().collect::<Vec<_>>()[..] else {
            return Err(gap("`__sort_key_wide` expects one argument"));
        };
        self.sort_key_is_structured(key_fn).map(Some)
    }

    pub(crate) fn expand_sort_key_wide(
        &mut self,
        arg_idxs: &[Idx<Expr>],
    ) -> Result<ValueRepr, CompilerError> {
        let [key_fn] = arg_idxs else {
            return Err(gap("`__sort_key_wide` expects one argument"));
        };
        let wide = self.sort_key_is_structured(*key_fn)?;
        Ok(ValueRepr::Scalar(
            self.emit_cur(Inst::Const(Lit::Bool(wide))),
        ))
    }

    /// `key_fn(a) < key_fn(b)` over the leaves of `K`: the first unequal leaf
    /// decides, and equal keys are not less.
    pub(crate) fn expand_sort_key_lt(
        &mut self,
        e: Idx<Expr>,
        arg_idxs: &[Idx<Expr>],
    ) -> Result<ValueRepr, CompilerError> {
        let [key_fn, a, b] = arg_idxs else {
            return Err(gap("`__sort_key_lt` expects three arguments"));
        };
        let ty = self.sort_key_ty(*key_fn)?;
        let leaf_tys = self.sort_key_leaf_tys(&ty).map_err(|reason| {
            CompilerError::new_with_label(
                self.body.expr_spans[e],
                format!(
                    "cannot sort by a key of type `{}`: {reason}; a key compares its \
                     int, uint, float and string leaves in order",
                    ty.render()
                ),
            )
        })?;
        let fn_ty = *self
            .inf
            .and_then(|inf| inf.expr_types.get(*key_fn))
            .ok_or_else(|| self.gap_here("sort key function has no type"))?;
        let plan = self
            .fn_value_rep_plan(fn_ty)?
            .ok_or_else(|| self.gap_here("sort key function has no function ABI plan"))?;
        let callee = self.lower_expr(*key_fn)?;
        let lhs = self.sort_key_leaves(&plan, callee, *a, &ty)?;
        let rhs = self.sort_key_leaves(&plan, callee, *b, &ty)?;
        if lhs.len() != leaf_tys.len() || rhs.len() != leaf_tys.len() {
            return Err(self.gap_here("sort key leaves disagree with the key type"));
        }

        let result = self
            .f
            .new_cell_typed(None, self.current_expr_storage_ty_id());
        let join = self.f.new_block();
        let last = leaf_tys.len() - 1;
        for (i, leaf_ty) in leaf_tys.iter().enumerate() {
            let (l, r) = (lhs[i], rhs[i]);
            if i < last {
                let differs = self.emit_cur(Inst::NotEq(l, r));
                let decide = self.f.new_block();
                let next = self.f.new_block();
                self.f.set_term(
                    self.cur,
                    Term::Branch {
                        cond: differs,
                        then_bb: decide,
                        else_bb: next,
                    },
                );
                self.cur = decide;
                let lt = self.emit_cur(leaf_lt(leaf_ty, l, r));
                self.store_and_join(result, lt, join);
                self.cur = next;
            } else {
                let lt = self.emit_cur(leaf_lt(leaf_ty, l, r));
                self.store_and_join(result, lt, join);
            }
        }
        self.cur = join;
        Ok(ValueRepr::Scalar(self.emit_cur(Inst::Load(result))))
    }

    /// One call of the key, flattened to its leaves.
    fn sort_key_leaves(
        &mut self,
        plan: &FunctionRepPlan,
        callee: Idx<Inst>,
        arg: Idx<Expr>,
        ty: &MonoTy,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        let key = self.indirect_call_by_plan(plan.clone(), callee, &[arg])?;
        let mut out = Vec::new();
        self.push_key_leaves(key.into_value(), ty, &mut out)?;
        Ok(out)
    }

    fn push_key_leaves(
        &mut self,
        r: ValueRepr,
        ty: &MonoTy,
        out: &mut Vec<Idx<Inst>>,
    ) -> Result<(), CompilerError> {
        if let MonoTy::Tuple(elems) = ty {
            let parts: Vec<ValueRepr> = match r {
                ValueRepr::Tuple { parts, .. } if parts.len() == elems.len() => parts,
                other => {
                    let base = self.pack(other)?;
                    (0..elems.len())
                        .map(|index| {
                            ValueRepr::Scalar(self.emit_cur(Inst::TupleIndex { base, index }))
                        })
                        .collect()
                }
            };
            for (part, elem) in parts.into_iter().zip(elems) {
                self.push_key_leaves(part, elem, out)?;
            }
            return Ok(());
        }
        if let Some(shape) = mono_value_shape(self.ws, ty) {
            out.extend(self.repr_to_leaves_shape(&r, &shape)?);
            return Ok(());
        }
        out.push(self.pack(r)?);
        Ok(())
    }

    /// The leaf types of key type `K`, or why `K` has no order.
    fn sort_key_leaf_tys(&self, ty: &MonoTy) -> Result<Vec<MonoTy>, String> {
        let mut out = Vec::new();
        self.push_sort_key_leaf_tys(ty, &mut out)?;
        Ok(out)
    }

    fn push_sort_key_leaf_tys(&self, ty: &MonoTy, out: &mut Vec<MonoTy>) -> Result<(), String> {
        match ty {
            MonoTy::Int | MonoTy::Uint | MonoTy::Float | MonoTy::String => {
                out.push(ty.clone());
                Ok(())
            }
            MonoTy::Tuple(elems) => elems
                .iter()
                .try_for_each(|elem| self.push_sort_key_leaf_tys(elem, out)),
            MonoTy::Nullable(_) => Err(format!("`{}` may be absent", ty.render())),
            _ => {
                let Some(shape) = mono_value_shape(self.ws, ty) else {
                    return Err(format!("`{}` has no order", ty.render()));
                };
                let leaves = self
                    .value_shape_leaf_monos(&shape)
                    .ok_or_else(|| format!("`{}` has a leaf with no order", ty.render()))?;
                leaves
                    .iter()
                    .try_for_each(|leaf| self.push_sort_key_leaf_tys(leaf, out))
            }
        }
    }
}

fn leaf_lt(ty: &MonoTy, l: Idx<Inst>, r: Idx<Inst>) -> Inst {
    match ty {
        MonoTy::Int => Inst::IntLt(l, r),
        MonoTy::Uint => Inst::UintLt(l, r),
        // Both backends emit `<` for strings too, which is the order a scalar
        // string key sorts by.
        _ => Inst::FloatLt(l, r),
    }
}
