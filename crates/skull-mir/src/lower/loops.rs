use super::*;

impl<'a> Builder<'a> {
    pub(crate) fn loop_target(
        &self,
        what: &str,
    ) -> Result<(Idx<Block>, Idx<Block>, usize), CompilerError> {
        self.loop_targets
            .last()
            .map(|&(cont, brk, cont_depth, break_depth)| {
                (
                    cont,
                    brk,
                    if what == "break" {
                        break_depth
                    } else {
                        cont_depth
                    },
                )
            })
            .ok_or_else(|| gap(format!("`{what}` outside a loop")))
    }

    pub(crate) fn push_loop_target(
        &mut self,
        cont: Idx<Block>,
        brk: Idx<Block>,
        break_depth: usize,
    ) {
        self.loop_targets
            .push((cont, brk, self.defers.len(), break_depth));
    }

    pub(crate) fn lower_while(
        &mut self,
        cond: Idx<Expr>,
        body: Idx<Expr>,
    ) -> Result<bool, CompilerError> {
        let header = self.f.new_block();
        let body_bb = self.f.new_block();
        let exit = self.f.new_block();
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = header;
        let const_true = matches!(self.body.expr_arena[cond], Expr::Bool(true));
        if const_true {
            // `while true`: exit is reachable only via `break`, which
            // `used_break_targets` records.
            self.f.set_term(self.cur, Term::Goto(body_bb));
        } else {
            self.lower_branch(cond, body_bb, exit, true)?;
        }

        self.cur = body_bb;
        self.push_loop_target(header, exit, self.defers.len());
        self.lower_stmt_expr(body)?;
        self.loop_targets.pop();
        if self.is_open() {
            self.f.set_term(self.cur, Term::Goto(header)); // back-edge
        }

        let diverges = const_true && !self.used_break_targets.contains(&exit);
        self.cur = exit;
        // The dead exit still gets a terminator so every block ends terminated.
        if diverges {
            self.f.set_term(self.cur, Term::Goto(header));
        }
        Ok(diverges)
    }

    pub(crate) fn lower_for(
        &mut self,
        vars: BindingPattern,
        iter: Idx<Expr>,
        body: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        if let Expr::Range { low, high } = self.body.expr_arena[iter] {
            return match vars {
                BindingPattern::Leaf(var_b) => self.lower_for_range(var_b, low, high, body),
                BindingPattern::Tuple(_) => Err(gap("for-range with a tuple pattern")),
            };
        }
        let enumerated_collection = match self.body.expr_arena[iter] {
            Expr::Call {
                callee: Callee::Unresolved { name, .. },
                args,
                ..
            } if name.as_str() == "enumerate" && args.len() == 1 => Some(args.start()),
            _ => None,
        };
        if let Some(info) = self
            .inf
            .and_then(|i| i.for_iter_protocol.get(&iter).cloned())
        {
            // A plain `[T]` walks its storage directly: `VecIter` would only inline back
            // into this counted loop. Shader lowering keeps the protocol.
            if !self.scx.values_first_class
                && enumerated_collection.is_none()
                && vars.leaves().iter().all(|&b| !self.binding_is_ref(b))
                && self.concrete_expr_ty(iter).is_some_and(|ty| {
                    matches!(
                        ty,
                        MonoTy::App {
                            ctor: crate::monoty::MonoCtor::Array,
                            ..
                        }
                    )
                })
            {
                return self.lower_for_vec(&vars, iter, body, false);
            }
            if self.scx.target() == bone_core::hir::FnBackend::Lua {
                if let Some(collection) = enumerated_collection.filter(|&c| self.iter_is_arena(c)) {
                    return self.lower_for_arena(&vars, collection, body, true);
                }
                // The arena head takes the same walk: the pair iterator's `next` skips free
                // slots in an inner loop that no pass turns back into a counted one.
                let walkable = match &vars {
                    BindingPattern::Leaf(_) => true,
                    BindingPattern::Tuple(subs) => subs.len() == 2,
                };
                if enumerated_collection.is_none()
                    && walkable
                    && self.iter_is_arena(iter)
                    && vars.leaves().iter().all(|&b| !self.binding_is_ref(b))
                {
                    return self.lower_for_arena(&vars, iter, body, false);
                }
            }
            return self.lower_for_user_iter(&vars, iter, body, &info);
        }
        if let Some(collection) = enumerated_collection {
            if self.iter_is_arena(collection) {
                return Err(gap("enumerated Arena must have an iterator protocol"));
            }
            return self.lower_for_vec(&vars, collection, body, true);
        }
        if self.iter_is_arena(iter) {
            // Bone routes a direct `for .. in arena` to the stdlib ArenaPairIter.
            return Err(gap(
                "for over Arena must route through the iterator protocol (for_iter_protocol missing)",
            ));
        }
        // FlatVec is routed here so `coll_kind` stays closed over
        // Vec/Map/NativeBuffer.
        if self.expr_is_flat_vec(iter) {
            return self.lower_for_array(vars, iter, body);
        }
        match self.coll_kind(iter)? {
            ContainerKind::Vec
            | ContainerKind::VecNonNull
            | ContainerKind::VecValueColumns(_)
            | ContainerKind::VecValueStrided(_) => Err(gap(
                "for over [T] must route through the iterator protocol (for_iter_protocol missing)",
            )),
            ContainerKind::NativeBuffer | ContainerKind::NativeBufferValue(_) => {
                self.lower_for_array(vars, iter, body)
            }
            ContainerKind::Map => self.lower_for_map(None, &vars, iter, body),
            ContainerKind::Record => unreachable!("for over a heap record"),
            // Leaf columns and leaf views are minted during element lowering, never user
            // types, so neither can be a `for` head.
            ContainerKind::VecColumn => Err(gap("for over a synthetic Vec leaf column")),
            ContainerKind::VecStrideSlot => Err(gap("for over a synthetic Vec strided slot")),
            ContainerKind::VecInBounds
            | ContainerKind::VecNonNullInBounds
            | ContainerKind::NativeBufferInBounds => Err(gap("for over a proven element access")),
            ContainerKind::NativeBufferLeaf => {
                Err(gap("for over a synthetic NativeBuffer leaf view"))
            }
        }
    }

    pub(crate) fn lower_for_user_iter(
        &mut self,
        vars: &BindingPattern,
        iter_expr: Idx<Expr>,
        body: Idx<Expr>,
        info: &ForIterInfo,
    ) -> Result<(), CompilerError> {
        let outer_expr = self.cur_expr.replace(iter_expr);
        let out = self.lower_for_user_iter_witnessed(vars, iter_expr, body, info);
        self.cur_expr = outer_expr;
        out
    }

    fn lower_for_user_iter_witnessed(
        &mut self,
        vars: &BindingPattern,
        iter_expr: Idx<Expr>,
        body: Idx<Expr>,
        info: &ForIterInfo,
    ) -> Result<(), CompilerError> {
        let it_val = match info.iter.as_ref().map(|w| w.func) {
            Some((m, f)) => {
                let iter_ref = FuncRef { module: m, func: f };
                let bundle = self.lower_expr_repr(iter_expr)?;
                // Resolved before the arguments are shaped: the splat must match the body
                // the call targets.
                let witness = self.with_site_op(crate::site::SiteOp::ForIter, |b| {
                    b.concrete_callee_for_site(iter_expr)
                })?;
                let args = self.iter_witness_args(&witness, bundle)?;
                // The head's `iter` witness and the head expression's own call share one
                // span; the site operation keeps them distinguishable.
                let call = self.with_site_op(crate::site::SiteOp::ForIter, |b| {
                    b.emit_cur(Inst::Call {
                        func: iter_ref,
                        args,
                    })
                });
                // The witness is an ordinary call to a generic `iter`, so under
                // instance-owned emission it must name a concrete instance.
                if let Some(delivery) = self.concrete_delivery_instance(&witness) {
                    self.f.spec_bindings.insert(call, delivery);
                }
                if self.ws.module(m).functions[f].generics.is_empty() || !witness.args.is_empty() {
                    crate::site::record_call_owner(&mut self.f, call, &witness);
                }
                self.f
                    .call_abi_plans
                    .insert(call, self.callee_abi_plan_for_instance(&witness)?);
                call
            }
            None => self.lower_expr(iter_expr)?,
        };
        let it_cell = self.f.new_cell(Some(crate::names::ustr("__it")));
        self.store_cell(it_cell, it_val);
        let owned = self.head_constructs_coroutine(iter_expr);
        let owned_flag = self.own_loop_coroutine(iter_expr, it_cell);
        if owned {
            self.defers.push(vec![(
                iter_expr,
                stmt::PendingDefer::LoopCoroutine {
                    iterator: it_cell,
                    flag: owned_flag,
                },
            )]);
        }

        // The `next` shape is read under the concrete callee the emitted call
        // carries, not the caller's, so both lowering sides agree.
        let concrete_next = if matches!(info.next, BoundNext::Concrete { .. }) {
            self.with_site_op(crate::site::SiteOp::ForNext(Box::default()), |b| {
                b.concrete_callee_for_site(iter_expr)
            })?
            .into()
        } else {
            None
        };
        let ret_shape = match &info.next {
            BoundNext::Concrete { .. } => self
                .callee_abi_plan_for_instance(
                    concrete_next
                        .as_ref()
                        .ok_or_else(|| gap("for-iter `next` has no concrete callee"))?,
                )?
                .ret_tuple()
                .cloned()
                .ok_or_else(|| gap("for-iter `next` must return a tuple"))?,
            BoundNext::CallerGeneric(gidx) => self.bound_next_tuple_shape(*gidx)?,
        };
        if ret_shape.slots.len() < 2 {
            return Err(gap("for-iter `next` must return `(bool, E)`"));
        }
        let n = ret_shape.slot_count(self.scx.rep_arena());

        let header = self.f.new_block();
        let body_bb = self.f.new_block();
        let exit = self.f.new_block();
        self.f.for_iter_headers.push(header);
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = header;
        let it_load = self.emit_cur(Inst::Load(it_cell));
        let dests = self.fresh_cells(n);
        match &info.next {
            BoundNext::Concrete {
                func: (m, f), sub, ..
            } => {
                let func = concrete_next
                    .as_ref()
                    .map(|owner| owner.func)
                    .unwrap_or(FuncRef {
                        module: *m,
                        func: *f,
                    });
                let call = self.with_site_op(crate::site::SiteOp::ForNext(Box::default()), |b| {
                    b.emit_cur(Inst::CallMulti {
                        dests: dests.clone(),
                        func,
                        args: vec![it_load],
                    })
                });
                if !sub.is_empty() {
                    self.f.bound_trees.insert(call, sub.clone());
                }
                let owner = concrete_next
                    .as_ref()
                    .ok_or_else(|| gap("concrete for-next has no callee instance"))?;
                if !sub.is_empty() {
                    self.record_callee_bound_tree(call, owner, true)?;
                }
                if let Some(delivery) = self.concrete_delivery_instance(owner) {
                    self.f.spec_bindings.insert(call, delivery);
                }
                crate::site::record_call_owner(&mut self.f, call, owner);
                self.f
                    .call_abi_plans
                    .insert(call, self.callee_abi_plan_for_instance(owner)?);
            }
            BoundNext::CallerGeneric(gidx) => {
                // Takes the same `ForNext` operation as the concrete arm; otherwise the
                // witness is recorded on one arm and invisible on the other.
                let call = self.with_site_op(crate::site::SiteOp::ForNext(Box::default()), |b| {
                    b.emit_cur(Inst::CallBoundMulti {
                        dests: dests.clone(),
                        generic_idx: *gidx as u32,
                        args: vec![it_load],
                    })
                });
                if ret_shape.splats(self.scx.rep_arena()) {
                    self.f.bound_next_split.insert(call);
                }
            }
        }
        let ok = self.emit_cur(Inst::Load(dests[0]));
        self.branch_to(ok, body_bb, exit);

        // Multiple elements (`next -> (bool, A, B)`) destructure against a matching
        // tuple pattern without allocating.
        self.cur = body_bb;
        let loads = self.load_cells(&dests[1..]);
        let elem_slot = match &ret_shape.slots[1..] {
            [one] => self.scx.rep_arena().layout(*one).clone(),
            rest => RepLayout::Tuple(TupleLayout {
                slots: rest.to_vec(),
                nullable: false,
            }),
        };
        let mut idx = 0;
        self.bind_pattern_slot(vars, &elem_slot, &loads, &mut idx)?;
        self.push_loop_target(header, exit, self.defers.len() - usize::from(owned));
        self.lower_stmt_expr(body)?;
        self.loop_targets.pop();
        self.goto_if_open(header);
        if owned {
            self.defers.pop();
        }
        self.cur = exit;
        if let Some(flag) = owned_flag {
            let off = self.emit_cur(Inst::Const(Lit::Bool(false)));
            self.store_cell(flag, off);
        }
        Ok(())
    }

    /// Retain an owned generator's cleanup while its coroutine is suspended.
    /// The same flag is cleared by early-exit cleanup and natural exhaustion.
    fn own_loop_coroutine(
        &mut self,
        iter_expr: Idx<Expr>,
        it_cell: Idx<Cell>,
    ) -> Option<Idx<Cell>> {
        if self.defer_mode != stmt::DeferMode::CoFlag || !self.head_constructs_coroutine(iter_expr)
        {
            return None;
        }
        let flag = self.f.new_cell(Some(crate::names::ustr("__defer")));
        let on = self.emit_cur(Inst::Const(Lit::Bool(true)));
        self.store_cell(flag, on);
        let saved_cur = self.cur;
        let entry = self.f.new_block();
        self.cur = entry;
        self.kill_loop_coroutine(it_cell, Some(flag));
        self.f.set_term(self.cur, Term::Return(Vec::new()));
        self.cur = saved_cur;
        self.f.coro_defers.push((flag, entry));
        self.register_owned_reload(flag, it_cell);
        Some(flag)
    }

    fn register_owned_reload(&mut self, flag: Idx<Cell>, iterator: Idx<Cell>) {
        let saved = self.cur;
        let entry = self.f.new_block();
        let reload = self.f.new_block();
        let done = self.f.new_block();
        self.cur = entry;
        let active = self.emit_cur(Inst::Load(flag));
        self.branch_to(active, reload, done);
        self.cur = reload;
        let it = self.emit_cur(Inst::Load(iterator));
        let fire = self.emit_cur(Inst::Global(crate::names::ustr(
            crate::mir::CORO_HOT_RELOAD,
        )));
        self.emit_cur(Inst::CallValue {
            callee: fire,
            args: vec![it],
        });
        self.f.set_term(self.cur, Term::Goto(done));
        self.f.set_term(done, Term::Return(Vec::new()));
        self.cur = saved;
        if let Some(tail) = self.owned_reload_tail {
            self.f.set_term(tail, Term::Goto(entry));
        } else {
            let lambda = self.emit_cur(Inst::Lambda {
                params: Vec::new(),
                entry,
            });
            self.emit_cur(Inst::CoroHook {
                kind: CoroHookKind::OnOwnedHotReload,
                lambda,
            });
        }
        self.owned_reload_tail = Some(done);
    }

    pub(crate) fn kill_loop_coroutine(&mut self, iterator: Idx<Cell>, flag: Option<Idx<Cell>>) {
        let done = flag.map(|flag| {
            let kill = self.f.new_block();
            let done = self.f.new_block();
            let active = self.emit_cur(Inst::Load(flag));
            self.branch_to(active, kill, done);
            self.cur = kill;
            let off = self.emit_cur(Inst::Const(Lit::Bool(false)));
            self.store_cell(flag, off);
            done
        });
        let it = self.emit_cur(Inst::Load(iterator));
        let kill = self.emit_cur(Inst::Global(crate::names::ustr(crate::mir::CORO_KILL)));
        self.emit_cur(Inst::CallValue {
            callee: kill,
            args: vec![it],
        });
        if let Some(done) = done {
            self.f.set_term(self.cur, Term::Goto(done));
            self.cur = done;
        }
    }

    fn head_constructs_coroutine(&self, e: Idx<Expr>) -> bool {
        let Expr::Call {
            callee: Callee::Unresolved { name, .. },
            ..
        } = self.body.expr_arena[e]
        else {
            return false;
        };
        let Some(inf) = self.inf else {
            return false;
        };
        resolve_call_fn(self.ws, inf, e, name)
            .is_some_and(|(m, f)| self.ws.module(m).functions[f].is_coroutine)
    }

    pub(crate) fn scrut_variants(&self, e: Idx<Expr>) -> HashSet<Ustr> {
        if let Some(ty) = self.concrete_expr_ty(e) {
            let Some(name) = ty.peel_nullable().0.nominal() else {
                return HashSet::default();
            };
            if let Some(&eid) = self.ws.enums_by_name.get(&name) {
                let ed = &self.ws.modules[eid.module.index()].enums[eid.def];
                return ed.variants.iter().map(|v| v.name).collect();
            }
            return HashSet::default();
        }
        HashSet::default()
    }

    /// Lets a payload-free enum's tag test int-compare the ordinal instead of
    /// reading `.__variant`.
    pub(crate) fn scrut_enum_name(&self, e: Idx<Expr>) -> Option<Ustr> {
        if let Some(ty) = self.concrete_expr_ty(e) {
            let name = ty.peel_nullable().0.nominal()?;
            return self.ws.enums_by_name.contains_key(&name).then_some(name);
        }
        None
    }

    pub(crate) fn expr_is_newtype(&self, e: Idx<Expr>) -> bool {
        if let Some(ty) = self.concrete_expr_ty(e) {
            return ty
                .peel_nullable()
                .0
                .nominal()
                .is_some_and(|name| self.ws.newtypes_by_name.contains_key(&name));
        }
        false
    }

    pub(crate) fn iter_is_arena(&self, iter: Idx<Expr>) -> bool {
        if let Some(ty) = self.concrete_expr_ty(iter) {
            return ty
                .peel_nullable()
                .0
                .nominal()
                .is_some_and(|name| bone_core::modules::is_intrinsic_key(name, "Arena"));
        }
        false
    }

    pub(crate) fn lower_for_arena(
        &mut self,
        vars: &BindingPattern,
        iter: Idx<Expr>,
        body: Idx<Expr>,
        enumerate: bool,
    ) -> Result<(), CompilerError> {
        let (enum_var, pat): (Option<Idx<BindingInfo>>, BindingPattern) = if enumerate {
            match vars {
                BindingPattern::Tuple(subs) if subs.len() == 2 => match &subs[0] {
                    BindingPattern::Leaf(i) => (Some(*i), subs[1].clone()),
                    _ => return Err(gap("enumerate(arena) binds (index, element)")),
                },
                _ => return Err(gap("enumerate(arena) binds a (index, element) tuple")),
            }
        } else {
            (None, vars.clone())
        };
        // Occupancy is the liveness check; generations only supply the handle's
        // payload, and an unused row needs no snapshot of the data.
        let entity_used = self.arena_component_used(&pat, 0, body)?;
        let data_used = self.arena_component_used(&pat, 1, body)?;
        let enum_var = enum_var.filter(|&b| self.loop_pattern_used(&BindingPattern::Leaf(b)));

        // At an all-scalar value `T` the arena's `data` is column-stored, so a bare
        // `Index` would read a table that isn't there.
        let column_elem = self.arena_column_elem_of(iter)?;

        // The columns are resolved once, outside the counted loop. A retired handle
        // resolves to an empty store, which the `i < n` header exits immediately.
        let arena = self.lower_iterable_collection(iter)?;
        let scene_id = self.read_field(arena, crate::names::ustr("_scene_id"));
        let arena_id = self.read_field(arena, crate::names::ustr("_arena_id"));
        let store = self.with_site_expr(iter, |b| {
            b.call_spine("__arena_store_or_empty", vec![scene_id, arena_id])
        })?;
        let occupied = self.read_field(store, crate::names::ustr("occupied"));
        let generations =
            entity_used.then(|| self.read_field(store, crate::names::ustr("generations")));
        let data = data_used.then(|| self.read_field(store, crate::names::ustr("data")));
        let occupied_kind = self.boxed_vec_kind(Some(&MonoTy::Bool));
        let n = self.emit_cur(Inst::Len {
            base: occupied,
            kind: occupied_kind,
        });
        let i_cell = self.f.new_cell(None);
        self.f.mark_loop_binding(i_cell);
        let zero = self.emit_cur(Inst::Const(Lit::Int(0)));
        self.store_cell(i_cell, zero);
        let enum_cell = enum_var.map(|_| {
            let c = self.f.new_cell(None);
            let z = self.emit_cur(Inst::Const(Lit::Int(0)));
            self.store_cell(c, z);
            c
        });

        let header = self.f.new_block();
        let body_bb = self.f.new_block();
        let occ_bb = self.f.new_block();
        let do_bb = self.f.new_block();
        let latch = self.f.new_block();
        let exit = self.f.new_block();
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = header;
        let i = self.emit_cur(Inst::Load(i_cell));
        let c = self.emit_cur(Inst::IntLt(i, n));
        self.branch_to(c, body_bb, exit);

        // The walk is bounded by the occupancy column's own length, and a `[bool]`
        // column has no holes, so no nil test is needed.
        self.cur = body_bb;
        let idx = self.emit_cur(Inst::Load(i_cell));
        self.f.set_term(self.cur, Term::Goto(occ_bb));

        self.cur = occ_bb;
        let occ = self.read_index(occupied, idx, occupied_kind);
        self.branch_to(occ, do_bb, latch);

        self.cur = do_bb;
        let entity = if let Some(generations) = generations {
            let e_gen = self.read_index(generations, idx, self.boxed_vec_kind(Some(&MonoTy::Int)));
            let entity_representation =
                self.nominal_value_representation(crate::names::ustr("Entity"))?;
            Some(self.emit_cur(Inst::MakeValueStruct {
                name: crate::names::ustr("Entity"),
                representation: entity_representation,
                form: ValueCtorForm::Declared,
                fields: vec![
                    (crate::names::ustr("_scene_id"), scene_id),
                    (crate::names::ustr("_arena_id"), arena_id),
                    (crate::names::ustr("_slot"), idx),
                    (crate::names::ustr("_generation"), e_gen),
                ],
            }))
        } else {
            None
        };
        // A column element is rebuilt from its leaves; for a `nobox` element that is
        // an implicit materialization, which the verifier rejects by name.
        let data = if let Some(data) = data {
            Some(match column_elem {
                Some(elem) => {
                    let repr = self.column_element_repr(data, idx, elem)?;
                    self.pack(repr.into_value())?
                }
                None => {
                    // The stored element stays the entry's; the binder gets its own copy.
                    let elem = match self.concrete_expr_ty(iter).map(|ty| ty.peel_nullable().0) {
                        Some(MonoTy::App { params, .. }) if self.iter_is_arena(iter) => {
                            params.first()
                        }
                        _ => None,
                    };
                    let stored = self.read_index(data, idx, self.boxed_vec_kind(elem));
                    let ty = match &pat {
                        BindingPattern::Tuple(subs) if subs.len() == 2 => {
                            self.loop_binder_ty(&subs[1], None)
                        }
                        _ => self.loop_binder_ty(&pat, Some(1)),
                    };
                    match ty {
                        Some(ty) => self.copied_value_box(&ty, stored)?,
                        None => stored,
                    }
                }
            })
        } else {
            None
        };
        match &pat {
            BindingPattern::Tuple(subs) if subs.len() == 2 => {
                if let Some(entity) = entity {
                    self.bind_for_pattern(&subs[0], entity)?;
                }
                if let Some(data) = data {
                    self.bind_for_pattern(&subs[1], data)?;
                }
            }
            BindingPattern::Leaf(_) if entity_used || data_used => {
                // A whole-pair use demands both components, so no source read sees this
                // placeholder.
                let entity = entity.unwrap_or_else(|| self.emit_cur(Inst::Const(Lit::Nil)));
                let data = data.unwrap_or_else(|| self.emit_cur(Inst::Const(Lit::Nil)));
                let pair = ValueRepr::Tuple {
                    parts: vec![ValueRepr::Scalar(entity), ValueRepr::Scalar(data)],
                    present: None,
                };
                self.bind_loop_element_repr(&pat, pair)?;
            }
            _ => {}
        }
        if let (Some(ev), Some(ec)) = (enum_var, enum_cell) {
            let cur = self.emit_cur(Inst::Load(ec));
            let bind_cell = self.new_binding_cell(
                ev,
                self.body.binding_arena[ev].name,
                self.binding_ty(ev),
                crate::names::ustr(""),
            );
            self.binds.insert(raw(ev), bind_cell);
            self.store_cell(bind_cell, cur);
            let inc = self.int_plus_one(cur);
            self.store_cell(ec, inc);
        }
        self.push_loop_target(latch, exit, self.defers.len());
        self.lower_stmt_expr(body)?;
        self.loop_targets.pop();
        self.goto_if_open(latch);

        self.cur = latch;
        self.increment_cell(i_cell);
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = exit;
        Ok(())
    }

    /// Lua stateless `next` iteration: the key cell is the cursor and a nil key
    /// after `MapNext` ends the loop. The enumerate counter steps in the latch.
    pub(crate) fn lower_for_map(
        &mut self,
        counter: Option<Idx<BindingInfo>>,
        vars: &BindingPattern,
        iter: Idx<Expr>,
        body: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        // A column-stored Map iterates its spine, yielding (key, slot), and reads the
        // value leaf-wise out of `data`; no element table exists.
        let col = self.map_column_value_of(iter)?;
        let m = self.lower_expr(iter)?;
        let table = match col {
            Some(_) => self.read_field(m, crate::names::ustr("spine")),
            None => m,
        };
        let key_cell = self.f.new_cell(None);
        let val_cell = self.f.new_cell(None);
        self.f.mark_loop_binding(key_cell);
        self.f.mark_loop_binding(val_cell);
        let nil = self.emit_cur(Inst::Const(Lit::Nil));
        self.store_cell(key_cell, nil);
        let i_cell = match counter {
            Some(ib) => {
                let i_cell = self.new_binding_cell(
                    ib,
                    self.body.binding_arena[ib].name,
                    self.binding_ty(ib),
                    crate::names::ustr(""),
                );
                self.binds.insert(raw(ib), i_cell);
                self.f.mark_loop_binding(i_cell);
                let zero = self.emit_cur(Inst::Const(Lit::Int(0)));
                self.store_cell(i_cell, zero);
                Some(i_cell)
            }
            None => None,
        };

        let header = self.f.new_block();
        let body_bb = self.f.new_block();
        let latch = match i_cell {
            Some(_) => self.f.new_block(),
            None => header,
        };
        let exit = self.f.new_block();
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = header;
        self.emit_cur(Inst::MapNext {
            key_cell,
            val_cell,
            table,
        });
        let k = self.emit_cur(Inst::Load(key_cell));
        self.branch_on_nil(k, exit, body_bb);

        self.cur = body_bb;
        let needs_value = match vars {
            BindingPattern::Tuple(subs) if subs.len() == 2 => self.loop_pattern_used(&subs[1]),
            _ => self.loop_pattern_used(vars),
        };
        let vv = match col.filter(|_| needs_value) {
            Some(elem) => {
                let slot = self.emit_cur(Inst::Load(val_cell));
                let data = self.map_col_data(m);
                let repr = self.column_element_repr(data, slot, elem)?;
                Some(self.pack(repr.into_value())?)
            }
            None => None,
        };
        self.bind_map_entry(vars, key_cell, val_cell, vv)?;
        self.push_loop_target(latch, exit, self.defers.len());
        self.lower_stmt_expr(body)?;
        self.loop_targets.pop();
        self.goto_if_open(latch);

        if let Some(i_cell) = i_cell {
            self.cur = latch;
            self.increment_cell(i_cell);
            self.f.set_term(self.cur, Term::Goto(header));
        }

        self.cur = exit;
        Ok(())
    }

    // An unused loop binding needs no snapshot, which also drops a column read's
    // bounds check. Writes count as uses: an assignment still needs the cell.
    fn loop_pattern_used(&self, pattern: &BindingPattern) -> bool {
        let leaves = pattern.leaves();
        self.body.expr_arena.iter().any(|(_, expr)| match expr {
            Expr::Var(b)
            | Expr::Call {
                callee: Callee::Local(b),
                ..
            } => leaves.contains(b),
            _ => false,
        })
    }

    /// A single arena binder holds a pair. Only a projection of the other
    /// component can avoid its snapshot; whole-pair uses and writes demand both.
    fn arena_component_used(
        &self,
        pattern: &BindingPattern,
        component: usize,
        body: Idx<Expr>,
    ) -> Result<bool, CompilerError> {
        use bone_core::hir_walk::HirChild;

        let binding = match pattern {
            BindingPattern::Tuple(subs) if subs.len() == 2 => {
                return Ok(self.loop_pattern_used(&subs[component]));
            }
            BindingPattern::Leaf(b) => *b,
            _ => {
                return Err(gap(
                    "for-over-Arena binds one (item) or two (entity, value)",
                ));
            }
        };
        let mut pending = vec![HirChild::Expr(body)];
        while let Some(node) = pending.pop() {
            if let HirChild::Expr(e) = node {
                match self.body.expr_arena[e] {
                    Expr::TupleIndex { tuple, index }
                        if index != component
                            && matches!(self.body.expr_arena[tuple], Expr::Var(b) if b == binding) =>
                    {
                        continue;
                    }
                    Expr::Var(b)
                    | Expr::Call {
                        callee: Callee::Local(b),
                        ..
                    } if b == binding => return Ok(true),
                    _ => {}
                }
            }
            self.body.for_each_child(node, |child| pending.push(child));
        }
        Ok(false)
    }

    /// `col_value` is the element of a column-stored Map. `None` keeps the boxed
    /// walk, loading the value cell in place so the emitted order is unchanged.
    pub(crate) fn bind_map_entry(
        &mut self,
        vars: &BindingPattern,
        key_cell: Idx<Cell>,
        val_cell: Idx<Cell>,
        col_value: Option<Idx<Inst>>,
    ) -> Result<(), CompilerError> {
        match vars {
            BindingPattern::Tuple(subs) if subs.len() == 2 => {
                if self.loop_pattern_used(&subs[0]) {
                    let kv = self.emit_cur(Inst::Load(key_cell));
                    self.bind_for_pattern(&subs[0], kv)?;
                }
                if !self.loop_pattern_used(&subs[1]) {
                    return Ok(());
                }
                let vv = match col_value {
                    Some(v) => v,
                    None => {
                        let v = self.emit_cur(Inst::Load(val_cell));
                        let ty = self.loop_binder_ty(&subs[1], None);
                        let r = self.loop_element_snapshot(ty, ValueRepr::Scalar(v))?;
                        self.pack(r)?
                    }
                };
                self.bind_for_pattern(&subs[1], vv)?;
                Ok(())
            }
            BindingPattern::Leaf(_) => {
                if !self.loop_pattern_used(vars) {
                    return Ok(());
                }
                let kv = self.emit_cur(Inst::Load(key_cell));
                let vv = match col_value {
                    Some(v) => v,
                    None => {
                        let v = self.emit_cur(Inst::Load(val_cell));
                        let ty = self.loop_binder_ty(vars, Some(1));
                        let r = self.loop_element_snapshot(ty, ValueRepr::Scalar(v))?;
                        self.pack(r)?
                    }
                };
                let pair = ValueRepr::Tuple {
                    parts: vec![ValueRepr::Scalar(kv), ValueRepr::Scalar(vv)],
                    present: None,
                };
                self.bind_loop_element_repr(vars, pair)
            }
            _ => Err(gap(
                "for-over-Map binds one (item) or two (key, value) vars",
            )),
        }
    }

    pub(crate) fn lower_for_vec(
        &mut self,
        vars: &BindingPattern,
        coll: Idx<Expr>,
        body: Idx<Expr>,
        enumerated: bool,
    ) -> Result<(), CompilerError> {
        let split = match vars {
            _ if !enumerated => None,
            BindingPattern::Tuple(subs) if subs.len() == 2 => match &subs[0] {
                BindingPattern::Leaf(i) => Some((*i, &subs[1])),
                _ => return Err(gap("enumerate binds (index, element)")),
            },
            _ => None,
        };
        if !self.expr_is_flat_vec(coll) && self.coll_kind(coll)? == ContainerKind::Map {
            let (ib, xpat) =
                split.ok_or_else(|| gap("enumerate over Map with a single binding"))?;
            return self.lower_for_map(Some(ib), xpat, coll, body);
        }
        self.lower_counted_loop(coll, body, split.map(|(i, _)| i), |s, i_now, repr| {
            let elem_pat = split.map(|(_, pat)| pat).or((!enumerated).then_some(vars));
            let ty = match elem_pat {
                Some(pat) => s.loop_binder_ty(pat, None),
                None => s.loop_binder_ty(vars, Some(1)),
            };
            let repr = s.loop_element_snapshot(ty, repr)?;
            // A tuple element whose `nobox` parts ride its box as leaves destructures
            // from the box's slots, not from its plain indices.
            if let Some(pat) = elem_pat {
                if let (BindingPattern::Tuple(_), ValueRepr::Scalar(base)) = (pat, &repr)
                    && let Some(MonoTy::App { params, .. }) = s.concrete_expr_ty(coll)
                    && let [elem @ MonoTy::Tuple(_)] = params.as_slice()
                    && let Some(shape) = s
                        .concrete_rep_tuple(elem, RepUse::Return, true, false)?
                        .filter(|shape| shape.splats(s.scx.rep_arena()))
                {
                    s.bind_tuple_repr(pat, &shape, ValueRepr::Scalar(*base))?;
                } else {
                    s.bind_loop_element_repr(pat, repr)?;
                }
            } else {
                let pair = ValueRepr::Tuple {
                    parts: vec![ValueRepr::Scalar(i_now), repr],
                    present: None,
                };
                s.bind_loop_element_repr(vars, pair)?;
            }
            Ok(())
        })
    }

    fn lower_counted_loop(
        &mut self,
        coll: Idx<Expr>,
        body: Idx<Expr>,
        index: Option<Idx<BindingInfo>>,
        bind: impl FnOnce(&mut Self, Idx<Inst>, ValueRepr) -> Result<(), CompilerError>,
    ) -> Result<(), CompilerError> {
        let (kind, arr_v, len_v) = self.lower_counted_storage(coll)?;
        let i_cell = match index {
            Some(ib) => {
                let c = self.new_binding_cell(
                    ib,
                    self.body.binding_arena[ib].name,
                    self.binding_ty(ib),
                    crate::names::ustr(""),
                );
                self.binds.insert(raw(ib), c);
                c
            }
            None => self.f.new_cell(None),
        };
        self.f.mark_loop_binding(i_cell);
        let zero = self.emit_cur(Inst::Const(Lit::Int(0)));
        self.store_cell(i_cell, zero);

        let header = self.f.new_block();
        let body_bb = self.f.new_block();
        let latch = self.f.new_block();
        let exit = self.f.new_block();
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = header;
        let i = self.emit_cur(Inst::Load(i_cell));
        let cmp = self.emit_cur(Inst::IntLt(i, len_v));
        self.branch_to(cmp, body_bb, exit);

        self.cur = body_bb;
        let i_now = self.emit_cur(Inst::Load(i_cell));
        let repr = self.loop_element_repr_at(arr_v, i_now, kind)?;
        bind(self, i_now, repr)?;
        self.push_loop_target(latch, exit, self.defers.len());
        self.lower_stmt_expr(body)?;
        self.loop_targets.pop();
        self.goto_if_open(latch);

        self.cur = latch;
        self.increment_cell(i_cell);
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = exit;
        Ok(())
    }

    /// A FlatVec is bounded by its `len` field, not the buffer capacity, and
    /// indexes its `buf` NativeBuffer.
    fn lower_counted_storage(
        &mut self,
        coll: Idx<Expr>,
    ) -> Result<(ContainerKind, Idx<Inst>, Idx<Inst>), CompilerError> {
        if self.expr_is_flat_vec(coll) {
            let kind = self.nb_kind(coll)?;
            let fv = self.lower_expr(coll)?;
            let buf = self.read_field(fv, crate::names::ustr("buf"));
            let len = self.read_field(fv, crate::names::ustr("len"));
            return Ok((kind, buf, len));
        }
        let kind = self.coll_kind(coll)?;
        let arr_v = self.lower_expr(coll)?;
        let len_v = self.emit_cur(Inst::Len { base: arr_v, kind });
        Ok((kind, arr_v, len_v))
    }

    /// `continue` jumps to the latch, so the index still increments.
    pub(crate) fn lower_for_array(
        &mut self,
        vars: BindingPattern,
        iter: Idx<Expr>,
        body: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        self.lower_counted_loop(iter, body, None, |s, _, repr| {
            let ty = s.loop_binder_ty(&vars, None);
            let repr = s.loop_element_snapshot(ty, repr)?;
            let elem = s.pack(repr)?;
            s.bind_for_pattern(&vars, elem)?;
            Ok(())
        })
    }

    /// Snapshot a boxed value before the loop body can mutate its source element.
    fn loop_element_snapshot(
        &mut self,
        ty: Option<std::borrow::Cow<'a, MonoTy>>,
        repr: ValueRepr,
    ) -> Result<ValueRepr, CompilerError> {
        match (repr, ty) {
            (ValueRepr::Scalar(v), Some(ty)) => self.copied_value_repr(&ty, v),
            (repr, _) => Ok(repr),
        }
    }

    fn loop_binder_ty(
        &self,
        vars: &BindingPattern,
        component: Option<usize>,
    ) -> Option<std::borrow::Cow<'a, MonoTy>> {
        let BindingPattern::Leaf(b) = vars else {
            return None;
        };
        if self.binding_is_ref(*b) {
            return None;
        }
        let ty = self.binding_ty(*b).and_then(|ty| self.concrete_ty_id(ty))?;
        match component {
            None => Some(std::borrow::Cow::Borrowed(ty)),
            Some(i) => match ty {
                MonoTy::Tuple(parts) => parts.get(i).map(std::borrow::Cow::Borrowed),
                _ => None,
            },
        }
    }

    pub(crate) fn bind_loop_element_repr(
        &mut self,
        vars: &BindingPattern,
        repr: ValueRepr,
    ) -> Result<(), CompilerError> {
        if let BindingPattern::Leaf(b) = vars
            && let Some(shape) = self.whole_tuple_binding(*b)?
        {
            let slots = self.tuple_repr_to_slots(repr, &shape)?;
            self.bind_tuple_slots(*b, &slots, shape);
            return Ok(());
        }
        if let BindingPattern::Leaf(b) = vars {
            if let Some(shape) = self
                .binding_ty(*b)
                .map(|t| self.nobox_shape_of_ty(t))
                .transpose()?
                .flatten()
            {
                let mono = self
                    .binding_ty(*b)
                    .and_then(|ty| self.concrete_ty_id(ty))
                    .ok_or_else(|| self.gap_here("loop binding has no concrete representation"))?;
                let representation = self.concrete_rep_id(mono, RepUse::Value, false, true)?;
                let bundle = ValueBundle::new(representation, repr);
                let leaves = self.repr_to_leaves_bundle_for_shape(&bundle, &shape)?;
                self.bind_leaves(*b, &leaves, shape)?;
                return Ok(());
            }
        }
        let value = self.pack(repr)?;
        self.bind_for_pattern(vars, value)?;
        Ok(())
    }

    pub(crate) fn bind_for_pattern(
        &mut self,
        vars: &BindingPattern,
        value: Idx<Inst>,
    ) -> Result<(), CompilerError> {
        match vars {
            BindingPattern::Leaf(b) => {
                let cell = self.new_binding_cell(
                    *b,
                    self.body.binding_arena[*b].name,
                    self.binding_ty(*b),
                    crate::names::ustr(""),
                );
                if self.binding_is_nonnull_value_struct(*b) {
                    self.f.mark_nonnull_value_struct(cell);
                }
                self.store_cell(cell, value);
                self.binds.insert(raw(*b), cell);
            }
            BindingPattern::Tuple(subs) => {
                // Destructuring a fresh `MakeTuple` binds its operands directly, with no
                // table allocation; this keeps `let (ok, x) = next(it)` zero-alloc.
                if let Inst::MakeTuple(vs) = &self.f.insts[value]
                    && vs.len() == subs.len()
                {
                    let vs = vs.clone();
                    for (sub, v) in subs.iter().zip(vs) {
                        self.bind_for_pattern(sub, v)?;
                    }
                    return Ok(());
                }
                for (i, sub) in subs.iter().enumerate() {
                    let elem = self.emit_cur(Inst::TupleIndex {
                        base: value,
                        index: i,
                    });
                    // The tuple box can be shared, so a value element the binding writes through
                    // is its own copy.
                    let elem = match sub {
                        BindingPattern::Leaf(b) if !self.binding_stays_unwritten(*b) => {
                            match self.binding_ty(*b).and_then(|ty| self.concrete_ty_id(ty)) {
                                Some(ty) => self.copied_value_box(ty, elem)?,
                                None => elem,
                            }
                        }
                        _ => elem,
                    };
                    self.bind_for_pattern(sub, elem)?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn bind_pattern_slot(
        &mut self,
        vars: &BindingPattern,
        slot: &RepLayout,
        values: &[Idx<Inst>],
        idx: &mut usize,
    ) -> Result<(), CompilerError> {
        match (vars, slot) {
            (BindingPattern::Leaf(b), RepLayout::Leaves(shape)) => {
                let n = shape.leaf_count();
                let leaves = values[*idx..*idx + n].to_vec();
                *idx += n;
                self.bind_leaves(*b, &leaves, shape.clone())?;
            }
            (BindingPattern::Tuple(subs), RepLayout::Tuple(inner))
                if subs.len() == inner.slots.len() =>
            {
                for (sub, &s) in subs.iter().zip(&inner.slots) {
                    self.bind_pattern_slot(sub, self.scx.rep_arena().layout(s), values, idx)?;
                }
            }
            // One name bound to a splatting tuple holds its slot cells: packing it would
            // pack the tuple's `nobox` parts, which no later pass can undo.
            (BindingPattern::Leaf(b), RepLayout::Tuple(inner))
                if inner.splats(self.scx.rep_arena()) =>
            {
                let n = inner.slot_count(self.scx.rep_arena());
                let slots = values[*idx..*idx + n].to_vec();
                *idx += n;
                self.bind_tuple_slots(*b, &slots, inner.clone());
            }
            (vars, slot) => {
                let r = self.slot_repr(slot, values, idx);
                let v = self.pack(r)?;
                self.bind_for_pattern(vars, v)?;
            }
        }
        Ok(())
    }

    /// Bind `b` to a value's leaves: one cell per leaf, registered in
    /// `group_binds`.
    pub(crate) fn bind_leaves(
        &mut self,
        b: Idx<BindingInfo>,
        leaves: &[Idx<Inst>],
        shape: ValueShape,
    ) -> Result<(), CompilerError> {
        let bname = self.body.binding_arena[b].name;
        if let Some(cells) = self.new_flat_enum_cells(b, bname, &shape)? {
            if cells.len() != leaves.len() {
                return Err(self.gap_here(format!(
                    "flat enum {} has {} binding cells for {} leaves",
                    shape.name,
                    cells.len(),
                    leaves.len()
                )));
            }
            for (cell, &value) in cells.iter().zip(leaves) {
                self.store_cell(*cell, value);
            }
            let cells = cells.into_iter().map(GroupLeaf::Cell).collect();
            self.group_binds.insert(raw(b), (cells, shape));
            return Ok(());
        }
        if self.binding_stays_unwritten(b) {
            self.f.value_lets += leaves.len();
            self.f.value_leaves += leaves.len();
            let values = leaves.iter().map(|&v| GroupLeaf::Value(v)).collect();
            self.group_binds.insert(raw(b), (values, shape));
            return Ok(());
        }
        let cells: Vec<GroupLeaf> = leaves
            .iter()
            .enumerate()
            .map(|(li, &value)| {
                let c = self.new_binding_cell(
                    b,
                    crate::names::indexed(bname.as_str(), "__", li),
                    None,
                    crate::names::ustr(li.to_string().as_str()),
                );
                self.store_cell(c, value);
                GroupLeaf::Cell(c)
            })
            .collect();
        self.group_binds.insert(raw(b), (cells, shape));
        Ok(())
    }

    pub(crate) fn value_shape_leaf_monos(&self, shape: &ValueShape) -> Option<Vec<MonoTy>> {
        if shape.nullable {
            return None;
        }
        let fields = value_struct_field_tys(self.ws, shape.name)?;
        if fields.len() != shape.fields.len() {
            return None;
        }
        let mut out = Vec::new();
        for ((shape_name, field_shape), (field_name, field_ty)) in shape.fields.iter().zip(fields) {
            if shape_name != &field_name {
                return None;
            }
            match field_shape {
                FieldShape::Prim => {
                    out.push(self.concrete_ty_id(field_ty).cloned().or_else(|| {
                        crate::monoty::convert_decl(self.ws, field_ty, &SpecSubst::default()).ok()
                    })?)
                }
                FieldShape::Nested(child) => out.extend(self.value_shape_leaf_monos(child)?),
            }
        }
        Some(out)
    }

    /// The slot layout a name takes when it binds a whole splatting tuple: it
    /// holds the slots in cells, never one box.
    pub(crate) fn whole_tuple_binding(
        &self,
        b: Idx<BindingInfo>,
    ) -> Result<Option<TupleLayout>, CompilerError> {
        if self.binding_is_ref(b) {
            return Ok(None);
        }
        match self.binding_ty(b).and_then(|ty| self.concrete_ty_id(ty)) {
            Some(ty @ MonoTy::Tuple(_)) => Ok(self
                .concrete_rep_tuple(ty, RepUse::Return, true, false)?
                .filter(|shape| shape.splats(self.scx.rep_arena()))),
            _ => Ok(None),
        }
    }

    pub(crate) fn bind_tuple_slots(
        &mut self,
        b: Idx<BindingInfo>,
        slots: &[Idx<Inst>],
        shape: TupleLayout,
    ) {
        if let Some(boxed) = self.flat_box_of_slots(slots)
            && self.binding_stays_unwritten(b)
        {
            self.tuple_bind_boxes.insert(raw(b), boxed);
        }
        let bname = self.body.binding_arena[b].name;
        let cells: Vec<Idx<Cell>> = slots
            .iter()
            .enumerate()
            .map(|(si, &value)| {
                let c = self.new_binding_cell(
                    b,
                    crate::names::indexed(bname.as_str(), "__", si),
                    None,
                    crate::names::ustr(si.to_string().as_str()),
                );
                self.store_cell(c, value);
                c
            })
            .collect();
        self.tuple_binds.insert(raw(b), (cells, shape));
    }

    /// The elements arrive as flat slots, so an element that is a `nobox` value
    /// binds its leaves directly.
    pub(crate) fn bind_tuple_repr(
        &mut self,
        vars: &BindingPattern,
        shape: &TupleLayout,
        r: ValueRepr,
    ) -> Result<(), CompilerError> {
        let values = self.tuple_repr_to_slots(r, shape)?;
        let mut idx = 0;
        match vars {
            BindingPattern::Tuple(subs) if subs.len() == shape.slots.len() => {
                for (sub, &slot) in subs.iter().zip(&shape.slots) {
                    self.bind_pattern_slot(
                        sub,
                        self.scx.rep_arena().layout(slot),
                        &values,
                        &mut idx,
                    )?;
                }
            }
            BindingPattern::Leaf(b) if !shape.nullable => {
                self.bind_tuple_slots(*b, &values, shape.clone());
            }
            _ => {
                let r = self.tuple_from_slots(shape, &values, &mut idx);
                let v = self.pack(r)?;
                self.bind_for_pattern(vars, v)?;
            }
        }
        Ok(())
    }
}
