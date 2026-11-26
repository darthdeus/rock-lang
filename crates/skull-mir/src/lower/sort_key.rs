//! The typed `Vec<T>.sort<K>` path compares structured keys leaf by leaf
//! and loads elements whose layout requires their concrete type.
use super::call::DirectCall;
use super::*;
use crate::lower::shape::mono_value_shape;
use crate::monoty::MonoTy;

impl<'a> Builder<'a> {
    fn uses_sort_key_leaf_abi(&self) -> bool {
        self.site_owner.as_ref().is_some_and(|owner| {
            let module = self.ws.module(owner.func.module);
            module.functions[owner.func.func].name.as_str() == "__vec_sort_by_key_leaves"
                && bone_core::modules::is_stdlib_module_file(module.file.as_str())
        })
    }

    fn is_stdlib_sort(&self, func: FuncRef) -> bool {
        let module = self.ws.module(func.module);
        let decl = &module.functions[func.func];
        matches!(decl.name.as_str(), "sort" | "__vec_sort_by_key_leaves")
            && bone_core::modules::is_stdlib_module_file(module.file.as_str())
            && decl.param_decls.len() == 2
    }

    /// Only the stdlib sort consumes this private callback ABI.
    pub(crate) fn sort_call_args(
        &mut self,
        e: Idx<Expr>,
        dc: &DirectCall<'a>,
        args: &[Idx<Expr>],
        recv: Option<Idx<Inst>>,
    ) -> Result<Option<Vec<Idx<Inst>>>, CompilerError> {
        let func = dc.callee.instance.func;
        if !self.is_stdlib_sort(func) {
            return Ok(None);
        }
        // Vec.sort already received the private callback from its caller.
        if self.site_owner.as_ref().is_some_and(|owner| {
            let module = self.ws.module(owner.func.module);
            module.functions[owner.func.func].name.as_str() == "sort"
                && bone_core::modules::is_stdlib_module_file(module.file.as_str())
        }) {
            return Ok(None);
        }
        let signature = self.direct_call_signature(dc)?;
        let key_ty = self
            .concrete_param_ty_of(signature, &dc.callee.instance, 1)?
            .ok_or_else(|| self.gap_here("sort key function has no concrete type"))?;
        let MonoTy::Function { ret, .. } = key_ty.as_ref() else {
            return Err(self.gap_here("sort key function has no concrete function type"));
        };
        if !self.sort_takes_leaf_path(&key_ty)? {
            return Ok(None);
        }
        self.validate_sort_key(e, ret)?;
        let plan = self.sort_key_plan(&key_ty, true)?;
        let decl = &self.ws.module(func.module).functions[func.func];
        let mut slots = [recv, None];
        let mut next_pos = usize::from(recv.is_some());
        for &arg in args {
            if self.body.spread_args.contains(&arg) {
                for (name, bundle) in self.spread_field_bundles(arg)? {
                    let pos = decl
                        .param_decls
                        .iter()
                        .position(|p| p.name == name)
                        .ok_or_else(|| self.gap_here("sort spread field matches no parameter"))?;
                    let value = self.pack(bundle.into_value())?;
                    slots[pos] = Some(if pos == 1 {
                        self.adapt_sort_key(arg, value, &key_ty, plan.clone())?
                    } else {
                        value
                    });
                }
                continue;
            }
            let pos = if let Some(name) = self.body.arg_names.get(&arg) {
                decl.param_decls
                    .iter()
                    .position(|p| p.name == *name)
                    .ok_or_else(|| self.gap_here("sort argument matches no parameter"))?
            } else {
                let pos = next_pos;
                next_pos += 1;
                pos
            };
            let slot = slots
                .get_mut(pos)
                .ok_or_else(|| self.gap_here("sort argument is outside its parameter list"))?;
            *slot = Some(if pos == 1 {
                self.lower_sort_key(arg, &key_ty, plan.clone())?
            } else {
                self.lower_expr(arg)?
            });
        }
        let [Some(vec), Some(key)] = slots else {
            return Err(self.gap_here("sort is missing its receiver or key"));
        };
        Ok(Some(vec![vec, key]))
    }

    fn sort_key_plan(&self, ty: &MonoTy, leaves: bool) -> Result<FunctionRepPlan, CompilerError> {
        BackendFnAbiPlan::for_function_value(self.ws, self.scx, ty, !leaves)
            .map_err(|error| self.rep_error(error))
    }

    fn lower_sort_key(
        &mut self,
        key: Idx<Expr>,
        ty: &MonoTy,
        plan: FunctionRepPlan,
    ) -> Result<Idx<Inst>, CompilerError> {
        if let Expr::Lambda { params, body } = self.body.expr_arena[key] {
            return self.lower_lambda_with_plan(key, params, body, plan);
        }
        let callee = self.lower_expr(key)?;
        self.adapt_sort_key(key, callee, ty, plan)
    }

    fn adapt_sort_key(
        &mut self,
        key: Idx<Expr>,
        callee: Idx<Inst>,
        ty: &MonoTy,
        plan: FunctionRepPlan,
    ) -> Result<Idx<Inst>, CompilerError> {
        let original = self
            .fn_value_rep_plan_for_mono(ty)?
            .ok_or_else(|| self.gap_here("sort key function has no ABI"))?;
        if original == plan {
            return Ok(callee);
        }
        // The function value may be called elsewhere, so adapt its fixed ABI here
        // rather than changing those calls.
        let capture = self.fresh_cells(1)[0];
        self.store_cell(capture, callee);
        let saved = self.cur;
        let entry = self.f.new_block();
        self.cur = entry;
        let callee = self.emit_cur(Inst::Load(capture));
        let arena = self.scx.rep_arena();
        let mut params = Vec::new();
        let mut argv = Vec::new();
        for (&source, &dest) in plan.params().iter().zip(original.params()) {
            let source = arena.layout(source);
            let cells = self.fresh_cells(source.slot_count(self.scx.rep_arena()));
            let loads = self.load_cells(&cells);
            params.extend(cells);
            let value = self.slot_repr(source, &loads, &mut 0);
            argv.extend(self.sort_slot_values(value, arena.layout(dest))?);
        }
        let result = original
            .result()
            .ok_or_else(|| self.gap_here("sort key has no result"))?;
        let slot = arena.layout(result);
        let value = if slot.splats() {
            let cells = self.fresh_cells(slot.slot_count(self.scx.rep_arena()));
            let (call, loads) = self.call_value_multi(cells, callee, argv);
            self.f.call_rep_plans.insert(call, original);
            self.slot_repr(slot, &loads, &mut 0)
        } else {
            let call = self.emit_cur(Inst::CallValue { callee, args: argv });
            self.f.call_rep_plans.insert(call, original);
            ValueRepr::Scalar(call)
        };
        let result = plan
            .result()
            .ok_or_else(|| self.gap_here("sort key has no result"))?;
        let leaves = self.sort_slot_values(value, arena.layout(result))?;
        self.f.set_term(self.cur, Term::Return(leaves));
        self.cur = saved;
        let outer_expr = self.cur_expr.replace(key);
        let outer_span = self.cur_span.replace(self.body.expr_spans[key]);
        let lambda = self.emit_cur(Inst::Lambda { params, entry });
        self.cur_expr = outer_expr;
        self.cur_span = outer_span;
        self.f.lambda_abis.insert(
            lambda,
            LambdaAbi {
                scalar_entry: None,
                fingerprint: plan.logical_fingerprint(arena),
                representation: plan,
                source: None,
            },
        );
        Ok(lambda)
    }

    fn sort_slot_values(
        &mut self,
        value: ValueRepr,
        slot: &RepLayout,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        match slot {
            RepLayout::Opaque => Ok(vec![self.pack(value)?]),
            RepLayout::Leaves(shape) => self.repr_to_leaves_shape(&value, shape),
            RepLayout::Tuple(shape) => self.tuple_repr_to_slots(value, shape),
        }
    }

    fn validate_sort_key(&self, e: Idx<Expr>, ty: &MonoTy) -> Result<Vec<MonoTy>, CompilerError> {
        self.sort_key_leaf_tys(ty).map_err(|reason| {
            CompilerError::new_with_label(
                self.body.expr_spans[e],
                format!(
                    "cannot sort by a key of type `{}`: {reason}; a key compares its \
                     int, uint, float and string leaves in order",
                    ty.render()
                ),
            )
        })
    }

    fn sort_key_ty(&self, key_fn: Idx<Expr>) -> Result<MonoTy, CompilerError> {
        match self.concrete_expr_ty(key_fn) {
            Some(MonoTy::Function { ret, .. }) => Ok((**ret).clone()),
            _ => Err(self.gap_here("sort key function has no concrete function type")),
        }
    }

    /// A splatting tuple needs its typed slots. A generic column element in Lua
    /// needs its concrete instantiation to reconstruct the key's value argument.
    fn sort_takes_leaf_path(&self, key_fn_ty: &MonoTy) -> Result<bool, CompilerError> {
        let MonoTy::Function { params, ret } = key_fn_ty else {
            return Err(self.gap_here("sort key function has no concrete function type"));
        };
        let structured_key = matches!(ret.peel_nullable().0, MonoTy::Tuple(_))
            || mono_value_shape(self.ws, ret).is_some();
        let splatting_element = match params.first() {
            Some(element @ MonoTy::Tuple(_)) => self
                .concrete_rep_tuple(element, RepUse::Return, true, false)?
                .is_some_and(|shape| shape.splats(self.scx.rep_arena())),
            _ => false,
        };
        let generic_column_element = if self.scx.target() == bone_core::hir::FnBackend::Lua
            && let Some(
                element @ MonoTy::App {
                    ctor: MonoCtor::Nominal(_),
                    params,
                },
            ) = params.first()
            && !params.is_empty()
        {
            let vec_ty = MonoTy::App {
                ctor: MonoCtor::Array,
                params: vec![element.clone()],
            };
            self.vec_flat_elem_for_type(&vec_ty)?
                .is_some_and(|(elem, storage)| storage == FlatStorage::Columns && !elem.is_nobox())
        } else {
            false
        };
        Ok(structured_key || splatting_element || generic_column_element)
    }

    pub(crate) fn sort_key_is_structured(&self, key_fn: Idx<Expr>) -> Result<bool, CompilerError> {
        let ty = self
            .concrete_expr_ty(key_fn)
            .ok_or_else(|| self.gap_here("sort key function has no concrete type"))?;
        self.sort_takes_leaf_path(ty)
    }

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

    /// The first unequal leaf decides; equal keys are not less.
    pub(crate) fn expand_sort_key_lt(
        &mut self,
        e: Idx<Expr>,
        arg_idxs: &[Idx<Expr>],
    ) -> Result<ValueRepr, CompilerError> {
        let [key_fn, a, b] = arg_idxs else {
            return Err(gap("`__sort_key_lt` expects three arguments"));
        };
        let ty = self.sort_key_ty(*key_fn)?;
        let leaf_tys = match self.validate_sort_key(e, &ty) {
            Ok(leaves) => leaves,
            // Generic stdlib bodies lower independently; the sort boundary rejects this
            // key at the user's call.
            Err(_) if self.uses_sort_key_leaf_abi() => {
                return Ok(ValueRepr::Scalar(
                    self.emit_cur(Inst::Const(Lit::Bool(false))),
                ));
            }
            Err(error) => return Err(error),
        };
        let fn_ty = self
            .concrete_expr_ty(*key_fn)
            .ok_or_else(|| self.gap_here("sort key function has no concrete type"))?;
        let plan = self.sort_key_plan(fn_ty, self.uses_sort_key_leaf_abi())?;
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

    fn sort_key_leaves(
        &mut self,
        plan: &FunctionRepPlan,
        callee: Idx<Inst>,
        arg: Idx<Expr>,
        ty: &MonoTy,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        let key = self.indirect_call_by_plan(plan.clone(), None, callee, &[arg])?;
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
        // Both backends emit `<` for strings too, matching a scalar string key's order.
        _ => Inst::FloatLt(l, r),
    }
}
