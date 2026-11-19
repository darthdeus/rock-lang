use super::call::{DirectCall, call_argument_parameter, call_uses_parameter_slots};
use super::*;
use crate::instance::Instance;
use bone_core::hir_walk::HirChild;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum FlatStorage {
    /// One column per dotted leaf path, `vec[path][slot]`.
    Columns,
    /// One `_data` row of `stride` slots per element.
    Strided,
}

impl FlatStorage {
    pub(crate) fn of_vec_kind(kind: ContainerKind) -> Option<(ColumnElem, Self)> {
        match kind {
            ContainerKind::VecValueColumns(elem) => Some((elem, Self::Columns)),
            ContainerKind::VecValueStrided(elem) => Some((elem, Self::Strided)),
            _ => None,
        }
    }

    pub(crate) fn vec_kind(self, elem: ColumnElem) -> ContainerKind {
        match self {
            Self::Columns => ContainerKind::VecValueColumns(elem),
            Self::Strided => ContainerKind::VecValueStrided(elem),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct FlatRefKey {
    pub(crate) elem: ColumnElem,
    pub(crate) storage: FlatStorage,
    pub(crate) prefix: Ustr,
}

impl FlatRefKey {
    fn child(self, field: Ustr) -> Self {
        let prefix = if self.prefix.is_empty() {
            field
        } else {
            crate::names::ustr(&format!("{}.{field}", self.prefix))
        };
        Self { prefix, ..self }
    }
}

pub(crate) type FlatSig = Vec<Option<FlatRefKey>>;

#[derive(Clone, Copy)]
pub(crate) struct FlatRefVal {
    pub(crate) vec: Idx<Inst>,
    pub(crate) slot: Idx<Inst>,
    pub(crate) key: FlatRefKey,
}

pub(crate) struct FlatCallPlan {
    inst: Instance,
    sig: FlatSig,
    args: Vec<(Idx<Expr>, usize, FlatRefKey)>,
    ret: Option<FlatRefKey>,
}

pub(crate) struct FlatArgCapture {
    expected: Vec<(Idx<Expr>, usize, FlatRefKey)>,
    slots: Vec<(usize, Idx<Inst>)>,
}

pub(crate) enum FlatCallOut {
    Flat(FlatRefVal),
    Value(ValueRepr),
}

#[derive(Default)]
pub(crate) struct FlatVariants {
    root: Option<Instance>,
    counter: u32,
    by_key: HashMap<(Instance, FlatSig), Ustr>,
    funcs: Vec<MirFunc>,
    returns: HashMap<(Instance, FlatSig), Option<FlatRefKey>>,
    scanning: Vec<ReturnScan>,
}

struct ReturnScan {
    key: (Instance, FlatSig),
    assumed: Option<Option<FlatRefKey>>,
    depends_on: Option<usize>,
    unknown_reads: u32,
}

pub(crate) type FlatVariantsHandle = Rc<RefCell<FlatVariants>>;

impl FlatVariants {
    pub(crate) fn for_root(root: Instance) -> FlatVariantsHandle {
        Rc::new(RefCell::new(Self {
            root: Some(root),
            ..Self::default()
        }))
    }

    pub(crate) fn take_funcs(&mut self) -> Vec<MirFunc> {
        std::mem::take(&mut self.funcs)
    }
}

fn is_phantom_call(body: &Body, e: Idx<Expr>) -> bool {
    matches!(
        &body.expr_arena[e],
        Expr::Call { callee: Callee::Unresolved { name, .. }, args, .. }
            if name.as_str() == "phantom" && (*args).into_iter().next().is_none()
    )
}

fn scan_returns(
    body: &Body,
    node: HirChild,
    returns: &mut Vec<Idx<Expr>>,
    lets: &mut Vec<(Idx<BindingInfo>, Idx<Expr>)>,
) {
    match node {
        HirChild::Expr(e) => match body.expr_arena[e] {
            Expr::Lambda { .. } => return,
            Expr::Return(Some(value)) => returns.push(value),
            _ => {}
        },
        HirChild::Statement(s) => match body.statement_arena[s] {
            Statement::Fn { .. } => return,
            Statement::Let { pattern, value, .. } => {
                if let bone_core::hir::Pattern::Bind(name) = body.pattern_arena[pattern] {
                    lets.push((name, value));
                }
            }
            Statement::Return(Some(value)) => returns.push(value),
            _ => {}
        },
        HirChild::Pattern(_) => return,
    }
    body.for_each_child(node, |child| scan_returns(body, child, returns, lets));
}

impl<'a> Builder<'a> {
    pub(crate) fn flat_container_elem(
        &self,
        container: Idx<Expr>,
    ) -> Result<Option<(ColumnElem, FlatStorage)>, CompilerError> {
        if let Some(flat) = self.vec_flat_elem_of(container)? {
            return Ok(Some(flat));
        }
        Ok(self
            .map_column_value_of(container)?
            .map(|elem| (elem, FlatStorage::Columns)))
    }

    pub(crate) fn flat_target_shape(&self, key: &FlatRefKey) -> Result<ValueShape, CompilerError> {
        let mut shape = self.column_shape(key.elem)?;
        if key.prefix.is_empty() {
            return Ok(shape);
        }
        for seg in key.prefix.as_str().split('.') {
            shape = match shape.fields.iter().find(|(name, _)| name.as_str() == seg) {
                Some((_, FieldShape::Nested(sub))) => sub.clone(),
                _ => {
                    return Err(self.gap_here(format!(
                        "flat `ref` path `{}` does not name a nested value of `{}`",
                        key.prefix, key.elem.name
                    )));
                }
            };
        }
        Ok(shape)
    }

    fn is_value_ref_type(&self, ty: &MonoTy) -> bool {
        match ty.peel_nullable().0 {
            MonoTy::App {
                ctor: MonoCtor::Ref,
                params,
            } => params
                .first()
                .and_then(|target| target.nominal())
                .is_some_and(|name| self.is_value_struct(name)),
            _ => false,
        }
    }

    pub(crate) fn classify_flat_ref(
        &self,
        e: Idx<Expr>,
    ) -> Result<Option<FlatRefKey>, CompilerError> {
        match self.body.expr_arena[e] {
            Expr::Index { base, .. } => Ok(self
                .flat_container_elem(base)?
                .filter(|(elem, _)| !elem.nullable)
                .map(|(elem, storage)| FlatRefKey {
                    elem,
                    storage,
                    prefix: crate::names::ustr(""),
                })),
            Expr::FieldAccess { base, field } => {
                let Some(key) = self.classify_flat_ref(base)? else {
                    return Ok(None);
                };
                let shape = self.flat_target_shape(&key)?;
                Ok(match shape.fields.iter().find(|(name, _)| *name == field) {
                    Some((_, FieldShape::Nested(sub))) if !sub.nullable => Some(key.child(field)),
                    _ => None,
                })
            }
            Expr::Var(b) => {
                if let Some(key) = self.flat_ref_binds.get(&raw(b)) {
                    return Ok(Some(*key));
                }
                match self.flat_scan_lets.get(&raw(b)) {
                    Some(&init) => self.classify_flat_ref(init),
                    None => Ok(None),
                }
            }
            Expr::Call { .. } => Ok(self.flat_call_plan(e)?.and_then(|plan| plan.ret)),
            _ => Ok(None),
        }
    }

    pub(crate) fn flat_call_plan(
        &self,
        e: Idx<Expr>,
    ) -> Result<Option<FlatCallPlan>, CompilerError> {
        let Expr::Call {
            callee: Callee::Unresolved { name, .. },
            args,
            ..
        } = self.body.expr_arena[e]
        else {
            return Ok(None);
        };
        let arg_idxs: Vec<Idx<Expr>> = args.into_iter().collect();
        let mut ref_args = Vec::new();
        for &a in &arg_idxs {
            if self.flat_ref_arg_candidate(e, a)? {
                ref_args.push(a);
            }
        }
        let has_ref_arg = !ref_args.is_empty();
        let returns_ref = self
            .concrete_ty(e)?
            .is_some_and(|ty| self.is_value_ref_type(ty));
        if !has_ref_arg && !returns_ref {
            return Ok(None);
        }
        if self.inf.is_some_and(|inf| {
            inf.bound_identity_calls.contains(&e) || inf.bound_calls.contains_key(&e)
        }) {
            return Ok(None);
        }
        let Ok(func) = self.resolve_funcref(e, name) else {
            return Ok(None);
        };
        if self.fn_expand_kind(func).is_some() {
            return Ok(None);
        }
        let decl = &self.ws.module(func.module).functions[func.func];
        let mut sig: FlatSig = vec![None; decl.param_decls.len()];
        let mut flat_args = Vec::new();
        let mut next_pos = 0usize;
        for &a in &arg_idxs {
            let Some(pidx) = call_argument_parameter(self.body, a, &mut next_pos, |name| {
                decl.param_decls.iter().position(|p| p.name == name)
            }) else {
                continue;
            };
            if pidx >= sig.len() || !ref_args.contains(&a) {
                continue;
            }
            if let Some(key) = self.classify_flat_ref(a)? {
                sig[pidx] = Some(key);
                flat_args.push((a, pidx, key));
            }
        }
        if flat_args.is_empty() && !returns_ref {
            return Ok(None);
        }
        let inst = self
            .concrete_callee_for_site_op(e, crate::site::SiteOp::DirectCall)?
            .ok_or_else(|| self.gap_here("flat `ref` call has no concrete callee identity"))?;
        let ret = if returns_ref {
            self.flat_return_of(&inst, &sig)?
        } else {
            None
        };
        if flat_args.is_empty() && ret.is_none() {
            return Ok(None);
        }
        Ok(Some(FlatCallPlan {
            inst,
            sig,
            args: flat_args,
            ret,
        }))
    }

    fn flat_ref_arg_candidate(&self, e: Idx<Expr>, a: Idx<Expr>) -> Result<bool, CompilerError> {
        if self.is_ref_call_arg(e, a) {
            return Ok(true);
        }
        Ok(match self.body.expr_arena[a] {
            Expr::Var(b) => {
                self.flat_ref_binds.contains_key(&raw(b))
                    || self.flat_scan_lets.contains_key(&raw(b))
            }
            Expr::Call { .. } => self
                .concrete_ty(a)?
                .is_some_and(|ty| self.is_value_ref_type(ty)),
            _ => false,
        })
    }

    pub(crate) fn is_flat_ref_call_arg(&self, a: Idx<Expr>) -> bool {
        self.flat_arg_captures
            .last()
            .is_some_and(|capture| capture.expected.iter().any(|(arg, _, _)| *arg == a))
    }

    fn flat_return_of(
        &self,
        inst: &Instance,
        sig: &[Option<FlatRefKey>],
    ) -> Result<Option<FlatRefKey>, CompilerError> {
        let key = (inst.clone(), sig.to_vec());
        {
            let mut reg = self.flat_variants.borrow_mut();
            if let Some(ret) = reg.returns.get(&key) {
                return Ok(*ret);
            }
            if let Some(i) = reg.scanning.iter().position(|scan| scan.key == key) {
                let assumed = reg.scanning[i].assumed;
                let reader = reg.scanning.last_mut().expect("a scan is in progress");
                reader.depends_on = Some(reader.depends_on.map_or(i, |d| d.min(i)));
                if assumed.is_none() {
                    reader.unknown_reads += 1;
                }
                return Ok(assumed.flatten());
            }
        }
        let Some(plan) = self.concrete_plans.get(inst) else {
            return Ok(None);
        };
        self.flat_return_scan(inst, plan, sig)
    }

    fn flat_return_scan(
        &self,
        inst: &Instance,
        plan: &'a ConcreteBodyPlan,
        sig: &[Option<FlatRefKey>],
    ) -> Result<Option<FlatRefKey>, CompilerError> {
        let returns_value_ref = plan
            .signature
            .result
            .is_some_and(|result| self.is_value_ref_type(self.concrete_mono(result)));
        if !returns_value_ref {
            return Ok(None);
        }
        let module = self.ws.module(inst.func.module);
        let decl = &module.functions[inst.func.func];
        if decl.inline_body.is_some() {
            return Ok(None);
        }
        let Some(body) = module.bodies.get(&BodyOwner::Function(inst.func.func)) else {
            return Ok(None);
        };
        let inf = self
            .ws
            .inference(inst.func.module)
            .and_then(|ir| ir.per_fn.get(&BodyOwner::Function(inst.func.func)));
        let mut scan = Builder::new(
            self.ws,
            self.scx,
            self.call_scx,
            plan,
            self.concrete_plans,
            body,
            inf,
            MirFunc::new(decl.name),
            self.flat_variants.clone(),
        );
        scan.generic_bounds = &decl.generic_bounds;
        scan.site_owner = Some(inst.clone());
        for (i, pb) in body.param_bindings.into_iter().enumerate() {
            if let Some(Some(key)) = sig.get(i) {
                scan.flat_ref_binds.insert(raw(pb), *key);
            }
        }
        let mut returns = Vec::new();
        let mut lets = Vec::new();
        if let Some(root) = body.root {
            match body.expr_arena[root] {
                Expr::Block { tail: Some(t), .. } => returns.push(t),
                Expr::Block { tail: None, .. } => {}
                _ => returns.push(root),
            }
            scan_returns(body, HirChild::Expr(root), &mut returns, &mut lets);
        }
        for (name, value) in lets {
            if scan
                .binding_ty(name)
                .and_then(|ty| scan.concrete_ty_id(ty))
                .is_some_and(|ty| {
                    matches!(
                        ty.peel_nullable().0,
                        MonoTy::App {
                            ctor: MonoCtor::Ref,
                            ..
                        }
                    )
                })
            {
                scan.flat_scan_lets.insert(raw(name), value);
            }
        }
        let key = (inst.clone(), sig.to_vec());
        let depth = {
            let mut reg = self.flat_variants.borrow_mut();
            reg.scanning.push(ReturnScan {
                key: key.clone(),
                assumed: None,
                depends_on: None,
                unknown_reads: 0,
            });
            reg.scanning.len() - 1
        };
        let result = scan.settle_flat_returns(decl, &returns, depth);
        let mut reg = self.flat_variants.borrow_mut();
        let done = reg.scanning.pop().expect("pushed above");
        let answer = result?;
        match done.depends_on {
            Some(d) if d < depth => {
                let reader = reg.scanning.last_mut().expect("an enclosing scan");
                reader.depends_on = Some(reader.depends_on.map_or(d, |r| r.min(d)));
                if answer.is_none() {
                    reader.unknown_reads += 1;
                }
            }
            _ => {
                reg.returns.insert(key, answer.flatten());
            }
        }
        Ok(answer.flatten())
    }

    fn settle_flat_returns(
        &self,
        decl: &Function,
        returns: &[Idx<Expr>],
        depth: usize,
    ) -> Result<Option<Option<FlatRefKey>>, CompilerError> {
        loop {
            let answer = self.agree_flat_returns(decl, returns, depth)?;
            let mut reg = self.flat_variants.borrow_mut();
            let scan = &mut reg.scanning[depth];
            match scan.depends_on {
                Some(d) if d < depth => return Ok(answer),
                Some(_) if answer != scan.assumed => {
                    scan.assumed = answer;
                    scan.depends_on = None;
                    scan.unknown_reads = 0;
                }
                _ => return Ok(answer),
            }
        }
    }

    fn agree_flat_returns(
        &self,
        decl: &Function,
        returns: &[Idx<Expr>],
        depth: usize,
    ) -> Result<Option<Option<FlatRefKey>>, CompilerError> {
        let unknown_reads = || self.flat_variants.borrow().scanning[depth].unknown_reads;
        let mut agreed = self.flat_variants.borrow().scanning[depth].assumed;
        for &r in returns {
            if matches!(self.body.expr_arena[r], Expr::Null) || is_phantom_call(self.body, r) {
                continue;
            }
            let before = unknown_reads();
            let key = self.classify_flat_ref(r)?;
            if unknown_reads() != before {
                continue;
            }
            match agreed {
                None => agreed = Some(key),
                Some(prev) if prev == key => {}
                Some(_) => {
                    return Err(CompilerError::new_with_label(
                        decl.name_span,
                        format!(
                            "`{}` returns a `ref` to a flat-stored container element on one path \
                             and a differently stored reference on another; the Lua backend \
                             passes such a `ref` as the container and a slot, so every return \
                             must address the same storage",
                            decl.name
                        ),
                    ));
                }
            }
        }
        Ok(agreed)
    }

    pub(crate) fn own_flat_return(
        &self,
        subject: &Instance,
    ) -> Result<Option<FlatRefKey>, CompilerError> {
        let decl = &self.ws.module(subject.func.module).functions[subject.func.func];
        let sig: FlatSig = vec![None; decl.param_decls.len()];
        self.flat_return_scan(subject, self.concrete_plan, &sig)
    }

    pub(crate) fn reject_flat_return_outside_direct_call(
        &self,
        e: Idx<Expr>,
        inst: &Instance,
    ) -> Result<(), CompilerError> {
        let decl = &self.ws.module(inst.func.module).functions[inst.func.func];
        let sig: FlatSig = vec![None; decl.param_decls.len()];
        if self.flat_return_of(inst, &sig)?.is_none() {
            return Ok(());
        }
        Err(CompilerError::new_with_label(
            self.body.expr_spans[e],
            format!(
                "`{}` returns a `ref` to a flat-stored container element, which only a direct \
                 call can receive on this backend",
                decl.name
            ),
        ))
    }

    pub(crate) fn lower_flat_ref(&mut self, e: Idx<Expr>) -> Result<FlatRefVal, CompilerError> {
        let key = self
            .classify_flat_ref(e)?
            .ok_or_else(|| self.gap_here("lowering a flat `ref` of a non-flat place"))?;
        match self.body.expr_arena[e] {
            Expr::Index { .. } => self.lower_flat_index_place(e, key),
            Expr::FieldAccess { base, .. } => {
                let base = self.lower_flat_ref(base)?;
                Ok(FlatRefVal { key, ..base })
            }
            Expr::Var(b) => {
                let (vec, slot) = *self
                    .flat_ref_cells
                    .get(&raw(b))
                    .ok_or_else(|| self.gap_here("flat `ref` binding has no cells"))?;
                let vec = self.emit_cur(Inst::Load(vec));
                let slot = self.emit_cur(Inst::Load(slot));
                Ok(FlatRefVal { vec, slot, key })
            }
            Expr::Call {
                callee: Callee::Unresolved { name, .. },
                args,
                ..
            } => {
                let outer = self.cur_span.replace(self.body.expr_spans[e]);
                let outer_expr = self.cur_expr.replace(e);
                let out = self.lower_flat_call_expr(e, name, args);
                self.cur_span = outer;
                self.cur_expr = outer_expr;
                match out? {
                    FlatCallOut::Flat(val) => Ok(val),
                    FlatCallOut::Value(_) => Err(self.gap_here("flat `ref` call returned a value")),
                }
            }
            _ => Err(self.gap_here("flat `ref` of an unsupported expression")),
        }
    }

    fn lower_flat_call_expr(
        &mut self,
        e: Idx<Expr>,
        name: Ustr,
        args: IdxRange<Expr>,
    ) -> Result<FlatCallOut, CompilerError> {
        let func = self.resolve_funcref(e, name)?;
        let arg_idxs: Vec<Idx<Expr>> = args.into_iter().collect();
        let any_named = call_uses_parameter_slots(self.body, &arg_idxs);
        let dc = self.plan_direct_call(e)?;
        let plan = self
            .flat_call_plan(e)?
            .ok_or_else(|| self.gap_here("flat `ref` call has no flat plan"))?;
        self.lower_flat_call(e, func, &arg_idxs, any_named, &dc, &plan, None)
    }

    /// Flat arguments go first as slots, in parameter order; each one's own
    /// argument position holds its vec. A flat result arrives as `vec, slot`.
    pub(crate) fn lower_flat_call(
        &mut self,
        e: Idx<Expr>,
        func: FuncRef,
        arg_idxs: &[Idx<Expr>],
        any_named: bool,
        dc: &DirectCall<'a>,
        plan: &FlatCallPlan,
        ret_tuple: Option<&TupleLayout>,
    ) -> Result<FlatCallOut, CompilerError> {
        if plan.inst != dc.callee_inst {
            return Err(self.gap_here("flat `ref` call disagrees with its concrete callee"));
        }
        self.flat_arg_captures.push(FlatArgCapture {
            expected: plan.args.clone(),
            slots: Vec::new(),
        });
        let args = self.direct_call_args(e, func, arg_idxs, any_named, dc, false);
        let capture = self.flat_arg_captures.pop().expect("pushed above");
        let (args, _) = args?;
        if capture.slots.len() != plan.args.len() {
            return Err(self.gap_here("a flat `ref` argument did not reach its `ref` parameter"));
        }
        let mut slots = capture.slots;
        slots.sort_by_key(|&(pidx, _)| pidx);
        if plan.args.is_empty() {
            let key = plan
                .ret
                .expect("a flat plan has flat arguments or a flat result");
            let dests = vec![self.f.new_cell(None), self.f.new_cell(None)];
            let (call, loads) = self.call_multi(dests, func, args);
            self.record_call(call, func, dc)?;
            self.f.flat_call_lanes.insert(call, 2);
            let [vec, slot] = loads[..] else {
                unreachable!()
            };
            return Ok(FlatCallOut::Flat(FlatRefVal { vec, slot, key }));
        }
        let hoisted = self.flat_variant(plan, dc)?;
        if !self.f.hoisted_fn_globals.contains(&hoisted) {
            self.f.hoisted_fn_globals.push(hoisted);
        }
        let callee = self.emit_cur(Inst::Global(hoisted));
        let mut argv: Vec<Idx<Inst>> = slots.into_iter().map(|(_, slot)| slot).collect();
        argv.extend(args);
        let multi = |s: &mut Self, dests: Vec<Idx<Cell>>, argv| {
            let lanes = dests.len();
            let (call, loads) = s.call_value_multi(dests, callee, argv);
            s.f.flat_call_lanes.insert(call, lanes);
            loads
        };
        if let Some(key) = plan.ret {
            let dests = vec![self.f.new_cell(None), self.f.new_cell(None)];
            let [vec, slot] = multi(self, dests, argv)[..] else {
                unreachable!()
            };
            return Ok(FlatCallOut::Flat(FlatRefVal { vec, slot, key }));
        }
        let ret_shape = if dc.plan.uses_scalar_abi() {
            dc.plan.ret_shape()
        } else {
            None
        };
        if let Some(shape) = ret_shape {
            let dests = self.fresh_cells(shape.leaf_count());
            let leaves = multi(self, dests, argv);
            return Ok(FlatCallOut::Value(Self::rebuild_from_leaves(
                shape, &leaves, &mut 0,
            )));
        }
        if let Some(shape) = ret_tuple {
            let dests: Vec<Idx<Cell>> = self
                .tuple_opaque_slot_types(e, shape)
                .into_iter()
                .map(|ty| self.f.new_cell_typed(None, ty))
                .collect();
            let loads = multi(self, dests, argv);
            return Ok(FlatCallOut::Value(self.tuple_result(shape, &loads)));
        }
        Ok(FlatCallOut::Value(ValueRepr::Scalar(
            self.emit_cur(Inst::CallValue { callee, args: argv }),
        )))
    }

    fn flat_variant(
        &mut self,
        plan: &FlatCallPlan,
        dc: &DirectCall<'a>,
    ) -> Result<Ustr, CompilerError> {
        let key = (plan.inst.clone(), plan.sig.clone());
        let (hoisted, identity) = {
            let mut reg = self.flat_variants.borrow_mut();
            if let Some(name) = reg.by_key.get(&key) {
                return Ok(*name);
            }
            let Some(root) = reg.root.clone() else {
                return Err(self.gap_here(
                    "a flat-stored element `ref` argument in a body that cannot host a flat variant",
                ));
            };
            let n = reg.counter;
            reg.counter += 1;
            let name = crate::names::ustr(&format!(
                "__flatref_{}_{n}",
                root.symbol(self.ws, self.scx.interner())
            ));
            let identity = crate::code_identity::CodeIdentity::Generated {
                enclosing: crate::code_identity::CodeOwner::Instance(root),
                role: name,
            };
            reg.by_key.insert(key, name);
            (name, identity)
        };
        let mf = self.lower_flat_variant(hoisted, &identity, plan, dc)?;
        self.f.lower_attribution.add_assign(&mf.lower_attribution);
        self.flat_variants.borrow_mut().funcs.push(mf);
        Ok(hoisted)
    }

    fn lower_flat_variant(
        &self,
        hoisted: Ustr,
        identity: &crate::code_identity::CodeIdentity,
        plan: &FlatCallPlan,
        dc: &DirectCall<'a>,
    ) -> Result<MirFunc, CompilerError> {
        let inst = &plan.inst;
        let module = self.ws.module(inst.func.module);
        let decl = &module.functions[inst.func.func];
        let unsupported = |why: &str| {
            Err(self.gap_here(format!(
                "a flat-stored element `ref` cannot be passed to `{}`: {why}",
                decl.name
            )))
        };
        if decl.inline_body.is_some() {
            return unsupported("its body is inline Lua");
        }
        if decl.is_coroutine {
            return Err(self.located(CompilerError::new_no_location(format!(
                "cannot pass a reference to a flat-stored container element to coroutine `{}`; \
                 pass the container and index instead",
                decl.name
            ))));
        }
        let Some(body) = module.bodies.get(&BodyOwner::Function(inst.func.func)) else {
            return unsupported("it has no body");
        };
        let Some(concrete_plan) = self.concrete_plans.get(inst) else {
            return unsupported("it has no concrete body plan");
        };
        let inf = self
            .ws
            .inference(inst.func.module)
            .and_then(|ir| ir.per_fn.get(&BodyOwner::Function(inst.func.func)));
        let mut mf = MirFunc::new(hoisted);
        mf.rep_arena = Some(self.scx.rep_arena().clone());
        mf.flat_variant_source = Some(inst.func);
        let flat_abi = plan
            .sig
            .iter()
            .map(|key| match key {
                None => "boxed".to_string(),
                Some(key) => format!(
                    "elem={};nullable={};nobox={};storage={:?};prefix={};{}",
                    bone_core::modules::artifact_name(key.elem.name.as_str()),
                    key.elem.nullable,
                    key.elem.is_nobox(),
                    key.storage,
                    key.prefix,
                    abi::rep_logical_fingerprint(self.scx.rep_arena(), key.elem.representation()),
                ),
            })
            .collect::<Vec<_>>()
            .join(",");
        let call_abi = format!("{}|flat [{flat_abi}]", dc.plan.logical_fingerprint());
        mf.code_identity = Some(identity.clone());
        mf.code_abi_fingerprint = Some(call_abi.clone());
        mf.code_compatibility = Some(crate::code_identity::CodeCompatibility {
            call_abi,
            capture: crate::code_identity::CaptureSchema::empty(),
            frame: None,
            lambda_topology: Default::default(),
        });
        mf.nested_fn_identities = self.f.nested_fn_identities.clone();
        let mut child = Builder::new(
            self.ws,
            self.scx,
            self.call_scx,
            concrete_plan,
            self.concrete_plans,
            body,
            inf,
            mf,
            self.flat_variants.clone(),
        );
        child.generic_bounds = &decl.generic_bounds;
        child.defer_mode = stmt::DeferMode::for_body(false, self.call_scx.target);
        child.param_plan = Some(dc.plan.clone());
        child.ret_rep = dc.plan.representation().result();
        child.fn_name_span = Some(decl.name_span);
        child.site_owner = Some(inst.clone());
        child.static_owner = static_owner_tag(self.ws, self.scx, inf, inst);
        child.flat_ret = plan.ret;
        let params: Vec<Idx<BindingInfo>> = body.param_bindings.into_iter().collect();
        let mut slot_cells = Vec::new();
        for (i, &pb) in params.iter().enumerate() {
            if plan.sig.get(i).copied().flatten().is_some() {
                let name = body.binding_arena[pb].name;
                let cell = child.new_binding_cell(
                    pb,
                    crate::names::ustr(&format!("{name}__slot")),
                    None,
                    crate::names::ustr("slot"),
                );
                child.f.cells[cell].producer = crate::mir::CellProducer::Param;
                child.f.params.push(cell);
                slot_cells.push(cell);
            }
        }
        let mut slot_cells = slot_cells.into_iter();
        for (i, &pb) in params.iter().enumerate() {
            let name = body.binding_arena[pb].name;
            match plan.sig.get(i).copied().flatten() {
                Some(key) => {
                    let vec = child.new_binding_cell(
                        pb,
                        crate::names::ustr(&format!("{name}__vec")),
                        None,
                        crate::names::ustr("vec"),
                    );
                    child.f.cells[vec].producer = crate::mir::CellProducer::Param;
                    child.f.params.push(vec);
                    let slot = slot_cells.next().expect("one slot cell per flat parameter");
                    child.flat_ref_binds.insert(raw(pb), key);
                    child.flat_ref_cells.insert(raw(pb), (vec, slot));
                }
                None => {
                    let slot = child.param_slot(i)?;
                    child.bind_param_slot(pb, name, slot)?;
                }
            }
        }
        child.fill_param_defaults()?;
        child.copy_ref_borrowed_params(&params)?;
        child.lower_root()?;
        for cell in &mut child.f.statics {
            cell.owner = Some(crate::code_identity::CodeIdentity::Named(inst.clone()));
        }
        Ok(finish_lower_attribution(child.f))
    }

    pub(crate) fn capture_flat_ref_arg(
        &mut self,
        e: Idx<Expr>,
    ) -> Result<Option<Idx<Inst>>, CompilerError> {
        let Some(&(_, pidx, _)) = self
            .flat_arg_captures
            .last()
            .and_then(|capture| capture.expected.iter().find(|(a, _, _)| *a == e))
        else {
            return Ok(None);
        };
        let val = self.lower_flat_ref(e)?;
        self.flat_arg_captures
            .last_mut()
            .expect("found above")
            .slots
            .push((pidx, val.slot));
        Ok(Some(val.vec))
    }

    pub(crate) fn lower_flat_let(
        &mut self,
        name: Idx<BindingInfo>,
        value: Idx<Expr>,
    ) -> Result<bool, CompilerError> {
        let is_ref = self
            .binding_ty(name)
            .and_then(|ty| self.concrete_ty_id(ty))
            .is_some_and(|ty| {
                matches!(
                    ty.peel_nullable().0,
                    MonoTy::App {
                        ctor: MonoCtor::Ref,
                        ..
                    }
                )
            });
        if !is_ref {
            return Ok(false);
        }
        let Some(key) = self.classify_flat_ref(value)? else {
            return Ok(false);
        };
        let val = self.lower_flat_ref(value)?;
        let bname = self.body.binding_arena[name].name;
        let vec = self.new_binding_cell(
            name,
            crate::names::ustr(&format!("{bname}__vec")),
            None,
            crate::names::ustr("vec"),
        );
        let slot = self.new_binding_cell(
            name,
            crate::names::ustr(&format!("{bname}__slot")),
            None,
            crate::names::ustr("slot"),
        );
        self.store_cell(vec, val.vec);
        self.store_cell(slot, val.slot);
        self.flat_ref_binds.insert(raw(name), key);
        self.flat_ref_cells.insert(raw(name), (vec, slot));
        Ok(true)
    }

    pub(crate) fn flat_return_leaves(
        &mut self,
        e: Idx<Expr>,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        if matches!(self.body.expr_arena[e], Expr::Null) || is_phantom_call(self.body, e) {
            if is_phantom_call(self.body, e) {
                self.lower_expr(e)?;
            }
            let nil = self.emit_cur(Inst::Const(Lit::Nil));
            return Ok(vec![nil, nil]);
        }
        let val = self.lower_flat_ref(e)?;
        Ok(vec![val.vec, val.slot])
    }

    /// `x == null` / `x != null` over a flat `ref`: absent is a nil vec.
    pub(crate) fn flat_ref_presence(
        &mut self,
        v: Idx<Expr>,
        op: BinOp,
    ) -> Result<Option<Idx<Inst>>, CompilerError> {
        if self.classify_flat_ref(v)?.is_none() {
            return Ok(None);
        }
        let val = self.lower_flat_ref(v)?;
        let nil = self.emit_cur(Inst::Const(Lit::Nil));
        Ok(Some(self.emit_cur(if matches!(op, BinOp::Eq) {
            Inst::Eq(val.vec, nil)
        } else {
            Inst::NotEq(val.vec, nil)
        })))
    }

    fn flat_leaf_keys(&self, key: &FlatRefKey, shape: &ValueShape) -> Vec<Ustr> {
        leaf_names(shape, key.prefix.as_str(), ".")
            .iter()
            .map(|k| crate::names::ustr(k))
            .collect()
    }

    pub(crate) fn flat_leaf_load(&mut self, place: FlatRefVal) -> Result<Idx<Inst>, CompilerError> {
        let (base, index, kind) = self.flat_leaf_slot(place)?;
        let value = self.read_index(base, index, kind);
        Ok(
            if kind == ContainerKind::VecStrideSlot && !self.strided_stores_raw(place.key.elem) {
                self.emit_cur(Inst::VecStrideFalseToNil(value))
            } else {
                value
            },
        )
    }

    pub(crate) fn flat_leaf_store(
        &mut self,
        place: FlatRefVal,
        value: Idx<Inst>,
    ) -> Result<(), CompilerError> {
        let (base, index, kind) = self.flat_leaf_slot(place)?;
        let value =
            if kind == ContainerKind::VecStrideSlot && !self.strided_stores_raw(place.key.elem) {
                self.emit_cur(Inst::VecStrideNilToFalse(value))
            } else {
                value
            };
        self.write_index(base, index, value, kind);
        Ok(())
    }

    fn flat_leaf_slot(
        &mut self,
        place: FlatRefVal,
    ) -> Result<(Idx<Inst>, Idx<Inst>, ContainerKind), CompilerError> {
        let FlatRefVal { vec, slot, key } = place;
        match key.storage {
            FlatStorage::Columns => {
                let col = self.read_field(vec, key.prefix);
                Ok((col, slot, ContainerKind::VecColumn))
            }
            FlatStorage::Strided => {
                let (at, data) = self.flat_strided_slot(vec, slot, key.elem, key.prefix)?;
                Ok((data, at, ContainerKind::VecStrideSlot))
            }
        }
    }

    fn flat_strided_slot(
        &mut self,
        vec: Idx<Inst>,
        slot: Idx<Inst>,
        elem: ColumnElem,
        path: Ustr,
    ) -> Result<(Idx<Inst>, Idx<Inst>), CompilerError> {
        let shape = self.column_shape(elem)?;
        let offset = value_leaf_paths(&shape)
            .iter()
            .position(|p| *p == path)
            .ok_or_else(|| {
                self.gap_here(format!(
                    "`{path}` does not name a slot of strided `[{}]`",
                    elem.name
                ))
            })?;
        let data = self.read_field(vec, crate::names::ustr("_data"));
        let at = self.strided_slot_index(slot, shape.leaf_count(), offset);
        Ok((at, data))
    }

    pub(crate) fn flat_root_place(
        &mut self,
        root: Idx<Expr>,
        prefix: Ustr,
    ) -> Result<Option<FlatRefVal>, CompilerError> {
        if let Expr::Index { base, .. } = self.body.expr_arena[root] {
            let Some((elem, storage)) = self.flat_container_elem(base)? else {
                return Ok(None);
            };
            let key = FlatRefKey {
                elem,
                storage,
                prefix,
            };
            return self.lower_flat_index_place(root, key).map(Some);
        }
        if self.classify_flat_ref(root)?.is_none() {
            return Ok(None);
        }
        let place = self.lower_flat_ref(root)?;
        Ok(Some(FlatRefVal {
            key: FlatRefKey {
                prefix,
                ..place.key
            },
            ..place
        }))
    }

    pub(crate) fn flat_ref_root_key(
        &self,
        root: Idx<Expr>,
    ) -> Result<Option<FlatRefKey>, CompilerError> {
        match self.body.expr_arena[root] {
            Expr::Var(b) if self.flat_ref_binds.contains_key(&raw(b)) => {
                self.classify_flat_ref(root)
            }
            Expr::Call { .. } => self.classify_flat_ref(root),
            _ => Ok(None),
        }
    }

    pub(crate) fn flat_ref_read(
        &mut self,
        e: Idx<Expr>,
    ) -> Result<Option<ValueBundle>, CompilerError> {
        match self.body.expr_arena[e] {
            Expr::Var(b) if self.flat_ref_binds.contains_key(&raw(b)) => {
                if self
                    .concrete_expr_ty(e)
                    .is_some_and(|ty| ty.peel_nullable().1)
                {
                    return Err(CompilerError::new_with_label(
                        self.body.expr_spans[e],
                        "a nullable `ref` to a flat-stored container element can be compared \
                         with null, passed to a `ref` parameter or returned; check it for null \
                         before reading it"
                            .to_string(),
                    ));
                }
                let val = self.lower_flat_ref(e)?;
                self.flat_group_read(val).map(Some)
            }
            Expr::Call { .. } => {
                let Some(key) = self.classify_flat_ref(e)? else {
                    return Ok(None);
                };
                if self
                    .concrete_expr_ty(e)
                    .is_some_and(|ty| ty.peel_nullable().1)
                {
                    return Err(CompilerError::new_with_label(
                        self.body.expr_spans[e],
                        format!(
                            "this call returns a nullable `ref` to a flat-stored `{}`; bind it \
                             with `let` and check it for null before reading it",
                            key.elem.name
                        ),
                    ));
                }
                let val = self.lower_flat_ref(e)?;
                self.flat_group_read(val).map(Some)
            }
            Expr::FieldAccess { base, field } => {
                let mut root = base;
                while let Expr::FieldAccess { base: b, .. } = self.body.expr_arena[root] {
                    root = b;
                }
                if self.flat_ref_root_key(root)?.is_none() {
                    return Ok(None);
                }
                let Some((root, path, nested)) = self.flat_field_path(base, field)? else {
                    return Ok(None);
                };
                let Some(place) = self.flat_root_place(root, path)? else {
                    return Err(self.gap_here("flat `ref` read has no root place"));
                };
                if nested.is_some() {
                    return self.flat_group_read(place).map(Some);
                }
                let leaf = self.flat_leaf_load(place)?;
                self.value_bundle_for_expr(e, ValueRepr::Scalar(leaf))
                    .map(Some)
            }
            _ => Ok(None),
        }
    }

    fn flat_group_read(&mut self, val: FlatRefVal) -> Result<ValueBundle, CompilerError> {
        let shape = self.flat_target_shape(&val.key)?;
        let keys = self.flat_leaf_keys(&val.key, &shape);
        let leaves = keys
            .into_iter()
            .map(|prefix| {
                self.flat_leaf_load(FlatRefVal {
                    key: FlatRefKey { prefix, ..val.key },
                    ..val
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let representation = if val.key.prefix.is_empty() {
            val.key.elem.representation()
        } else {
            self.nominal_value_representation(shape.name)?
        };
        Ok(ValueBundle::new(
            representation,
            Self::rebuild_from_leaves(&shape, &leaves, &mut 0),
        ))
    }
}
