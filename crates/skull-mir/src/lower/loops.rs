//! Loops and the bindings they introduce: `while`, the `for` forms (user
//! iterator, arena, map, enumerate, array) and the pattern binding that lands
//! each element -- including the per-iteration tuple that gets scalarized.
use super::*;

impl Builder<'_> {
    /// The innermost `(continue_target, break_target, defer depth)`.
    pub(crate) fn loop_target(
        &self,
        what: &str,
    ) -> Result<(Idx<Block>, Idx<Block>, usize), CompilerError> {
        self.loop_targets
            .last()
            .copied()
            .ok_or_else(|| gap(format!("`{what}` outside a loop")))
    }

    /// `while cond { body }`: header (test) -> body -> back-edge to header; exit
    /// on false. `continue` -> header, `break` -> exit.
    /// Returns true when the statement TERMINATES the enclosing list: a
    /// `while true` (constant cond) whose exit no `break` ever targeted -- its
    /// fall-through is unreachable, so the caller must not lower trailing
    /// statements into it (and a co fn body ending in one is not a fall-off).
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
        self.loop_targets.push((header, exit, self.defers.len()));
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
            // Lua keeps its counted arena walk; native rows and JS use the
            // recorded iterator. The walk's synthesized `__arena_store_or_empty`
            // call has no instance-graph site, which only JS checks strictly.
            // Adding that site would let JS take the walk too.
            if !self.scx.row_value_containers
                && self.scx.target() == bone_core::hir::FnBackend::Lua
                && let Some(collection) = enumerated_collection.filter(|&c| self.iter_is_arena(c))
            {
                return self.lower_for_arena(&vars, collection, body, true);
            }
            return self.lower_for_user_iter(&vars, iter, body, &info);
        }
        if let Some(collection) = enumerated_collection {
            if self.iter_is_arena(collection) {
                return Err(gap("enumerated Arena must have an iterator protocol"));
            }
            return self.lower_for_enumerate(&vars, collection, body);
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
            // `for x in [T]` always arrives with `for_iter_protocol` populated
            // (bone routes Array to the stdlib VecIter protocol); reaching the
            // dispatch means bone and skull disagree -- reject, don't emit a
            // second lowering. `enumerate(vec)` keeps its own path above.
            // Either Vec flavor: columns change storage, not the iteration
            // protocol -- `VecIter.next` reads `it.vec[it.i]`, which is an
            // `Index` and picks up the column kind on its own.
            ContainerKind::Vec
            | ContainerKind::VecValueColumns(_)
            | ContainerKind::VecValueStrided(_) => Err(gap(
                "for over [T] must route through the iterator protocol (for_iter_protocol missing)",
            )),
            ContainerKind::NativeBuffer | ContainerKind::NativeBufferValue(_) => {
                self.lower_for_array(vars, iter, body)
            }
            ContainerKind::Map => self.lower_for_map(&vars, iter, body),
            // Synthetic: a leaf column and a leaf VIEW are both minted during
            // element lowering and are never user types, so neither can be a
            // `for` head.
            ContainerKind::VecColumn => Err(gap("for over a synthetic Vec leaf column")),
            ContainerKind::VecStrideSlot => Err(gap("for over a synthetic Vec strided slot")),
            ContainerKind::NativeBufferLeaf => {
                Err(gap("for over a synthetic NativeBuffer leaf view"))
            }
        }
    }

    /// `for <pat> in <coll>` over a user type via the iter/next protocol (bone
    /// recorded the resolution). Desugars to:
    ///   it = iter(coll)        -- or `it = coll` when iter is identity
    ///   loop { let (ok, <pat>) = next(it); if !ok break; <body> }
    /// `next` returns a tuple, so slice-1's multi-return ABI makes the per-element
    /// `next` call zero-alloc (a `CallMulti` into `[ok, elem..]` dests). `it` lives
    /// in a cell so `next` mutates the same cursor each turn. `continue` re-enters
    /// the header (runs `next` again); `break` exits.
    ///
    /// A `Concrete` next with a non-empty bound tree is a combinator-chain root:
    /// the tree is recorded against the emitted `CallMulti` so the inliner can
    /// substitute the placeholders inside the spliced callee. A `CallerGeneric`
    /// next (a `for` over a bounded generic inside a bounded body) emits the
    /// placeholder [`Inst::CallBoundMulti`] directly.
    pub(crate) fn lower_for_user_iter(
        &mut self,
        vars: &BindingPattern,
        iter_expr: Idx<Expr>,
        body: Idx<Expr>,
        info: &ForIterInfo,
    ) -> Result<(), CompilerError> {
        // The `for` protocol's witnesses belong to the HEAD expression, which is
        // the key `inf.for_iter_protocol` -- and therefore the instance graph --
        // records them under. The enclosing cursor names the whole construct
        // (and is absent entirely when the `for` is a statement), so both halves
        // are set explicitly here rather than inherited from a cursor that names
        // the wrong expression. Same reason `lower_expr` sets them for an
        // implicit-iter coercion.
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
        let it_cell = self.f.new_cell(Some(Ustr::from("__it")));
        self.emit_cur(Inst::Store {
            cell: it_cell,
            value: it_val,
        });
        let owned_flag = self.own_loop_coroutine(iter_expr, it_cell);

        // next returns `(bool, E)` / `(bool, A, B, ...)`. The shape is read under
        // the concrete callee the emitted call will carry, not the caller's own:
        // the witness result plan states how many values come back, so the two
        // lowering sides agree without re-deriving the shape.
        let concrete_next = if matches!(info.next, BoundNext::Concrete { .. }) {
            self.with_site_op(crate::site::SiteOp::ForNext(Vec::new()), |b| {
                b.concrete_callee_for_site(iter_expr)
            })?
            .ok_or_else(|| gap("for-next has no concrete callee"))?
            .into()
        } else {
            None
        };
        // A placeholder next is `(bool, E)` by bound conformance, but `E`'s SHAPE
        // is this body's own specialization to answer -- the concrete callee is
        // only the source of how many values come back, and the two agree because
        // both read the same instantiation. Resolving it here is what splits a
        // presence-carrying element into {presence cell, leaf cells} at the point
        // the shape is known; leaving it opaque forces the splice to rebuild an
        // aggregate, and a presence leaf has no field to rebuild into.
        let ret_shape = match &info.next {
            BoundNext::Concrete { .. } => self
                .callee_abi_plan_for_instance(
                    concrete_next
                        .as_ref()
                        .ok_or_else(|| gap("for-iter `next` has no concrete callee"))?,
                )?
                .ret_tuple()
                .ok_or_else(|| gap("for-iter `next` must return a tuple"))?,
            BoundNext::CallerGeneric(gidx) => self.bound_next_tuple_shape(*gidx)?,
        };
        if ret_shape.slots.len() < 2 {
            return Err(gap("for-iter `next` must return `(bool, E)`"));
        }
        let n = ret_shape.slot_count();

        let header = self.f.new_block();
        let body_bb = self.f.new_block();
        let exit = self.f.new_block();
        self.f.for_iter_headers.push(header);
        self.f.set_term(self.cur, Term::Goto(header));

        // header: (ok, elem..) = next(it); branch on ok.
        self.cur = header;
        let it_load = self.emit_cur(Inst::Load(it_cell));
        let dests: Vec<Idx<Cell>> = (0..n).map(|_| self.f.new_cell(None)).collect();
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
                // The synthesized `next` call has no expr to key
                // `call_generic_bindings` by, so its instantiation comes from
                // bone's recorded `next_generics`. Without this the inliner
                // splices the SHARED generic snapshot, whose element access was
                // lowered against the boxed representation -- which reads a
                // `_data` that column storage does not have.
                // The `for` protocol's `next` witness -- the sibling of the
                // `ForIter` above at the SAME expression.
                let call = self.with_site_op(crate::site::SiteOp::ForNext(Vec::new()), |b| {
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
                let call = self.with_site_op(crate::site::SiteOp::ForNext(Vec::new()), |b| {
                    b.emit_cur(Inst::CallBoundMulti {
                        dests: dests.clone(),
                        generic_idx: *gidx as u32,
                        args: vec![it_load],
                    })
                });
                if ret_shape.splats() {
                    self.f.bound_next_split.insert(call);
                }
            }
        }
        let ok = self.emit_cur(Inst::Load(dests[0]));
        self.f.set_term(
            self.cur,
            Term::Branch {
                cond: ok,
                then_bb: body_bb,
                else_bb: exit,
            },
        );

        // body: bind the pattern to the element slots. One element binds
        // directly (its leaves, if it splats); multiple (`next -> (bool, A, B)`)
        // are a tuple `bind_pattern_slot` destructures against a matching tuple
        // pattern (no alloc).
        self.cur = body_bb;
        let loads: Vec<Idx<Inst>> = dests[1..]
            .iter()
            .map(|c| self.emit_cur(Inst::Load(*c)))
            .collect();
        let elem_slot = match &ret_shape.slots[1..] {
            [one] => one.clone(),
            rest => RetSlot::Nested(TupleRetShape {
                slots: rest.to_vec(),
                nullable: false,
            }),
        };
        let mut idx = 0;
        self.bind_pattern_slot(vars, &elem_slot, &loads, &mut idx)?;
        self.loop_targets.push((header, exit, self.defers.len()));
        self.lower_stmt_expr(body)?;
        self.loop_targets.pop();
        if self.is_open() {
            self.f.set_term(self.cur, Term::Goto(header));
        }
        self.cur = exit;
        if let Some(flag) = owned_flag {
            let off = self.emit_cur(Inst::Const(Lit::Bool(false)));
            self.emit_cur(Inst::Store {
                cell: flag,
                value: off,
            });
        }
        Ok(())
    }

    /// A co fn `for` whose head constructs a coroutine (`for v in gen()`)
    /// holds the only reference to it, so a kill or error of the enclosing
    /// coroutine while it is suspended in the loop must take the generator
    /// down too, or the generator's defers and `on_killed` never run. That is
    /// registered as a terminal cleanup guarded by a frame flag, which is
    /// cleared on every path out of the loop. Leaving the loop (break,
    /// exhaustion) does not kill: a plain fn does not either. A head that
    /// names an existing handle is not owned by the loop and is left alone.
    fn own_loop_coroutine(
        &mut self,
        iter_expr: Idx<Expr>,
        it_cell: Idx<Cell>,
    ) -> Option<Idx<Cell>> {
        if self.defer_mode != stmt::DeferMode::CoFlag || !self.head_constructs_coroutine(iter_expr)
        {
            return None;
        }
        let flag = self.f.new_cell(Some(Ustr::from("__defer")));
        let on = self.emit_cur(Inst::Const(Lit::Bool(true)));
        self.emit_cur(Inst::Store {
            cell: flag,
            value: on,
        });
        let saved_cur = self.cur;
        let entry = self.f.new_block();
        let b_kill = self.f.new_block();
        let b_done = self.f.new_block();
        self.cur = entry;
        let fv = self.emit_cur(Inst::Load(flag));
        self.f.set_term(
            entry,
            Term::Branch {
                cond: fv,
                then_bb: b_kill,
                else_bb: b_done,
            },
        );
        self.cur = b_kill;
        let off = self.emit_cur(Inst::Const(Lit::Bool(false)));
        self.emit_cur(Inst::Store {
            cell: flag,
            value: off,
        });
        let it = self.emit_cur(Inst::Load(it_cell));
        let kill = self.emit_cur(Inst::Global(Ustr::from(crate::mir::CORO_KILL)));
        self.emit_cur(Inst::CallValue {
            callee: kill,
            args: vec![it],
        });
        self.f.set_term(b_kill, Term::Goto(b_done));
        self.f.set_term(b_done, Term::Return(Vec::new()));
        self.cur = saved_cur;
        self.f.coro_defers.push((flag, entry));
        Some(flag)
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
                return HashSet::new();
            };
            if let Some(&eid) = self.ws.enums_by_name.get(&name) {
                let ed = &self.ws.modules[eid.module.index()].enums[eid.def];
                return ed.variants.iter().map(|v| v.name).collect();
            }
            return HashSet::new();
        }
        HashSet::new()
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

    /// `for (e, v) in arena` / `for item in arena` (optionally `enumerate(arena)`)
    /// over the stdlib `Arena { entries: Vec<ArenaEntry>, .. }`: a numeric walk
    /// over `entries` skipping nil / unoccupied slots, yielding the `(Entity,
    /// data)` pair (two vars bind them apart; one var binds the 2-tuple). Mirrors
    /// rock-codegen's `IterKind::Arena`. `enumerate` binds the FIRST var to a
    /// 0-based counter advancing per yielded (occupied) element.
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
        let single = matches!(pat, BindingPattern::Leaf(_));
        let leaves = match &pat {
            BindingPattern::Tuple(subs) if subs.len() == 2 => None,
            BindingPattern::Leaf(_) => Some(()),
            _ => {
                return Err(gap(
                    "for-over-Arena binds one (item) or two (entity, value)",
                ));
            }
        };
        let _ = leaves;

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
        let scene_id = self.emit_cur(Inst::Field {
            base: arena,
            field: Ustr::from("_scene_id"),
        });
        let arena_id = self.emit_cur(Inst::Field {
            base: arena,
            field: Ustr::from("_arena_id"),
        });
        let store = self.call_spine("__arena_store_or_empty", vec![scene_id, arena_id])?;
        let occupied = self.emit_cur(Inst::Field {
            base: store,
            field: Ustr::from("occupied"),
        });
        let generations = self.emit_cur(Inst::Field {
            base: store,
            field: Ustr::from("generations"),
        });
        let data = self.emit_cur(Inst::Field {
            base: store,
            field: Ustr::from("data"),
        });
        let n = self.emit_cur(Inst::Len {
            base: occupied,
            kind: ContainerKind::Vec,
        });
        let i_cell = self.f.new_cell(None);
        self.f.mark_loop_binding(i_cell);
        let zero = self.emit_cur(Inst::Const(Lit::Int(0)));
        self.emit_cur(Inst::Store {
            cell: i_cell,
            value: zero,
        });
        let enum_cell = enum_var.map(|_| {
            let c = self.f.new_cell(None);
            let z = self.emit_cur(Inst::Const(Lit::Int(0)));
            self.emit_cur(Inst::Store { cell: c, value: z });
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
        self.f.set_term(
            self.cur,
            Term::Branch {
                cond: c,
                then_bb: body_bb,
                else_bb: exit,
            },
        );

        // body: nothing to skip before the occupancy read -- the walk is bounded
        // by the occupancy column's own length, and a `[bool]` column has no
        // holes, so the old nil test had nothing left to catch.
        self.cur = body_bb;
        let idx = self.emit_cur(Inst::Load(i_cell));
        self.f.set_term(self.cur, Term::Goto(occ_bb));

        // occ: skip unoccupied.
        self.cur = occ_bb;
        let occ = self.emit_cur(Inst::Index {
            base: occupied,
            index: idx,
            kind: ContainerKind::Vec,
        });
        self.f.set_term(
            self.cur,
            Term::Branch {
                cond: occ,
                then_bb: do_bb,
                else_bb: latch,
            },
        );

        // do: build Entity, bind, run body.
        self.cur = do_bb;
        // A slot's index IS its position, so the loop counter is the Entity's
        // `_index` and only the generation needs a column read. The built
        // value is an `Entity` whose fields are `_`-prefixed (soft-private).
        // `Entity` is a `value` struct (GC-carrying tier): build it as a
        // `MakeValueStruct` so it scalarizes (a non-escaping `for (e, _)` scan
        // builds no table) and, where it escapes, materializes through
        // `Entity_vt` -- carrying the `__ty` tag + the value-`__eq` metatable.
        // A raw `MakeStruct` here would leak an un-tagged, metatable-less table
        // whose `==` silently mis-compares (arena-scan entities not equal to
        // pairs-scan entities).
        let e_gen = self.emit_cur(Inst::Index {
            base: generations,
            index: idx,
            kind: ContainerKind::Vec,
        });
        let entity_representation = self.nominal_value_representation(Ustr::from("Entity"))?;
        let entity = self.emit_cur(Inst::MakeValueStruct {
            name: Ustr::from("Entity"),
            representation: entity_representation,
            form: ValueCtorForm::Declared,
            fields: vec![
                (Ustr::from("_scene_id"), scene_id),
                (Ustr::from("_arena_id"), arena_id),
                (Ustr::from("_slot"), idx),
                (Ustr::from("_generation"), e_gen),
            ],
        });
        // A column element is rebuilt from its leaves here. For an ordinary value
        // struct SROA folds the field reads back to the leaf cells, so the pack
        // costs nothing; for a `nobox` element it is an implicit materialization
        // and the verifier rejects it by name rather than emitting a quiet box.
        let data = match column_elem {
            Some(elem) => {
                let repr = self.column_element_repr(data, idx, elem)?;
                self.pack(repr.into_value())?
            }
            None => {
                // The stored element stays the entry's; the binder gets its own
                // copy, as `ArenaPairIter.next` gives the plain loop.
                let stored = self.emit_cur(Inst::Index {
                    base: data,
                    index: idx,
                    kind: ContainerKind::Vec,
                });
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
        };
        match &pat {
            BindingPattern::Tuple(subs) if subs.len() == 2 => {
                self.bind_for_pattern(&subs[0], entity);
                self.bind_for_pattern(&subs[1], data);
            }
            _ if single => {
                let tup = self.emit_cur(Inst::MakeTuple(vec![entity, data]));
                self.bind_for_pattern(&pat, tup);
            }
            _ => {
                return Err(gap(
                    "for-over-Arena binds one (item) or two (entity, value)",
                ));
            }
        }
        // enumerate counter: bind to the current count, then bump (occupied only).
        if let (Some(ev), Some(ec)) = (enum_var, enum_cell) {
            let cur = self.emit_cur(Inst::Load(ec));
            let bind_cell = self.new_binding_cell(
                ev,
                self.body.binding_arena[ev].name,
                self.binding_ty(ev),
                Ustr::from(""),
            );
            self.binds.insert(raw(ev), bind_cell);
            self.emit_cur(Inst::Store {
                cell: bind_cell,
                value: cur,
            });
            let one = self.emit_cur(Inst::Const(Lit::Int(1)));
            let inc = self.emit_cur(Inst::IntAdd(cur, one));
            self.emit_cur(Inst::Store {
                cell: ec,
                value: inc,
            });
        }
        self.loop_targets.push((latch, exit, self.defers.len()));
        self.lower_stmt_expr(body)?;
        self.loop_targets.pop();
        if self.is_open() {
            self.f.set_term(self.cur, Term::Goto(latch));
        }

        // latch: i += 1.
        self.cur = latch;
        let i2 = self.emit_cur(Inst::Load(i_cell));
        let one = self.emit_cur(Inst::Const(Lit::Int(1)));
        let inc = self.emit_cur(Inst::IntAdd(i2, one));
        self.emit_cur(Inst::Store {
            cell: i_cell,
            value: inc,
        });
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = exit;
        Ok(())
    }

    /// `for (k, v) in m` / `for kv in m`: Lua stateless `next` iteration. The key
    /// cell is the cursor; a nil key after `MapNext` ends the loop.
    pub(crate) fn lower_for_map(
        &mut self,
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
            Some(_) => self.emit_cur(Inst::Field {
                base: m,
                field: Ustr::from("spine"),
            }),
            None => m,
        };
        let key_cell = self.f.new_cell(None);
        let val_cell = self.f.new_cell(None);
        self.f.mark_loop_binding(key_cell);
        self.f.mark_loop_binding(val_cell);
        let nil = self.emit_cur(Inst::Const(Lit::Nil));
        self.emit_cur(Inst::Store {
            cell: key_cell,
            value: nil,
        });

        let header = self.f.new_block();
        let body_bb = self.f.new_block();
        let exit = self.f.new_block();
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = header;
        self.emit_cur(Inst::MapNext {
            key_cell,
            val_cell,
            table,
        });
        let k = self.emit_cur(Inst::Load(key_cell));
        let nil2 = self.emit_cur(Inst::Const(Lit::Nil));
        let done = self.emit_cur(Inst::Eq(k, nil2));
        self.f.set_term(
            self.cur,
            Term::Branch {
                cond: done,
                then_bb: exit,
                else_bb: body_bb,
            },
        );

        self.cur = body_bb;
        let vv = match col {
            Some(elem) => {
                let slot = self.emit_cur(Inst::Load(val_cell));
                let data = self.map_col_data(m);
                let repr = self.column_element_repr(data, slot, elem)?;
                Some(self.pack(repr.into_value())?)
            }
            None => None,
        };
        self.bind_map_entry(vars, key_cell, val_cell, vv)?;
        self.loop_targets.push((header, exit, self.defers.len())); // continue -> header (re-runs next)
        self.lower_stmt_expr(body)?;
        self.loop_targets.pop();
        if self.is_open() {
            self.f.set_term(self.cur, Term::Goto(header));
        }

        self.cur = exit;
        Ok(())
    }

    /// `for (i, (k, v)) in enumerate(m)`: Lua stateless `next` iteration (like
    /// `lower_for_map`) with a 0-based counter cell bound to `i`, incremented in
    /// the latch so `continue` still steps it. `xpat` binds the `(k, v)` entry.
    pub(crate) fn lower_for_map_enumerate(
        &mut self,
        ib: Idx<BindingInfo>,
        xpat: &BindingPattern,
        iter: Idx<Expr>,
        body: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        let m = self.lower_expr(iter)?;
        let key_cell = self.f.new_cell(None);
        let val_cell = self.f.new_cell(None);
        self.f.mark_loop_binding(key_cell);
        self.f.mark_loop_binding(val_cell);
        let nil = self.emit_cur(Inst::Const(Lit::Nil));
        self.emit_cur(Inst::Store {
            cell: key_cell,
            value: nil,
        });
        let i_cell = self.new_binding_cell(
            ib,
            self.body.binding_arena[ib].name,
            self.binding_ty(ib),
            Ustr::from(""),
        );
        self.binds.insert(raw(ib), i_cell);
        self.f.mark_loop_binding(i_cell);
        let zero = self.emit_cur(Inst::Const(Lit::Int(0)));
        self.emit_cur(Inst::Store {
            cell: i_cell,
            value: zero,
        });

        let header = self.f.new_block();
        let body_bb = self.f.new_block();
        let latch = self.f.new_block();
        let exit = self.f.new_block();
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = header;
        self.emit_cur(Inst::MapNext {
            key_cell,
            val_cell,
            table: m,
        });
        let k = self.emit_cur(Inst::Load(key_cell));
        let nil2 = self.emit_cur(Inst::Const(Lit::Nil));
        let done = self.emit_cur(Inst::Eq(k, nil2));
        self.f.set_term(
            self.cur,
            Term::Branch {
                cond: done,
                then_bb: exit,
                else_bb: body_bb,
            },
        );

        self.cur = body_bb;
        self.bind_map_entry(xpat, key_cell, val_cell, None)?;
        self.loop_targets.push((latch, exit, self.defers.len())); // continue -> latch (steps counter)
        self.lower_stmt_expr(body)?;
        self.loop_targets.pop();
        if self.is_open() {
            self.f.set_term(self.cur, Term::Goto(latch));
        }

        self.cur = latch;
        let i2 = self.emit_cur(Inst::Load(i_cell));
        let one = self.emit_cur(Inst::Const(Lit::Int(1)));
        let inc = self.emit_cur(Inst::IntAdd(i2, one));
        self.emit_cur(Inst::Store {
            cell: i_cell,
            value: inc,
        });
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = exit;
        Ok(())
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
                let kv = self.emit_cur(Inst::Load(key_cell));
                self.bind_for_pattern(&subs[0], kv);
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

    /// `for (i, x) in enumerate(coll)` (split bindings) or `for pair in
    /// enumerate(coll)` (one binding to the `(index, element)` tuple), over an
    /// indexable collection. The counter is a cell; `x = coll[i]`.
    pub(crate) fn lower_for_enumerate(
        &mut self,
        vars: &BindingPattern,
        coll: Idx<Expr>,
        body: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        // Split `(i, xpat)` vs a single binding to the whole `(i, element)` tuple.
        let split: Option<(Idx<BindingInfo>, BindingPattern)> = match vars {
            BindingPattern::Tuple(subs) if subs.len() == 2 => match &subs[0] {
                BindingPattern::Leaf(i) => Some((*i, subs[1].clone())),
                _ => return Err(gap("enumerate binds (index, element)")),
            },
            _ => None,
        };
        let kind = self.coll_kind(coll)?;
        if kind == ContainerKind::Map {
            let (ib, xpat) =
                split.ok_or_else(|| gap("enumerate over Map with a single binding"))?;
            return self.lower_for_map_enumerate(ib, &xpat, coll, body);
        }
        let arr_v = self.lower_expr(coll)?;
        let len_v = self.emit_cur(Inst::Len { base: arr_v, kind });
        // The counter cell is named after the index var in split mode; an internal
        // cell in single-binding mode (the index is folded into the tuple).
        let i_cell = match &split {
            Some((ib, _)) => {
                let c = self.new_binding_cell(
                    *ib,
                    self.body.binding_arena[*ib].name,
                    self.binding_ty(*ib),
                    Ustr::from(""),
                );
                self.binds.insert(raw(*ib), c);
                c
            }
            None => self.f.new_cell(None),
        };
        self.f.mark_loop_binding(i_cell);
        let zero = self.emit_cur(Inst::Const(Lit::Int(0)));
        self.emit_cur(Inst::Store {
            cell: i_cell,
            value: zero,
        });

        let header = self.f.new_block();
        let body_bb = self.f.new_block();
        let latch = self.f.new_block();
        let exit = self.f.new_block();
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = header;
        let i = self.emit_cur(Inst::Load(i_cell));
        let cmp = self.emit_cur(Inst::IntLt(i, len_v));
        self.f.set_term(
            self.cur,
            Term::Branch {
                cond: cmp,
                then_bb: body_bb,
                else_bb: exit,
            },
        );

        self.cur = body_bb;
        let i_now = self.emit_cur(Inst::Load(i_cell));
        let repr = self.loop_element_repr_at(arr_v, i_now, kind)?;
        let ty = match &split {
            Some((_, xpat)) => self.loop_binder_ty(xpat, None),
            None => self.loop_binder_ty(vars, Some(1)),
        };
        let repr = self.loop_element_snapshot(ty, repr)?;
        match &split {
            // split: index already bound to i_cell; bind the element pattern.
            Some((_, xpat)) => self.bind_loop_element_repr(xpat, repr)?,
            // single: bind the whole `(index, element)` tuple.
            None => {
                let elem = self.pack(repr)?;
                let pair = self.emit_cur(Inst::MakeTuple(vec![i_now, elem]));
                self.bind_for_pattern(vars, pair);
            }
        }
        self.loop_targets.push((latch, exit, self.defers.len()));
        self.lower_stmt_expr(body)?;
        self.loop_targets.pop();
        if self.is_open() {
            self.f.set_term(self.cur, Term::Goto(latch));
        }

        self.cur = latch;
        let i2 = self.emit_cur(Inst::Load(i_cell));
        let one = self.emit_cur(Inst::Const(Lit::Int(1)));
        let inc = self.emit_cur(Inst::IntAdd(i2, one));
        self.emit_cur(Inst::Store {
            cell: i_cell,
            value: inc,
        });
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = exit;
        Ok(())
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
        // `for v in fv` over FlatVec: numeric loop bounded by the `len` FIELD
        // (not the buffer capacity), elements via the `buf` NativeBuffer.
        let (kind, arr_v, len_v) = if self.expr_is_flat_vec(iter) {
            let kind = self.nb_kind(iter)?;
            let fv = self.lower_expr(iter)?;
            let buf = self.emit_cur(Inst::Field {
                base: fv,
                field: Ustr::from("buf"),
            });
            let len = self.emit_cur(Inst::Field {
                base: fv,
                field: Ustr::from("len"),
            });
            (kind, buf, len)
        } else {
            let kind = self.coll_kind(iter)?;
            let arr_v = self.lower_expr(iter)?;
            let len_v = self.emit_cur(Inst::Len { base: arr_v, kind });
            (kind, arr_v, len_v)
        };
        let i_cell = self.f.new_cell(None);
        self.f.mark_loop_binding(i_cell);
        let zero = self.emit_cur(Inst::Const(Lit::Int(0)));
        self.emit_cur(Inst::Store {
            cell: i_cell,
            value: zero,
        });

        let header = self.f.new_block();
        let body_bb = self.f.new_block();
        let latch = self.f.new_block();
        let exit = self.f.new_block();
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = header;
        let i = self.emit_cur(Inst::Load(i_cell));
        let cmp = self.emit_cur(Inst::IntLt(i, len_v));
        self.f.set_term(
            self.cur,
            Term::Branch {
                cond: cmp,
                then_bb: body_bb,
                else_bb: exit,
            },
        );

        self.cur = body_bb;
        let i_now = self.emit_cur(Inst::Load(i_cell));
        let repr = self.loop_element_repr_at(arr_v, i_now, kind)?;
        let ty = self.loop_binder_ty(&vars, None);
        let repr = self.loop_element_snapshot(ty, repr)?;
        let elem = self.pack(repr)?;
        self.bind_for_pattern(&vars, elem);
        self.loop_targets.push((latch, exit, self.defers.len()));
        self.lower_stmt_expr(body)?;
        self.loop_targets.pop();
        if self.is_open() {
            self.f.set_term(self.cur, Term::Goto(latch));
        }

        self.cur = latch;
        let i2 = self.emit_cur(Inst::Load(i_cell));
        let one = self.emit_cur(Inst::Const(Lit::Int(1)));
        let inc = self.emit_cur(Inst::IntAdd(i2, one));
        self.emit_cur(Inst::Store {
            cell: i_cell,
            value: inc,
        });
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = exit;
        Ok(())
    }

    /// A loop binding reads its element out of the collection (a Vec element,
    /// a Map value), which the body can still write (`xs[0].f0 = 99`), so a
    /// boxed element of an ordinary value type `ty` is snapshot at the binding,
    /// as `let v = xs[i]` is. A captured or otherwise unscalarized binding
    /// would read the write through the shared box. A leaf repr is already
    /// its own copy.
    fn loop_element_snapshot(
        &mut self,
        ty: Option<Rc<MonoTy>>,
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
    ) -> Option<Rc<MonoTy>> {
        let BindingPattern::Leaf(b) = vars else {
            return None;
        };
        if self.binding_is_ref(*b) {
            return None;
        }
        let ty = self.binding_ty(*b).and_then(|ty| self.concrete_ty_id(ty))?;
        match component {
            None => Some(ty),
            Some(i) => match ty.as_ref() {
                MonoTy::Tuple(parts) => parts.get(i).cloned().map(Rc::new),
                _ => None,
            },
        }
    }

    /// [`Self::bind_for_pattern`] for an element that arrived as a repr: a
    /// `nobox` element binds its LEAVES (`group_binds`, the shape a scalarized
    /// value param takes) instead of a packed box.
    ///
    /// Packing is what the mark forbids, and for a NULLABLE element it is also
    /// lossy in a way no later pass can undo: `pack` drops the Group's presence,
    /// so the absent elements would come back as a struct of nils rather than as
    /// `null`. SROA peels a redundant pack for the non-nullable case, which is
    /// why that one worked through the boxed bind; presence has no such recovery.
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
                let representation = self.concrete_rep_id(&mono, RepUse::Value, false, true)?;
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
                    Ustr::from(""),
                );
                if self.binding_is_nonnull_value_struct(*b) {
                    self.f.mark_nonnull_value_struct(cell);
                }
                self.emit_cur(Inst::Store { cell, value });
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
                let group = self.group_binds.get(&key).map(|(cells, _)| cells.clone());
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

    /// [`Self::bind_for_pattern`] against a SPLATTING tuple's flat slot values.
    /// A `Leaves` slot binds its leaves into `group_binds` cells -- the same form
    /// a scalarized value param takes -- so reading the binding never rebuilds a
    /// box. Any pattern/slot pairing that does not line up (a single binding for
    /// a whole nested tuple, say) rebuilds and falls back to the boxed bind,
    /// which is where a whole-value use of a `nobox` element meets the
    /// guarantee's rejection.
    pub(crate) fn bind_pattern_slot(
        &mut self,
        vars: &BindingPattern,
        slot: &RetSlot,
        values: &[Idx<Inst>],
        idx: &mut usize,
    ) -> Result<(), CompilerError> {
        match (vars, slot) {
            (BindingPattern::Leaf(b), RetSlot::Leaves(shape)) => {
                let n = shape.leaf_count();
                let leaves = values[*idx..*idx + n].to_vec();
                *idx += n;
                self.bind_leaves(*b, &leaves, shape.clone())?;
            }
            (BindingPattern::Tuple(subs), RetSlot::Nested(inner))
                if subs.len() == inner.slots.len() =>
            {
                for (sub, s) in subs.iter().zip(inner.slots.clone()) {
                    self.bind_pattern_slot(sub, &s, values, idx)?;
                }
            }
            // ONE name bound to a splatting tuple (`let (ok, x) = next(it)` in a
            // combinator over a `zip`/`enumerate` element). It holds its SLOT
            // cells, like a value binding holds its leaves: packing it here would
            // pack the tuple's `nobox` parts with it, which is the guarantee's
            // violation and not something a later pass can undo.
            (BindingPattern::Leaf(b), RetSlot::Nested(inner)) if inner.splats() => {
                let n = inner.slot_count();
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
        if let Some(cells) = self.new_flat_enum_cells(b, bname, &shape, owner.as_deref())? {
            if cells.len() != leaves.len() {
                return Err(self.gap_here(format!(
                    "flat enum {} has {} binding cells for {} leaves",
                    shape.name,
                    cells.len(),
                    leaves.len()
                )));
            }
            for (cell, &value) in cells.iter().zip(leaves) {
                self.emit_cur(Inst::Store { cell: *cell, value });
            }
            self.group_binds.insert(raw(b), (cells, shape));
            return Ok(());
        }
        let leaf_tys = self.value_shape_leaf_monos(&shape).map(|tys| {
            tys.into_iter()
                .map(|ty| self.scx.interner().intern(ty))
                .collect::<Vec<_>>()
        });
        let cells: Vec<Idx<Cell>> = leaves
            .iter()
            .enumerate()
            .map(|(li, &value)| {
                let c = self.new_binding_cell(
                    b,
                    Ustr::from(format!("{bname}__{li}").as_str()),
                    None,
                    Ustr::from(li.to_string().as_str()),
                );
                self.f.cells[c].native_ty = leaf_tys.as_ref().and_then(|tys| tys.get(li).copied());
                self.emit_cur(Inst::Store { cell: c, value });
                c
            })
            .collect();
        self.group_binds.insert(raw(b), (cells, shape));
        Ok(())
    }

    fn value_shape_leaf_monos(&self, shape: &ValueShape) -> Option<Vec<MonoTy>> {
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
                FieldShape::Prim => out.push(
                    self.concrete_ty_id(field_ty)
                        .map(|ty| ty.as_ref().clone())
                        .or_else(|| {
                            crate::monoty::convert_decl(self.ws, field_ty, &SpecSubst::default())
                                .ok()
                        })?,
                ),
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
        shape: TupleRetShape,
    ) {
        let bname = self.body.binding_arena[b].name;
        let cells: Vec<Idx<Cell>> = slots
            .iter()
            .enumerate()
            .map(|(si, &value)| {
                let c = self.new_binding_cell(
                    b,
                    Ustr::from(format!("{bname}__{si}").as_str()),
                    None,
                    Ustr::from(si.to_string().as_str()),
                );
                self.f.cells[c].producer = crate::mir::CellProducer::TupleSlot;
                self.emit_cur(Inst::Store { cell: c, value });
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
        shape: &TupleRetShape,
        r: ValueRepr,
    ) -> Result<(), CompilerError> {
        let values = self.tuple_repr_to_slots(r, shape)?;
        let mut idx = 0;
        match vars {
            BindingPattern::Tuple(subs) if subs.len() == shape.slots.len() => {
                for (sub, slot) in subs.iter().zip(shape.slots.clone()) {
                    self.bind_pattern_slot(sub, &slot, &values, &mut idx)?;
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
