//! Loops and the bindings they introduce: `while`, the `for` forms (user
//! iterator, arena, map, enumerate, array) and the pattern binding that lands
//! each element -- including the per-iteration tuple that gets scalarized.
use super::*;

impl Builder<'_> {
    pub(crate) fn loop_target(&self, what: &str) -> Result<(Idx<Block>, Idx<Block>), CompilerError> {
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
    pub(crate) fn lower_while(&mut self, cond: Idx<Expr>, body: Idx<Expr>) -> Result<bool, CompilerError> {
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
            let cond_v = self.lower_expr(cond)?;
            self.f.set_term(
                self.cur,
                Term::Branch {
                    cond: cond_v,
                    then_bb: body_bb,
                    else_bb: exit,
                },
            );
        }

        self.cur = body_bb;
        self.loop_targets.push((header, exit));
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
        if let Expr::Range { low, high } = self.body.expr_arena[iter].clone() {
            return match vars {
                BindingPattern::Leaf(var_b) => self.lower_for_range(var_b, low, high, body),
                BindingPattern::Tuple(_) => Err(gap("for-range with a tuple pattern")),
            };
        }
        // User iterator protocol (bone recorded the resolution for this For):
        // desugar to `it = iter(coll); loop { (ok, ..) = next(it); !ok -> break }`.
        // Only populated for non-builtin collections, so this never shadows a
        // builtin template. `iter: None` = identity (the collection IS the
        // iterator); a `CallerGeneric` next = placeholder in a bounded body.
        if let Some(info) = self.inf.and_then(|i| i.for_iter_protocol.get(&iter).cloned()) {
            return self.lower_for_user_iter(&vars, iter, body, &info);
        }
        if let Expr::Call {
            callee: Callee::Unresolved { name, .. },
            args,
        } = self.body.expr_arena[iter].clone()
        {
            if name.as_str() == "enumerate" {
                let av: Vec<Idx<Expr>> = args.into_iter().collect();
                if let [coll] = av[..] {
                    if self.iter_is_arena(coll) {
                        return self.lower_for_arena(&vars, coll, body, true);
                    }
                    return self.lower_for_enumerate(&vars, coll, body);
                }
            }
            // A non-enumerate call returning a collection: fall through to the
            // collection dispatch below. `lower_for_*` lower the iter once into a
            // value, so the call runs exactly once (no per-iteration re-eval).
        }
        if self.iter_is_arena(iter) {
            // Direct `for .. in arena` arrives with `for_iter_protocol`
            // populated (bone routes Arena to the stdlib ArenaPairIter); only
            // `enumerate(arena)` still uses the template above.
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
            ContainerKind::Vec | ContainerKind::VecValueColumns(_) => Err(gap(
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
            ContainerKind::NativeBufferLeaf => Err(gap("for over a synthetic NativeBuffer leaf view")),
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
                let repr = self.lower_expr_repr(iter_expr)?;
                // Resolved BEFORE the arguments are shaped, for the same reason
                // the implicit-coercion form does it: the splat must match the
                // body the call targets.
                let witness = match info.iter.as_ref() {
                    Some(w) => {
                        let span = self
                            .body
                            .expr_spans
                            .get(iter_expr)
                            .copied()
                            .unwrap_or(self.ws.module(m).functions[f].name_span);
                        match self.next_call_spec(iter_ref, &w.generics, span)? {
                            Some(key) => crate::instance::Instance::intern(
                                self.scx.interner(),
                                iter_ref,
                                key,
                            ),
                            None if self.ws.module(m).functions[f].generics.is_empty() => {
                                crate::instance::Instance::intern(
                                    self.scx.interner(),
                                    iter_ref,
                                    Vec::new(),
                                )
                            }
                            None => crate::instance::Instance::base(iter_ref),
                        }
                    }
                    None if self.ws.module(m).functions[f].generics.is_empty() => {
                        crate::instance::Instance::intern(self.scx.interner(), iter_ref, Vec::new())
                    }
                    None => crate::instance::Instance::base(iter_ref),
                };
                let args = self.iter_witness_args(&witness, repr)?;
                // The `for` head's `iter` witness, not the head expression's
                // own call: both live at one expression and share one span, so
                // the operation is what keeps them distinguishable.
                let call = self.with_site_op(crate::site::SiteOp::ForIter, |b| {
                    b.emit_cur(Inst::Call { func: iter_ref, args })
                });
                // DELIVERY, from the same `IterWitness::generics` the identity
                // record below reads. The witness is an ordinary call to a
                // generic `iter`, so under instance-owned emission it must name
                // an instance like any other -- it was the last call form still
                // reaching a shared generic body by construction, and once that
                // body stopped being emitted it named a nil global.
                if !witness.args.is_empty() {
                    self.f.spec_bindings.insert(call, witness.clone());
                }
                if self.ws.module(m).functions[f].generics.is_empty() || !witness.args.is_empty() {
                    crate::site::record_call_owner(
                        &mut self.f,
                        self.ws,
                        self.scx.interner(),
                        call,
                        &witness,
                    );
                }
                self.f
                    .call_abi_plans
                    .insert(call, crate::lower::BackendFnAbiPlan::for_instance(self.ws, self.scx, &witness));
                call
            }
            None => self.lower_expr(iter_expr)?,
        };
        let it_cell = self.f.new_cell(Some(Ustr::from("__it")));
        self.emit_cur(Inst::Store { cell: it_cell, value: it_val });

        // next returns `(bool, E)` / `(bool, A, B, ...)`. The shape is read under
        // the SPECIALIZATION the emitted call will carry, not the caller's own:
        // when the spec is dropped the call reaches the shared generic body,
        // whose `E` is unbound and therefore never splats -- so the two sides
        // agree about how many values come back.
        let spec = match &info.next {
            BoundNext::Concrete { func: (m, f), next_generics, .. } => self
                .next_call_spec(
                    FuncRef { module: *m, func: *f },
                    next_generics,
                    self.body
                        .expr_spans
                        .get(iter_expr)
                        .copied()
                        .unwrap_or(self.ws.module(*m).functions[*f].name_span),
                )?,
            BoundNext::CallerGeneric(_) => None,
        };
        // A placeholder next is `(bool, E)` by bound conformance, but `E`'s SHAPE
        // is this body's own specialization to answer -- the concrete callee is
        // only the source of how many values come back, and the two agree because
        // both read the same instantiation. Resolving it here is what splits a
        // presence-carrying element into {presence cell, leaf cells} at the point
        // the shape is known; leaving it opaque forces the splice to rebuild an
        // aggregate, and a presence leaf has no field to rebuild into.
        let ret_shape = match &info.next {
            BoundNext::Concrete { func: (m, f), .. } => {
                let func = FuncRef { module: *m, func: *f };
                let subst = spec
                    .as_ref()
                    .map(|k| spec_subst_from_key(self.ws, func, k))
                    .unwrap_or_default();
                self.ws.module(*m).functions[*f]
                    .return_ty
                    .and_then(|t| tuple_ret_shape(self.ws, &subst, t))
                    .ok_or_else(|| gap("for-iter `next` must return a tuple"))?
            }
            BoundNext::CallerGeneric(gidx) => self.bound_next_tuple_shape(*gidx),
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
            BoundNext::Concrete { func: (m, f), sub, .. } => {
                let func = FuncRef { module: *m, func: *f };
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
                let callee_decl = &self.ws.module(func.module).functions[func.func];
                let owner = spec.as_ref().map(|key| {
                    crate::instance::Instance::intern(self.scx.interner(), func, key.clone())
                });
                if spec.is_some() {
                    self.f.spec_bindings.insert(call, owner.clone().expect("spec has owner"));
                }
                if callee_decl.generics.is_empty() || owner.is_some() {
                    let owner = owner.unwrap_or_else(|| {
                        crate::instance::Instance::intern(self.scx.interner(), func, Vec::new())
                    });
                    crate::site::record_call_owner(
                        &mut self.f,
                        self.ws,
                        self.scx.interner(),
                        call,
                        &owner,
                    );
                    self.f.call_abi_plans.insert(
                        call,
                        crate::lower::BackendFnAbiPlan::for_instance(self.ws, self.scx, &owner),
                    );
                } else {
                    self.f.call_abi_plans.insert(
                        call,
                        self.callee_abi_plan(func, None, &SpecSubst::default()),
                    );
                }
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
        self.f.set_term(self.cur, Term::Branch { cond: ok, then_bb: body_bb, else_bb: exit });

        // body: bind the pattern to the element slots. One element binds
        // directly (its leaves, if it splats); multiple (`next -> (bool, A, B)`)
        // are a tuple `bind_pattern_slot` destructures against a matching tuple
        // pattern (no alloc).
        self.cur = body_bb;
        let loads: Vec<Idx<Inst>> =
            dests[1..].iter().map(|c| self.emit_cur(Inst::Load(*c))).collect();
        let elem_slot = match &ret_shape.slots[1..] {
            [one] => one.clone(),
            rest => RetSlot::Nested(TupleRetShape { slots: rest.to_vec(), nullable: false }),
        };
        let mut idx = 0;
        self.bind_pattern_slot(vars, &elem_slot, &loads, &mut idx);
        self.loop_targets.push((header, exit));
        self.lower_stmt_expr(body)?;
        self.loop_targets.pop();
        if self.is_open() {
            self.f.set_term(self.cur, Term::Goto(header));
        }
        self.cur = exit;
        Ok(())
    }

    /// The variant names of `e`'s enum type (empty if `e` is not an enum) — used
    /// to recognize an unqualified nullary-variant pattern at a match root.
    pub(crate) fn scrut_variants(&self, e: Idx<Expr>) -> HashSet<Ustr> {
        if let Some(TypeExpr::App { ctor: TypeCtor::Nominal(name), .. }) = self.expr_ty(e) {
            if let Some(&eid) = self.ws.enums_by_name.get(&name) {
                let ed = &self.ws.modules[eid.module.index()].enums[eid.def];
                return ed.variants.iter().map(|v| v.name).collect();
            }
        }
        HashSet::new()
    }

    /// The enum NAME of a scrutinee whose static type is an enum (`None` else) --
    /// threaded into `match_pattern`/`test_tag` so a payload-free enum's tag test
    /// can int-compare the ordinal instead of reading `.__variant`.
    pub(crate) fn scrut_enum_name(&self, e: Idx<Expr>) -> Option<Ustr> {
        match self.expr_ty(e) {
            Some(TypeExpr::App { ctor: TypeCtor::Nominal(name), .. })
                if self.ws.enums_by_name.contains_key(&name) =>
            {
                Some(name)
            }
            _ => None,
        }
    }

    /// Whether `e`'s inferred type is a transparent newtype nominal.
    pub(crate) fn expr_is_newtype(&self, e: Idx<Expr>) -> bool {
        matches!(
            self.expr_ty(e),
            Some(TypeExpr::App { ctor: TypeCtor::Nominal(name), .. })
                if self.ws.newtypes_by_name.contains_key(&name)
        )
    }

    /// Whether `iter`'s inferred type is an `Arena<T>` nominal.
    pub(crate) fn iter_is_arena(&self, iter: Idx<Expr>) -> bool {
        matches!(
            self.expr_ty(iter),
            Some(TypeExpr::App { ctor: TypeCtor::Nominal(name), .. }) if name.as_str() == "Arena"
        )
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
            _ => return Err(gap("for-over-Arena binds one (item) or two (entity, value)")),
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
        let arena = self.lower_expr(iter)?;
        let scene_id = self.emit_cur(Inst::Field { base: arena, field: Ustr::from("_scene_id") });
        let arena_id = self.emit_cur(Inst::Field { base: arena, field: Ustr::from("_arena_id") });
        let store = self.call_spine("__arena_store_or_empty", vec![scene_id, arena_id])?;
        let occupied = self.emit_cur(Inst::Field { base: store, field: Ustr::from("occupied") });
        let generations = self.emit_cur(Inst::Field { base: store, field: Ustr::from("generations") });
        let data = self.emit_cur(Inst::Field { base: store, field: Ustr::from("data") });
        let n = self.emit_cur(Inst::Len { base: occupied, kind: ContainerKind::Vec });
        let i_cell = self.f.new_cell(None);
        self.f.mark_loop_binding(i_cell);
        let zero = self.emit_cur(Inst::Const(Lit::Int(0)));
        self.emit_cur(Inst::Store { cell: i_cell, value: zero });
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
        self.f.set_term(self.cur, Term::Branch { cond: c, then_bb: body_bb, else_bb: exit });

        // body: nothing to skip before the occupancy read -- the walk is bounded
        // by the occupancy column's own length, and a `[bool]` column has no
        // holes, so the old nil test had nothing left to catch.
        self.cur = body_bb;
        let idx = self.emit_cur(Inst::Load(i_cell));
        self.f.set_term(self.cur, Term::Goto(occ_bb));

        // occ: skip unoccupied.
        self.cur = occ_bb;
        let occ = self.emit_cur(Inst::Index { base: occupied, index: idx, kind: ContainerKind::Vec });
        self.f.set_term(self.cur, Term::Branch { cond: occ, then_bb: do_bb, else_bb: latch });

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
        let e_gen =
            self.emit_cur(Inst::Index { base: generations, index: idx, kind: ContainerKind::Vec });
        let entity = self.emit_cur(Inst::MakeValueStruct {
            name: Ustr::from("Entity"),
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
                self.pack(repr)
            }
            None => self.emit_cur(Inst::Index { base: data, index: idx, kind: ContainerKind::Vec }),
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
            _ => return Err(gap("for-over-Arena binds one (item) or two (entity, value)")),
        }
        // enumerate counter: bind to the current count, then bump (occupied only).
        if let (Some(ev), Some(ec)) = (enum_var, enum_cell) {
            let cur = self.emit_cur(Inst::Load(ec));
            let bind_cell = self.f.new_cell(Some(self.body.binding_arena[ev].name));
            self.binds.insert(raw(ev), bind_cell);
            self.emit_cur(Inst::Store { cell: bind_cell, value: cur });
            let one = self.emit_cur(Inst::Const(Lit::Int(1)));
            let inc = self.emit_cur(Inst::IntAdd(cur, one));
            self.emit_cur(Inst::Store { cell: ec, value: inc });
        }
        self.loop_targets.push((latch, exit));
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
        self.emit_cur(Inst::Store { cell: i_cell, value: inc });
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
            Some(_) => self.emit_cur(Inst::Field { base: m, field: Ustr::from("spine") }),
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
                Some(self.pack(repr))
            }
            None => None,
        };
        self.bind_map_entry(vars, key_cell, val_cell, vv)?;
        self.loop_targets.push((header, exit)); // continue -> header (re-runs next)
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
        self.emit_cur(Inst::Store { cell: key_cell, value: nil });
        let i_cell = self.f.new_cell(Some(self.body.binding_arena[ib].name));
        self.binds.insert(raw(ib), i_cell);
        self.f.mark_loop_binding(i_cell);
        let zero = self.emit_cur(Inst::Const(Lit::Int(0)));
        self.emit_cur(Inst::Store { cell: i_cell, value: zero });

        let header = self.f.new_block();
        let body_bb = self.f.new_block();
        let latch = self.f.new_block();
        let exit = self.f.new_block();
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = header;
        self.emit_cur(Inst::MapNext { key_cell, val_cell, table: m });
        let k = self.emit_cur(Inst::Load(key_cell));
        let nil2 = self.emit_cur(Inst::Const(Lit::Nil));
        let done = self.emit_cur(Inst::Eq(k, nil2));
        self.f.set_term(self.cur, Term::Branch { cond: done, then_bb: exit, else_bb: body_bb });

        self.cur = body_bb;
        self.bind_map_entry(xpat, key_cell, val_cell, None)?;
        self.loop_targets.push((latch, exit)); // continue -> latch (steps counter)
        self.lower_stmt_expr(body)?;
        self.loop_targets.pop();
        if self.is_open() {
            self.f.set_term(self.cur, Term::Goto(latch));
        }

        self.cur = latch;
        let i2 = self.emit_cur(Inst::Load(i_cell));
        let one = self.emit_cur(Inst::Const(Lit::Int(1)));
        let inc = self.emit_cur(Inst::IntAdd(i2, one));
        self.emit_cur(Inst::Store { cell: i_cell, value: inc });
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
                    None => self.emit_cur(Inst::Load(val_cell)),
                };
                self.bind_for_pattern(&subs[1], vv);
                Ok(())
            }
            BindingPattern::Leaf(_) => {
                let kv = self.emit_cur(Inst::Load(key_cell));
                let vv = match col_value {
                    Some(v) => v,
                    None => self.emit_cur(Inst::Load(val_cell)),
                };
                let pair = self.emit_cur(Inst::MakeTuple(vec![kv, vv]));
                self.bind_for_pattern(vars, pair);
                Ok(())
            }
            _ => Err(gap("for-over-Map binds one (item) or two (key, value) vars")),
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
            let (ib, xpat) = split.ok_or_else(|| gap("enumerate over Map with a single binding"))?;
            return self.lower_for_map_enumerate(ib, &xpat, coll, body);
        }
        let arr_v = self.lower_expr(coll)?;
        let len_v = self.emit_cur(Inst::Len { base: arr_v, kind });
        // The counter cell is named after the index var in split mode; an internal
        // cell in single-binding mode (the index is folded into the tuple).
        let i_cell = match &split {
            Some((ib, _)) => {
                let c = self.f.new_cell(Some(self.body.binding_arena[*ib].name));
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
        match &split {
            // split: index already bound to i_cell; bind the element pattern.
            Some((_, xpat)) => self.bind_loop_element_repr(xpat, repr),
            // single: bind the whole `(index, element)` tuple.
            None => {
                let elem = self.pack(repr);
                let pair = self.emit_cur(Inst::MakeTuple(vec![i_now, elem]));
                self.bind_for_pattern(vars, pair);
            }
        }
        self.loop_targets.push((latch, exit));
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
            let kind = self.nb_kind(iter);
            let fv = self.lower_expr(iter)?;
            let buf = self.emit_cur(Inst::Field { base: fv, field: Ustr::from("buf") });
            let len = self.emit_cur(Inst::Field { base: fv, field: Ustr::from("len") });
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
        let elem = self.loop_element_at(arr_v, i_now, kind)?;
        self.bind_for_pattern(&vars, elem);
        self.loop_targets.push((latch, exit));
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

    /// [`Self::bind_for_pattern`] for an element that arrived as a repr: a
    /// `nobox` element binds its LEAVES (`group_binds`, the shape a scalarized
    /// value param takes) instead of a packed box.
    ///
    /// Packing is what the mark forbids, and for a NULLABLE element it is also
    /// lossy in a way no later pass can undo: `pack` drops the Group's presence,
    /// so the absent elements would come back as a struct of nils rather than as
    /// `null`. SROA peels a redundant pack for the non-nullable case, which is
    /// why that one worked through the boxed bind; presence has no such recovery.
    pub(crate) fn bind_loop_element_repr(&mut self, vars: &BindingPattern, repr: ValueRepr) {
        if let BindingPattern::Leaf(b) = vars {
            if let Some(shape) = self.binding_ty(*b).and_then(|t| self.nobox_shape_of_ty(t)) {
                let name = self.body.binding_arena[*b].name;
                let leaves = self.repr_to_leaves(&repr, &shape);
                let cells: Vec<Idx<Cell>> = leaves
                    .iter()
                    .enumerate()
                    .map(|(li, &value)| {
                        let c = self
                            .f
                            .new_cell(Some(Ustr::from(format!("{name}__{li}").as_str())));
                        self.emit_cur(Inst::Store { cell: c, value });
                        c
                    })
                    .collect();
                self.group_binds.insert(raw(*b), (cells, shape));
                return;
            }
        }
        let value = self.pack(repr);
        self.bind_for_pattern(vars, value);
    }

    /// Bind a for-loop pattern to an element value: `Leaf` -> one cell; `Tuple`
    /// -> destructure via `TupleIndex` into a cell per element.
    pub(crate) fn bind_for_pattern(&mut self, vars: &BindingPattern, value: Idx<Inst>) {
        match vars {
            BindingPattern::Leaf(b) => {
                let cell = self.f.new_cell(Some(self.body.binding_arena[*b].name));
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
                // `let (ok, x) = next(it)` zero-alloc (slice 1).
                if let Inst::MakeTuple(vs) = &self.f.insts[value] {
                    if vs.len() == subs.len() {
                        let vs = vs.clone();
                        for (sub, v) in subs.iter().zip(vs) {
                            self.bind_for_pattern(sub, v);
                        }
                        return;
                    }
                }
                for (i, sub) in subs.iter().enumerate() {
                    let elem = self.emit_cur(Inst::TupleIndex { base: value, index: i });
                    self.bind_for_pattern(sub, elem);
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
    ) {
        match (vars, slot) {
            (BindingPattern::Leaf(b), RetSlot::Leaves(shape)) => {
                let n = shape.leaf_count();
                let leaves = values[*idx..*idx + n].to_vec();
                *idx += n;
                self.bind_leaves(*b, &leaves, shape.clone());
            }
            (BindingPattern::Tuple(subs), RetSlot::Nested(inner))
                if subs.len() == inner.slots.len() =>
            {
                for (sub, s) in subs.iter().zip(inner.slots.clone()) {
                    self.bind_pattern_slot(sub, &s, values, idx);
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
                let v = self.pack(r);
                self.bind_for_pattern(vars, v);
            }
        }
    }

    /// Bind `b` to a value's LEAVES: one cell per leaf, registered in
    /// `group_binds`. The single form for "this binding is a scalarized value",
    /// shared by `if let` and the splatting tuple destructures.
    pub(crate) fn bind_leaves(&mut self, b: Idx<BindingInfo>, leaves: &[Idx<Inst>], shape: ValueShape) {
        let bname = self.body.binding_arena[b].name;
        let cells: Vec<Idx<Cell>> = leaves
            .iter()
            .enumerate()
            .map(|(li, &value)| {
                let c = self.f.new_cell(Some(Ustr::from(format!("{bname}__{li}").as_str())));
                self.emit_cur(Inst::Store { cell: c, value });
                c
            })
            .collect();
        self.group_binds.insert(raw(b), (cells, shape));
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
                let c = self.f.new_cell(Some(Ustr::from(format!("{bname}__{si}").as_str())));
                self.emit_cur(Inst::Store { cell: c, value });
                c
            })
            .collect();
        self.tuple_binds.insert(raw(b), (cells, shape));
    }

    /// Destructure a splatting tuple into `vars`. The elements arrive as flat
    /// slots, so an element that is a `nobox` value binds its leaves directly.
    pub(crate) fn bind_tuple_repr(&mut self, vars: &BindingPattern, shape: &TupleRetShape, r: ValueRepr) {
        let values = self.tuple_repr_to_slots(r, shape);
        let mut idx = 0;
        match vars {
            BindingPattern::Tuple(subs) if subs.len() == shape.slots.len() => {
                for (sub, slot) in subs.iter().zip(shape.slots.clone()) {
                    self.bind_pattern_slot(sub, &slot, &values, &mut idx);
                }
            }
            _ => {
                let r = self.tuple_from_slots(shape, &values, &mut idx);
                let v = self.pack(r);
                self.bind_for_pattern(vars, v);
            }
        }
    }
}
