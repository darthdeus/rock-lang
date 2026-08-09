//! Statement lowering: `static` locals, nested `fn`s and their ABI, the
//! statement list itself, and the `defer` machinery (inline splice, the LIFO
//! run at block exit and before every `return`, and the coroutine copy).
use super::*;

impl Builder<'_> {
    /// Lower a statement sequence into the current block. Returns `true` if a
    /// `return` terminated the block (the rest is dead).
    /// `static name = init` -> a private global cell + done-flag, guarded so the
    /// initializer runs only on the first reach:
    /// `if not <done> then <cell> = <init>; <done> = true end`. Cell/done-flag
    /// names are recorded on `MirFunc::statics` for the mir_emit reload preamble;
    /// reads/writes of the binding resolve to the cell global via the
    /// `static_cells` guards in the read/assign/compound/incdec paths.
    pub(crate) fn lower_static(
        &mut self,
        name: Idx<BindingInfo>,
        value: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        let sname = self.body.binding_arena[name].name;
        let ctr = {
            let c = self.static_counters.entry(sname).or_insert(0);
            let v = *c;
            *c += 1;
            v
        };
        let cell = Ustr::from(format!("__static_{}_{}_{}", self.static_owner, sname, ctr).as_str());
        let done_flag = Ustr::from(format!("{cell}__done").as_str());
        self.static_cells.insert(raw(name), cell);

        let ty = self
            .inf
            .and_then(|i| i.binding_types.get(name).copied())
            .or(self.body.binding_arena[name].declared_ty);
        let type_str = ty
            .map(|t| self.ws.render(self.inf, t))
            .unwrap_or_else(|| "any".to_string());
        let init_hash = {
            use std::hash::Hasher;
            let mut hsh = std::collections::hash_map::DefaultHasher::new();
            hash_init_expr(self.body, value, &mut hsh);
            hsh.finish()
        };
        self.f.statics.push(StaticCell { cell, done_flag, type_str, init_hash });

        let done_v = self.emit_cur(Inst::Global(done_flag));
        let init_bb = self.f.new_block();
        let join = self.f.new_block();
        self.f
            .set_term(self.cur, Term::Branch { cond: done_v, then_bb: join, else_bb: init_bb });
        self.cur = init_bb;
        let v = self.lower_expr(value)?;
        self.emit_cur(Inst::StoreGlobal { name: cell, value: v });
        let t = self.emit_cur(Inst::Const(Lit::Bool(true)));
        self.emit_cur(Inst::StoreGlobal { name: done_flag, value: t });
        self.f.set_term(self.cur, Term::Goto(join));
        self.cur = join;
        Ok(())
    }

    /// The value ABI of a nested `fn`, read by both its definition and its call
    /// sites. Same rule as [`BackendFnAbiPlan`] for a top-level fn, minus annotation
    /// gates: a fn value narrows to `nobox` shapes; a direct-only fn may use every
    /// scalarizable value shape.
    pub(crate) fn nested_fn_abi(
        &self,
        name: Idx<BindingInfo>,
        params: IdxRange<BindingInfo>,
        ret_ty: Option<TyId>,
        hoisted: Ustr,
    ) -> NestedFn {
        let param_names: Vec<Ustr> =
            params.into_iter().map(|pb| self.body.binding_arena[pb].name).collect();
        let boxed =
            NestedFn { hoisted, param_names: param_names.clone(), param_shapes: Vec::new(), ret_shape: None };
        let taken_by_value = self
            .body
            .expr_arena
            .iter()
            .any(|(_, e)| matches!(e, Expr::Var(b) if *b == name));
        if self.scx.values_first_class {
            return boxed;
        }
        // TAKEN BY VALUE: the ABI narrows to what an indirect call site can
        // compute from the fn TYPE alone ([`Self::fn_value_abi`]), which is
        // `nobox` values only. Both ends must run the SAME predicate -- using
        // the wider `body_sig_value_shape` here would scalarize an ordinary
        // value param that an opaque call site still boxes, and the two would
        // disagree exactly where neither can see the other. Dropping to the
        // boxed ABI entirely, which is what this used to do, is the same
        // disagreement in the other direction: it is what made a `nobox` param
        // reject once the fn was taken by value.
        let slot_of = |s: &Self, t: TyId| {
            if taken_by_value {
                s.nobox_param_slot_of_ty(t)
            } else {
                // A direct-only nested fn may use every scalarizable value
                // shape, but the TUPLE rule is the same either way -- only a
                // `nobox` element splats -- so both branches share it.
                match s.body_sig_value_shape(t) {
                    Some(shape) => RetSlot::Leaves(shape),
                    None => s.tuple_param_slot_of_ty(t),
                }
            }
        };
        let shape_of = |s: &Self, t: TyId| {
            if taken_by_value {
                s.nobox_shape_of_ty(t)
            } else {
                s.body_sig_value_shape(t)
            }
        };
        let abi = NestedFn {
            hoisted,
            param_names,
            param_shapes: params
                .into_iter()
                .map(|pb| {
                    self.binding_ty(pb).map_or(RetSlot::Opaque, |t| slot_of(self, t))
                })
                .collect(),
            ret_shape: self
                .nested_fn_ret_ty(name, ret_ty)
                .and_then(|t| shape_of(self, t)),
        };
        if abi.uses_scalar_abi() { abi } else { boxed }
    }

    /// A nested `fn`'s return type. An ANNOTATION is authoritative when present;
    /// without one the type is inferred, and it must be read off the binding's own
    /// `fn(..) -> R` type rather than defaulted away -- an inferred value return is
    /// as much a part of the signature as a written one, and treating it as absent
    /// silently drops the callee back to the boxed ABI.
    pub(crate) fn nested_fn_ret_ty(&self, name: Idx<BindingInfo>, ret_ty: Option<TyId>) -> Option<TyId> {
        if ret_ty.is_some() {
            return ret_ty;
        }
        let fty = self.resolve_tyid(self.binding_ty(name)?)?;
        match self.ty_node(fty)? {
            TypeExpr::Function { ret, .. } => Some(fty.with_idx(ret)),
            _ => None,
        }
    }

    /// [`sig_value_shape`] for a type that may live in the BODY's inference arena
    /// rather than the workspace one -- a nested `fn`'s params are declared inside
    /// the enclosing body, so `static_ty` cannot see them. Same shape policy
    /// (`sig_shape_of_nominal`), reached through `ty_node`, which also applies the
    /// enclosing specialization substitution.
    pub(crate) fn body_sig_value_shape(&self, ty: TyId) -> Option<ValueShape> {
        let ty = self.resolve_tyid(ty)?;
        match self.ty_node(ty)? {
            TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } => sig_shape_of_nominal(self.ws, n),
            TypeExpr::Nullable(inner) => {
                let inner = self.resolve_tyid(ty.with_idx(inner))?;
                match self.ty_node(inner)? {
                    TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } => {
                        sig_shape_of_nullable_nominal(self.ws, n)
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Hoist a nested `fn` to a private top-level [`MirFunc`] `__local_<owner>_
    /// <name>_<ctr>`. The name is registered in `nested_fn_globals` BEFORE the
    /// body is lowered so self-recursion resolves; the child inherits the
    /// parent's in-scope statics + sibling nested fns (decl-before-use).
    pub(crate) fn lower_nested_fn(
        &mut self,
        name: Idx<BindingInfo>,
        params: IdxRange<BindingInfo>,
        ret_ty: Option<TyId>,
        body: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        let sname = self.body.binding_arena[name].name;
        let ctr = {
            let c = self.nested_fn_counters.entry(sname).or_insert(0);
            let v = *c;
            *c += 1;
            v
        };
        let hoisted = Ustr::from(format!("__local_{}_{}_{}", self.static_owner, sname, ctr).as_str());
        let abi = self.nested_fn_abi(name, params, ret_ty, hoisted);
        self.nested_fn_globals.insert(raw(name), abi.clone());

        let mf = MirFunc::new(hoisted);
        let entry = mf.entry();
        let mut child = Builder {
            ws: self.ws,
            scx: self.scx,
            spec_subst: self.spec_subst,
            struct_default_subst: self.struct_default_subst.clone(),
            generic_bounds: self.generic_bounds,
            body: self.body,
            inf: self.inf,
            f: mf,
            cur: entry,
            binds: HashMap::new(),
            loop_targets: Vec::new(),
            used_break_targets: HashSet::default(),
            defers: Vec::new(),
            in_defer_body: false,
            resume_arg: None,
            // A hoisted nested fn is emitted via `emit_nested_fns`, which never
            // runs `inline_functions` -- box value args.
            param_shapes: abi.param_shapes.clone(),
            ret_shape: abi.ret_shape.clone(),
            ret_tuple: None,
            // A nested fn is emitted as a sibling top-level Lua fn and is never a
            // GLSL callee (referenced indirectly), so a return coercion here is
            // Lua-identity only; leaving it off matches today's emitted text.
            ret_ty: None,
            fn_name_span: None,
            cur_span: None,
            cur_expr: None,
            cur_domain: crate::site::SiteDomain::Own,
            cur_op: crate::site::SiteOp::DirectCall,
            site_owner: self.site_owner.clone(),
            group_binds: HashMap::new(),
        tuple_binds: HashMap::new(),
            // In-scope statics + sibling nested fns are module-level, so the
            // hoisted body may reference them; enclosing params/locals are NOT
            // inherited (bone rejects capturing them).
            static_cells: self.static_cells.clone(),
            static_owner: hoisted,
            static_counters: HashMap::new(),
            nested_fn_globals: self.nested_fn_globals.clone(),
            nested_fn_counters: HashMap::new(),
            presence_only: None,
        };

        for (i, pb) in params.into_iter().enumerate() {
            let pname = child.body.binding_arena[pb].name;
            let slot = child.param_shapes.get(i).cloned().unwrap_or(RetSlot::Opaque);
            child.bind_param_slot(pb, pname, &slot);
        }

        child.defers.push(Vec::new());
        if let Expr::Block { statements, tail } = child.body.expr_arena[body] {
            let terminated = child.lower_stmts(statements)?;
            if !terminated {
                let leaves = child.return_leaves(tail)?;
                child.run_top_frame()?;
                child.f.set_term(child.cur, Term::Return(leaves));
            }
        } else {
            let leaves = child.return_leaves(Some(body))?;
            child.f.set_term(child.cur, Term::Return(leaves));
        }
        child.defers.pop();

        self.f.nested_fns.push(child.f);
        Ok(())
    }

    pub(crate) fn lower_stmts(&mut self, stmts: IdxRange<Statement>) -> Result<bool, CompilerError> {
        for sidx in stmts {
            // Coarse anchor for anything lowered outside an expression (a `for`
            // desugaring's element bind, a scope-exit splice). `lower_expr`
            // narrows it wherever a real expression is in hand.
            self.cur_span = self.body.statement_spans.get(sidx).copied();
            match self.body.statement_arena[sidx].clone() {
                Statement::Let { name, value } => {
                    // Flat enums and guaranteed no-box values live in leaf cells,
                    // matching the scalar ABI instead of relying on later SROA.
                    let threaded_shape = self
                        .flat_enum_value_shape_of(value)
                        .or_else(|| self.nobox_binding_shape(name))
                        .or_else(|| self.nobox_value_shape_of(value));
                    if let Some(shape) = threaded_shape {
                        let r = self.lower_expr_repr(value)?;
                        let leaves = self.repr_to_leaves(&r, &shape);
                        let bname = self.body.binding_arena[name].name;
                        let cells: Vec<Idx<Cell>> = leaves
                            .into_iter()
                            .enumerate()
                            .map(|(li, lv)| {
                                let c = self.f.new_cell(Some(Ustr::from(format!("{bname}__{li}").as_str())));
                                self.emit_cur(Inst::Store { cell: c, value: lv });
                                c
                            })
                            .collect();
                        self.group_binds.insert(raw(name), (cells, shape));
                    } else if let Some(shape) = self
                        .nullable_tuple_wire_shape(value)?
                        .or_else(|| self.expr_tuple_shape(value).filter(TupleRetShape::splats))
                    {
                        // A splatting tuple binds its WIRE -- one cell per slot, led
                        // by presence when the wire carries one. Packing it instead
                        // boxes the `nobox` element the splat exists to keep
                        // unboxed; a packed tuple is also structurally never nil, so
                        // a nullable one answered `t == null` as false when absent.
                        let r = self.lower_expr_repr(value)?;
                        let slots = self.tuple_repr_to_slots(r, &shape);
                        self.bind_tuple_slots(name, &slots, shape);
                    } else {
                        let v = self.lower_expr(value)?;
                        let v = self.coerced_to_binding(v, value, name);
                        let cty = self.binding_ty(name);
                        let cell = self.f.new_cell_typed(Some(self.body.binding_arena[name].name), cty);
                        if self.binding_is_nonnull_value_struct(name) {
                            self.f.mark_nonnull_value_struct(cell);
                        }
                        self.emit_cur(Inst::Store { cell, value: v });
                        self.binds.insert(raw(name), cell);
                    }
                }
                Statement::Expr(e) => {
                    self.lower_stmt_expr(e)?;
                }
                Statement::Return(opt) => {
                    let leaves = self.return_leaves(opt)?;
                    // Run every pending defer (all enclosing block scopes, LIFO)
                    // before the actual return.
                    self.run_all_defers()?;
                    self.f.set_term(self.cur, Term::Return(leaves));
                    return Ok(true);
                }
                // Collected, not run inline; spliced at block exit / before return.
                // In a co fn, dying (kill/error) counts as exiting every open
                // scope: registration also sets a frame flag + builds a detached
                // terminal-cleanup copy (see `MirFunc::coro_defers`).
                Statement::Defer(body) => {
                    let flag = if self.resume_arg.is_some() {
                        let flag = self.f.new_cell(Some(Ustr::from("__defer")));
                        let on = self.emit_cur(Inst::Const(Lit::Bool(true)));
                        self.emit_cur(Inst::Store { cell: flag, value: on });
                        self.build_coro_defer_copy(body, flag)?;
                        Some(flag)
                    } else {
                        None
                    };
                    if let Some(frame) = self.defers.last_mut() {
                        frame.push((body, flag));
                    }
                }
                // `let (a, b) = e` — destructure via TupleIndex into a cell per
                // leaf (the same machinery `for (k,v)` uses).
                Statement::LetTuple { vars, value } => {
                    match self.expr_tuple_shape(value).filter(TupleRetShape::splats) {
                        Some(shape) => {
                            let r = self.lower_expr_repr(value)?;
                            self.bind_tuple_repr(&vars, &shape, r);
                        }
                        None => {
                            let v = self.lower_expr(value)?;
                            self.bind_for_pattern(&vars, v);
                        }
                    }
                }
                // `let Wrapper(x) = e` — a transparent newtype's runtime value IS
                // the inner, so `x` binds directly to the value.
                Statement::LetNewtype { var, value, .. } => {
                    let v = self.lower_expr(value)?;
                    let cell = self.f.new_cell(Some(self.body.binding_arena[var].name));
                    self.emit_cur(Inst::Store { cell, value: v });
                    self.binds.insert(raw(var), cell);
                }
                Statement::Static { name, value } => {
                    self.lower_static(name, value)?;
                }
                Statement::Fn { name, params, ret_ty, body } => {
                    self.lower_nested_fn(name, params, ret_ty, body)?;
                }
                Statement::Comment(_) => {}
                Statement::While { cond, body } => {
                    if self.lower_while(cond, body)? {
                        return Ok(true);
                    }
                }
                Statement::For { vars, iter, body } => {
                    self.lower_for(vars, iter, body)?;
                }
                Statement::WhileLet {
                    pattern,
                    value,
                    body,
                } => {
                    self.lower_while_let(pattern, value, body)?;
                }
                Statement::Break => {
                    let (_, brk) = self.loop_target("break")?;
                    self.used_break_targets.insert(brk);
                    self.f.set_term(self.cur, Term::Goto(brk));
                    return Ok(true);
                }
                Statement::Continue => {
                    let (cont, _) = self.loop_target("continue")?;
                    self.f.set_term(self.cur, Term::Goto(cont));
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Splice the current (innermost) block scope's pending `defer` bodies in
    /// LIFO order into the current block — the normal block-exit path.
    pub(crate) fn run_top_frame(&mut self) -> Result<(), CompilerError> {
        if let Some(frame) = self.defers.last() {
            for (body, flag) in frame.clone().into_iter().rev() {
                self.lower_defer_splice(body, flag)?;
            }
        }
        Ok(())
    }

    /// Splice EVERY pending defer (all enclosing block scopes, innermost first,
    /// each LIFO) — before a `return` that abandons those scopes.
    pub(crate) fn run_all_defers(&mut self) -> Result<(), CompilerError> {
        let frames: Vec<Vec<(Idx<Expr>, Option<Idx<Cell>>)>> = self.defers.iter().rev().cloned().collect();
        for frame in frames {
            for (body, flag) in frame.into_iter().rev() {
                self.lower_defer_splice(body, flag)?;
            }
        }
        Ok(())
    }

    /// One inline defer splice. With a co-fn terminal flag the splice is
    /// `if flag { flag = false; <body> }` — the SAME guarded shape as the
    /// detached kill copy, so kill-then-splice and splice-then-kill both run
    /// the defer exactly once (a body that kills its own handle mid-run
    /// otherwise double-fires on the trailing scope exit). Flagless (plain fn)
    /// defers splice unguarded.
    pub(crate) fn lower_defer_splice(&mut self, body: Idx<Expr>, flag: Option<Idx<Cell>>) -> Result<(), CompilerError> {
        if let Some(f) = flag {
            let b_body = self.f.new_block();
            let b_done = self.f.new_block();
            let fv = self.emit_cur(Inst::Load(f));
            self.f.set_term(self.cur, Term::Branch { cond: fv, then_bb: b_body, else_bb: b_done });
            self.cur = b_body;
            let off = self.emit_cur(Inst::Const(Lit::Bool(false)));
            self.emit_cur(Inst::Store { cell: f, value: off });
            let was = std::mem::replace(&mut self.in_defer_body, true);
            let res = self.lower_stmt_expr(body);
            self.in_defer_body = was;
            res?;
            self.f.set_term(self.cur, Term::Goto(b_done));
            self.cur = b_done;
        } else {
            let was = std::mem::replace(&mut self.in_defer_body, true);
            let res = self.lower_stmt_expr(body);
            self.in_defer_body = was;
            res?;
        }
        Ok(())
    }

    /// Build the detached terminal-cleanup copy of a co fn `defer` body:
    /// `if flag { flag = false; <body> }` ending in a bare return, in blocks
    /// UNREACHABLE from the fn entry (like closure bodies — the coroutine
    /// transform and DCE leave them alone). `emit_coroutine` compiles each as a
    /// standalone `<name>__defer<k>(s)` fn; the ctor stores them BY VALUE on
    /// the frame so a stale (stamp-killed) coroutine runs ITS OWN layout's
    /// cleanup. Lowered at the registration site with the same binds; the
    /// resolution maps are snapshotted so `let`s inside the copy can't leak
    /// into main-body lowering, and loop targets are cleared so a stray
    /// `break` can't jump into the main CFG.
    pub(crate) fn build_coro_defer_copy(&mut self, body: Idx<Expr>, flag: Idx<Cell>) -> Result<(), CompilerError> {
        let saved_cur = self.cur;
        let saved_binds = self.binds.clone();
        let saved_group = self.group_binds.clone();
        let saved_loops = std::mem::take(&mut self.loop_targets);

        let g_entry = self.f.new_block();
        let b_body = self.f.new_block();
        let b_done = self.f.new_block();
        self.cur = g_entry;
        let fv = self.emit_cur(Inst::Load(flag));
        self.f.set_term(g_entry, Term::Branch { cond: fv, then_bb: b_body, else_bb: b_done });
        self.cur = b_body;
        let off = self.emit_cur(Inst::Const(Lit::Bool(false)));
        self.emit_cur(Inst::Store { cell: flag, value: off });
        let was = std::mem::replace(&mut self.in_defer_body, true);
        let res = self.lower_stmt_expr(body);
        self.in_defer_body = was;
        res?;
        self.f.set_term(self.cur, Term::Goto(b_done));
        self.f.set_term(b_done, Term::Return(Vec::new()));
        self.f.coro_defers.push((flag, g_entry));

        self.cur = saved_cur;
        self.binds = saved_binds;
        self.group_binds = saved_group;
        self.loop_targets = saved_loops;
        Ok(())
    }

    pub(crate) fn lower_block_repr(
        &mut self,
        statements: comfy_la_arena::IdxRange<Statement>,
        tail: Option<Idx<Expr>>,
    ) -> Result<ValueRepr, CompilerError> {
        self.defers.push(Vec::new());
        let terminated = self.lower_stmts(statements)?;
        if terminated {
            self.defers.pop();
            return Ok(ValueRepr::Scalar(self.emit_cur(Inst::Const(Lit::Nil))));
        }
        let repr = match tail {
            Some(tail) => self.lower_expr_repr(tail)?,
            None => ValueRepr::Scalar(self.emit_cur(Inst::Const(Lit::Nil))),
        };
        self.run_top_frame()?;
        self.defers.pop();
        Ok(repr)
    }

    /// Lower an expression in statement position (value discarded). A tail-less
    /// block lowers to nothing (no dead unit); side-effecting tails still run.
    pub(crate) fn lower_stmt_expr(&mut self, e: Idx<Expr>) -> Result<(), CompilerError> {
        match self.body.expr_arena[e].clone() {
            Expr::Block { statements, tail } => {
                self.defers.push(Vec::new());
                let terminated = self.lower_stmts(statements)?;
                if !terminated {
                    if let Some(t) = tail {
                        // The tail is itself statement-position: a tail `if`/`match`
                        // skips its result cell too.
                        self.lower_stmt_expr(t)?;
                    }
                    self.run_top_frame()?;
                }
                self.defers.pop();
                Ok(())
            }
            // Control flow in statement position: lower WITHOUT a result cell (the
            // discarded if/match result + its store/load is the bulk of skull's
            // trace-hostile dead-cell soup). Arms recurse statement-position.
            Expr::If { cond, then_branch, else_branch } => {
                self.lower_if_stmt(cond, then_branch, else_branch)
            }
            Expr::Match { scrutinee, arms } => self.lower_match_stmt(scrutinee, arms),
            // Assignments are statements (unit value); lower the effect directly
            // so statement position leaves no dead unit.
            Expr::Assign { target, value } => self.lower_assign_effect(target, value),
            Expr::CompoundAssign {
                target,
                op_fn,
                value,
            } => self.lower_compound_assign_effect(e, target, op_fn, value),
            _ => {
                self.lower_expr(e)?;
                Ok(())
            }
        }
    }
}
