//! Loops and the bindings they introduce: `while`, the `for` forms (user
//! iterator, arena, map, enumerate, array) and the pattern binding that lands
//! each element -- including the per-iteration tuple that gets scalarized.
use super::*;

impl<'a> Builder<'a> {
    /// The innermost targets and the defer depth left by this exit.
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
            // `while true`: no branch to exit -- exit is reachable only via
            // `break`, which is exactly what `used_break_targets` records.
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
        if diverges {
            // Unreachable: give the dead exit a harmless terminator so every
            // block ends terminated even though nothing jumps here.
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
        // User iterator protocol (bone recorded the resolution for this For).
        if let Some(info) = self
            .inf
            .and_then(|i| i.for_iter_protocol.get(&iter).cloned())
        {
            // A plain `[T]` walks its storage directly: the stdlib `VecIter`
            // would only inline back into this counted loop. Shader lowering
            // keeps the protocol.
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
            // Lua keeps its counted arena walk; native rows and JS use the
            // recorded iterator.
            if !self.scx.row_value_containers && self.scx.target() == bone_core::hir::FnBackend::Lua
            {
                if let Some(collection) = enumerated_collection.filter(|&c| self.iter_is_arena(c)) {
                    return self.lower_for_arena(&vars, collection, body, true);
                }
                // The arena itself as the head takes the same walk: `next` of
                // the stdlib pair iterator skips free slots in an inner loop,
                // which inlines to a flag-driven goto loop no pass turns back
                // into a counted one.
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
            // Direct `for .. in arena` arrives with `for_iter_protocol`
            // populated (bone routes Arena to the stdlib ArenaPairIter).
            return Err(gap(
                "for over Arena must route through the iterator protocol (for_iter_protocol missing)",
            ));
        }
        // FlatVec iterates its `buf` storage bounded by the `len` field;
        // `lower_for_array` owns that special-case. Route here so `coll_kind`
        // stays closed over Vec/Map/NativeBuffer (it can't classify FlatVec).
        if self.expr_is_flat_vec(iter) {
            return self.lower_for_array(vars, iter, body);
        }
        match self.coll_kind(iter)? {
            // Array iteration normally uses the resolved VecIter witness.
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
            // Synthetic: a leaf column and a leaf VIEW are both minted during
            // element lowering and are never user types, so neither can be a
            // `for` head.
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
        // Use the iterator head as the synthesized call site.
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
                // Resolved BEFORE the arguments are shaped, for the same reason
                // the implicit-coercion form does it: the splat must match the
                // body the call targets.
                let witness = if info.iter.is_some() {
                    self.with_site_op(crate::site::SiteOp::ForIter, |b| {
                        b.concrete_callee_for_site(iter_expr)
                    })?
                    .ok_or_else(|| gap("for-iter has no concrete callee"))?
                } else {
                    crate::instance::Instance::base(iter_ref)
                };
                let args = self.iter_witness_args(&witness, bundle)?;
                // The `for` head's `iter` witness, not the head expression's
                // own call: both live at one expression and share one span, so
                // the operation is what keeps them distinguishable.
                let call = self.with_site_op(crate::site::SiteOp::ForIter, |b| {
                    b.emit_cur(Inst::Call {
                        func: iter_ref,
                        args,
                    })
                });
                // DELIVERY, from the same `IterWitness::generics` the identity
                // record below reads. The witness is an ordinary call to a
                // generic `iter`, so under instance-owned emission it must name
                // a concrete instance like any other.
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

        // next returns `(bool, E)` / `(bool, A, B, ...)`. The shape is read under
        // the concrete callee the emitted call will carry, not the caller's own:
        // the witness result plan states how many values come back, so the two
        // lowering sides agree without re-deriving the shape.
        let concrete_next = if matches!(info.next, BoundNext::Concrete { .. }) {
            self.with_site_op(crate::site::SiteOp::ForNext(Box::default()), |b| {
                b.concrete_callee_for_site(iter_expr)
            })?
            .ok_or_else(|| gap("for-next has no concrete callee"))?
            .into()
        } else {
            None
        };
        // Resolve the yielded element layout in the current specialization.
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

        // header: (ok, elem..) = next(it); branch on ok.
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
                // Use the iterator witness for the synthesized call's instance.
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
                // The same `for`-protocol `next` obligation its concrete sibling
                // above carries, at the same expression -- so it takes the same
                // operation. Without it the witness is recorded on one arm and
                // invisible on the other.
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

        // body: bind the pattern to the element slots. One element binds
        // directly (its leaves, if it splats); multiple (`next -> (bool, A, B)`)
        // are a tuple `bind_pattern_slot` destructures against a matching tuple
        // pattern (no alloc).
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

    /// Whether `e` is a direct call of a `co fn`, i.e. a fresh coroutine.
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

    /// The variant names of `e`'s enum type (empty if `e` is not an enum) — used
    /// to recognize an unqualified nullary-variant pattern at a match root.
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

    /// The enum NAME of a scrutinee whose static type is an enum (`None` else) --
    /// threaded into `match_pattern`/`test_tag` so a payload-free enum's tag test
    /// can int-compare the ordinal instead of reading `.__variant`.
    pub(crate) fn scrut_enum_name(&self, e: Idx<Expr>) -> Option<Ustr> {
        if let Some(ty) = self.concrete_expr_ty(e) {
            let name = ty.peel_nullable().0.nominal()?;
            return self.ws.enums_by_name.contains_key(&name).then_some(name);
        }
        None
    }

    /// Whether `e`'s inferred type is a transparent newtype nominal.
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

    /// Whether `iter`'s inferred type is an `Arena<T>` nominal.
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
        // Resolve the (optional enum index var, the entity/value pattern).
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
        // Occupancy below is the liveness check. Generations only supply the
        // handle's payload, and an unused row needs no snapshot of the data.
        let entity_used = self.arena_component_used(&pat, 0, body)?;
        let data_used = self.arena_component_used(&pat, 1, body)?;
        let enum_var = enum_var.filter(|&b| self.loop_pattern_used(&BindingPattern::Leaf(b)));

        // The element type, when it is column-eligible: under bookkeeping SoA the
        // arena's `data` is an ordinary `[T]`, so at an all-scalar value `T` it is
        // COLUMN-stored and a bare `Index` would read a table that isn't there.
        let column_elem = self.arena_column_elem_of(iter)?;

        // An Arena is a weak handle, so the columns come from its registry entry.
        // Resolved ONCE here, outside the counted loop: Scene ownership must not
        // put a registry lookup in front of every row. A retired handle resolves
        // to an empty store, which the `i < n` header exits immediately.
        // This counted template consumes the collection, not its inferred iter(arg) wrapper.
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

        // header: i < n ?
        self.cur = header;
        let i = self.emit_cur(Inst::Load(i_cell));
        let c = self.emit_cur(Inst::IntLt(i, n));
        self.branch_to(c, body_bb, exit);

        // body: nothing to skip before the occupancy read -- the walk is bounded
        // by the occupancy column's own length, and a `[bool]` column has no
        // holes, so the old nil test had nothing left to catch.
        self.cur = body_bb;
        let idx = self.emit_cur(Inst::Load(i_cell));
        self.f.set_term(self.cur, Term::Goto(occ_bb));

        // occ: skip unoccupied.
        self.cur = occ_bb;
        let occ = self.read_index(occupied, idx, occupied_kind);
        self.branch_to(occ, do_bb, latch);

        // do: build Entity, bind, run body.
        self.cur = do_bb;
        // Reconstruct an Entity's slot from the counter and read its generation.
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
        // A column element is rebuilt from its leaves here. For an ordinary value
        // struct SROA folds the field reads back to the leaf cells, so the pack
        // costs nothing; for a `nobox` element it is an implicit materialization
        // and the verifier rejects it by name rather than emitting a quiet box.
        let data = if let Some(data) = data {
            Some(match column_elem {
                Some(elem) => {
                    let repr = self.column_element_repr(data, idx, elem)?;
                    self.pack(repr.into_value())?
                }
                None => {
                    // The stored element stays the entry's; the binder gets its own
                    // copy, as `ArenaPairIter.next` gives the plain loop.
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
                    self.bind_for_pattern(&subs[0], entity);
                }
                if let Some(data) = data {
                    self.bind_for_pattern(&subs[1], data);
                }
            }
            BindingPattern::Leaf(_) if entity_used || data_used => {
                // Keep the pair's positions for projections. A whole-pair use
                // demands both components, so no source read sees a placeholder.
                let entity = entity.unwrap_or_else(|| self.emit_cur(Inst::Const(Lit::Nil)));
                let data = data.unwrap_or_else(|| self.emit_cur(Inst::Const(Lit::Nil)));
                let tup = self.emit_cur(Inst::MakeTuple(vec![entity, data]));
                self.bind_for_pattern(&pat, tup);
            }
            _ => {}
        }
        // enumerate counter: bind to the current count, then bump (occupied only).
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

        // latch: i += 1.
        self.cur = latch;
        self.increment_cell(i_cell);
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = exit;
        Ok(())
    }

    /// `for (k, v) in m` / `for kv in m`: Lua stateless `next` iteration. The key
    /// cell is the cursor; a nil key after `MapNext` ends the loop. Under
    /// `enumerate(m)`, `counter` binds a 0-based index stepped in the latch, so
    /// `continue` still steps it.
    pub(crate) fn lower_for_map(
        &mut self,
        counter: Option<Idx<BindingInfo>>,
        vars: &BindingPattern,
        iter: Idx<Expr>,
        body: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        // A column-stored Map iterates its SPINE: the walk yields (key, slot),
        // and the value is read leaf-wise out of `data` at that slot. No element
        // table is touched, because none exists.
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

    // An unused loop binding needs no snapshot. In particular, a column read
    // includes a bounds check, which cleanup must otherwise preserve even
    // when none of its leaves are used. Count writes as uses too: lowering a
    // later assignment still needs the binding's cell.
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

    /// `col_value` is the already-built element for a COLUMN-stored Map, whose
    /// value is leaves at a slot rather than a cell. `None` keeps the boxed
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
                    self.bind_for_pattern(&subs[0], kv);
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
                self.bind_for_pattern(&subs[1], vv);
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
                let pair = self.emit_cur(Inst::MakeTuple(vec![kv, vv]));
                self.bind_for_pattern(vars, pair);
                Ok(())
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
        // Split `(i, xpat)` vs a single binding to the whole `(i, element)` tuple.
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
            // A tuple element whose `nobox` parts ride its box as leaves
            // destructures from the box's slots, not from its plain indices.
            if let Some(pat) = elem_pat {
                if let (BindingPattern::Tuple(_), ValueRepr::Scalar(base)) = (pat, &repr)
                    && let Some(MonoTy::App { params, .. }) = s.concrete_expr_ty(coll)
                    && let [elem @ MonoTy::Tuple(_)] = params.as_slice()
                    && let Some(shape) = s
                        .concrete_rep_tuple(elem, RepUse::Return, true, false)?
                        .filter(|shape| shape.splats(self.scx.rep_arena()))
                {
                    let base = *base;
                    let slots: Vec<Idx<Inst>> = (0..shape.slot_count(self.scx.rep_arena()))
                        .map(|index| s.emit_cur(Inst::TupleIndex { base, index }))
                        .collect();
                    let tuple = s.tuple_from_slots(&shape, &slots, &mut 0);
                    s.bind_tuple_repr(pat, &shape, tuple)?;
                } else {
                    s.bind_loop_element_repr(pat, repr)?;
                }
            } else {
                let elem = s.pack(repr)?;
                let pair = s.emit_cur(Inst::MakeTuple(vec![i_now, elem]));
                s.bind_for_pattern(vars, pair);
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
        // The counter cell is named after the index var in split mode; an internal
        // cell in single-binding mode (the index is folded into the tuple).
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

    /// The storage a counted loop indexes, its container kind and its live
    /// length. A FlatVec is bounded by its `len` field (not the buffer
    /// capacity) and indexes its `buf` NativeBuffer.
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

    /// `for x in coll { body }` over an indexable collection: hoist the
    /// collection + its length, then `i = 0; while i < len { x = coll[i]; ...; i++ }`.
    /// The index is a cell; `continue` -> latch (still increments).
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
            s.bind_for_pattern(&vars, elem);
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

    /// The type a loop pattern binds an element as: a plain binder's own type,
    /// or part `component` of a single binder's tuple (`for pair in
    /// xs.enumerate()`). A destructuring pattern or a `ref` binder has none.
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
        self.bind_for_pattern(vars, value);
        Ok(())
    }

    /// Bind a for-loop pattern to an element value: `Leaf` -> one cell; `Tuple`
    /// -> destructure via `TupleIndex` into a cell per element.
    pub(crate) fn bind_for_pattern(&mut self, vars: &BindingPattern, value: Idx<Inst>) {
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
                // Fast path: destructuring a fresh `MakeTuple` (a tuple literal or a
                // multi-return call packed as `MakeTuple(loads)`) binds directly to
                // its operands -- no table alloc, no `TupleIndex`. This is what makes
                // `let (ok, x) = next(it)` zero-alloc.
                if let Inst::MakeTuple(vs) = &self.f.insts[value] {
                    if vs.len() == subs.len() {
                        let vs = vs.clone();
                        for (sub, v) in subs.iter().zip(vs) {
                            self.bind_for_pattern(sub, v);
                        }
                        self.mark_destructured(vars);
                        return;
                    }
                }
                for (i, sub) in subs.iter().enumerate() {
                    let elem = self.emit_cur(Inst::TupleIndex {
                        base: value,
                        index: i,
                    });
                    self.bind_for_pattern(sub, elem);
                }
                self.mark_destructured(vars);
            }
        }
    }

    /// Charge every cell a destructuring pattern bound to the destructure, for
    /// the cleanup Load/Store census ([`crate::mir::CellProducer`]).
    fn mark_destructured(&mut self, vars: &BindingPattern) {
        match vars {
            BindingPattern::Leaf(b) => {
                let key = raw(*b);
                let single = self.binds.get(&key).copied();
                let group = self.group_binds.get(&key).map(|(leaves, _)| {
                    leaves
                        .iter()
                        .filter_map(|leaf| match leaf {
                            GroupLeaf::Cell(cell) => Some(*cell),
                            GroupLeaf::Value(_) => None,
                        })
                        .collect::<Vec<_>>()
                });
                let tuple = self.tuple_binds.get(&key).map(|(cells, _)| cells.clone());
                for cell in single
                    .into_iter()
                    .chain(group.into_iter().flatten())
                    .chain(tuple.into_iter().flatten())
                {
                    self.f.cells[cell].producer = crate::mir::CellProducer::Destructure;
                }
            }
            BindingPattern::Tuple(subs) => {
                for sub in subs {
                    self.mark_destructured(sub);
                }
            }
        }
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
            // ONE name bound to a splatting tuple (`let (ok, x) = next(it)` in a
            // combinator over a `zip`/`enumerate` element). It holds its SLOT
            // cells, like a value binding holds its leaves: packing it here would
            // pack the tuple's `nobox` parts with it, which is the guarantee's
            // violation and not something a later pass can undo.
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
                self.bind_for_pattern(vars, v);
            }
        }
        Ok(())
    }

    /// Bind `b` to a value's LEAVES: one cell per leaf, registered in
    /// `group_binds`. The single form for "this binding is a scalarized value",
    /// shared by `if let` and the splatting tuple destructures.
    pub(crate) fn bind_leaves(
        &mut self,
        b: Idx<BindingInfo>,
        leaves: &[Idx<Inst>],
        shape: ValueShape,
    ) -> Result<(), CompilerError> {
        let bname = self.body.binding_arena[b].name;
        let owner = self.binding_ty(b).and_then(|ty| self.concrete_ty_id(ty));
        if let Some(cells) = self.new_flat_enum_cells(b, bname, &shape, owner)? {
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
        let leaf_tys = self.value_shape_leaf_monos(&shape).map(|tys| {
            tys.into_iter()
                .map(|ty| self.scx.interner().intern(ty))
                .collect::<Vec<_>>()
        });
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
                self.f.cells[c].native_ty = leaf_tys.as_ref().and_then(|tys| tys.get(li).copied());
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

    /// Bind `b` to a splatting tuple's SLOTS: one cell per slot, registered in
    /// [`Builder::tuple_binds`]. The tuple twin of [`Self::bind_leaves`], and the
    /// same rule -- a read rebuilds the unboxed repr from the cells instead of
    /// the binding ever holding a packed tuple.
    pub(crate) fn bind_tuple_slots(
        &mut self,
        b: Idx<BindingInfo>,
        slots: &[Idx<Inst>],
        shape: TupleLayout,
    ) {
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
                self.f.cells[c].producer = crate::mir::CellProducer::TupleSlot;
                self.store_cell(c, value);
                c
            })
            .collect();
        self.tuple_binds.insert(raw(b), (cells, shape));
    }

    /// Destructure a splatting tuple into `vars`. The elements arrive as flat
    /// slots, so an element that is a `nobox` value binds its leaves directly.
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
                self.mark_destructured(vars);
            }
            _ => {
                let r = self.tuple_from_slots(shape, &values, &mut idx);
                let v = self.pack(r)?;
                self.bind_for_pattern(vars, v);
            }
        }
        Ok(())
    }
}
