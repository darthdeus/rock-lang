//! A `ref` to a container element that Lua stores flat -- the value columns of
//! a Vec, a Map's or an Arena's data column, or a strided row -- has no element
//! table to alias. It travels as the column vec plus the element's checked
//! slot, and every read and write through it goes to the element's leaves.
//!
//! A callee reached with such an argument is lowered again for that argument
//! storage: a flat variant is the callee's body with each flat `ref` parameter
//! bound to a vec and a slot, hoisted beside the caller like a nested fn. A
//! `ref` return that addresses a flat element returns `vec, slot`. A body that
//! never reaches a flat element emits exactly as before, so the cost is paid
//! only where a flat element is borrowed.
use super::call::DirectCall;
use super::*;
use crate::instance::Instance;
use bone_semantic::hir::InterpPart;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum FlatStorage {
    /// One column per dotted leaf path, `vec[path][slot]`.
    Columns,
    /// One `_data` row of `stride` slots per element.
    Strided,
}

/// What a flat `ref` addresses: the stored element and the dotted path of the
/// nested value field inside it (empty for the whole element). Every key
/// derived from it is static, so a body lowered for one key addresses leaves
/// directly.
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
            Ustr::from(&format!("{}.{field}", self.prefix))
        };
        Self { prefix, ..self }
    }
}

/// Per callee parameter, the flat storage its `ref` argument has.
pub(crate) type FlatSig = Vec<Option<FlatRefKey>>;

#[derive(Clone, Copy)]
pub(crate) struct FlatRefVal {
    pub(crate) vec: Idx<Inst>,
    pub(crate) slot: Idx<Inst>,
    pub(crate) key: FlatRefKey,
}

/// A direct call that passes or returns a flat `ref`.
pub(crate) struct FlatCallPlan {
    inst: Instance,
    sig: FlatSig,
    /// `(argument, parameter index, key)` for each flat argument.
    args: Vec<(Idx<Expr>, usize, FlatRefKey)>,
    ret: Option<FlatRefKey>,
}

/// The flat arguments of the call whose arguments are being lowered. Each
/// `ref` argument it expects lowers to its vec, and its slot is recorded here.
pub(crate) struct FlatArgCapture {
    expected: Vec<(Idx<Expr>, usize, FlatRefKey)>,
    slots: Vec<(usize, Idx<Inst>)>,
}

pub(crate) enum FlatCallOut {
    Flat(FlatRefVal),
    Value(ValueRepr),
}

/// Flat variants and flat-return answers of one emitted root body, shared by
/// every builder that lowers into it (nested fns and variants included).
#[derive(Default)]
pub(crate) struct FlatVariants {
    /// The root instance, set only by a root that hoists the variants into
    /// its `nested_fns`; variants are named after its symbol.
    root: Option<Instance>,
    counter: u32,
    by_key: HashMap<(Instance, FlatSig), Ustr>,
    funcs: Vec<MirFunc>,
    returns: HashMap<(Instance, FlatSig), Option<FlatRefKey>>,
    /// Return scans in progress, innermost last.
    scanning: Vec<ReturnScan>,
}

/// One in-progress return scan. A call back into it (recursion, direct or
/// mutual) reads `assumed`; the scan repeats until its answer equals what
/// it assumed.
struct ReturnScan {
    key: (Instance, FlatSig),
    /// `None` until a pass has an answer: a return that reads an unknown
    /// answer decides nothing, rather than counting as boxed.
    assumed: Option<Option<FlatRefKey>>,
    /// The outermost in-progress scan this pass read an answer from.
    depends_on: Option<usize>,
    /// Reads of an unknown answer during this pass.
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

/// Every returned expression of a body and every `let` in it, excluding the
/// bodies of lambdas and nested fns, which return for themselves.
fn scan_returns(
    body: &Body,
    e: Idx<Expr>,
    returns: &mut Vec<Idx<Expr>>,
    lets: &mut Vec<(Idx<BindingInfo>, Idx<Expr>)>,
) {
    let go = |x: Idx<Expr>, returns: &mut Vec<Idx<Expr>>, lets: &mut Vec<_>| {
        scan_returns(body, x, returns, lets)
    };
    match body.expr_arena[e] {
        Expr::Unit
        | Expr::Null
        | Expr::Hole(_)
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::Bool(_)
        | Expr::String(_)
        | Expr::Var(_)
        | Expr::Global(_)
        | Expr::SiblingRef(_)
        | Expr::Break
        | Expr::Continue
        | Expr::Lambda { .. } => {}
        Expr::Tuple(es) | Expr::Array(es) => {
            for x in es {
                go(x, returns, lets);
            }
        }
        Expr::InterpolatedString(parts) => {
            for p in parts {
                if let InterpPart::Value { expr, .. } = body.interp_part_arena[p] {
                    go(expr, returns, lets);
                }
            }
        }
        Expr::Not(i, _)
        | Expr::BitNot(i)
        | Expr::UnaryMinus(i, _)
        | Expr::Spread(i)
        | Expr::YieldDelegate(i)
        | Expr::As { value: i, .. }
        | Expr::TupleIndex { tuple: i, .. }
        | Expr::FieldAccess { base: i, .. }
        | Expr::SafeNav { base: i, .. }
        | Expr::IncDec { target: i, .. } => go(i, returns, lets),
        Expr::Return(o) => {
            if let Some(i) = o {
                returns.push(i);
                go(i, returns, lets);
            }
        }
        Expr::Yield(o) => {
            if let Some(i) = o {
                go(i, returns, lets);
            }
        }
        Expr::Index { base: a, index: b }
        | Expr::Range { low: a, high: b }
        | Expr::BinOp { lhs: a, rhs: b, .. }
        | Expr::Assign {
            target: a,
            value: b,
        }
        | Expr::CompoundAssign {
            target: a,
            value: b,
            ..
        } => {
            go(a, returns, lets);
            go(b, returns, lets);
        }
        Expr::If {
            cond: a,
            then_branch: b,
            else_branch,
        }
        | Expr::IfLet {
            value: a,
            then_branch: b,
            else_branch,
            ..
        } => {
            go(a, returns, lets);
            go(b, returns, lets);
            if let Some(x) = else_branch {
                go(x, returns, lets);
            }
        }
        Expr::Block { statements, tail } => {
            for s in statements {
                scan_stmt_returns(body, s, returns, lets);
            }
            if let Some(t) = tail {
                go(t, returns, lets);
            }
        }
        Expr::Match { scrutinee, arms } => {
            go(scrutinee, returns, lets);
            for a in arms {
                let arm = body.match_arm_arena[a];
                if let Some(g) = arm.guard {
                    go(g, returns, lets);
                }
                go(arm.body, returns, lets);
            }
        }
        Expr::Call { callee, args, .. } => {
            if let Callee::Value(v) = callee {
                go(v, returns, lets);
            }
            for x in args {
                go(x, returns, lets);
            }
        }
        Expr::SafeNavCall { receiver, args, .. } => {
            go(receiver, returns, lets);
            for x in args {
                go(x, returns, lets);
            }
        }
        Expr::SafeNavLet {
            receiver, body: b, ..
        } => {
            go(receiver, returns, lets);
            go(b, returns, lets);
        }
        Expr::StructLiteral { fields, bases, .. } => {
            for b in bases {
                go(b, returns, lets);
            }
            for fld in fields {
                go(body.struct_lit_field_arena[fld].value, returns, lets);
            }
        }
        Expr::EnumVariant { args, .. } => match args {
            VariantArgs::Unit => {}
            VariantArgs::Tuple(es) => {
                for x in es {
                    go(x, returns, lets);
                }
            }
            VariantArgs::Record(fs) => {
                for fld in fs {
                    go(body.struct_lit_field_arena[fld].value, returns, lets);
                }
            }
        },
    }
}

fn scan_stmt_returns(
    body: &Body,
    s: Idx<Statement>,
    returns: &mut Vec<Idx<Expr>>,
    lets: &mut Vec<(Idx<BindingInfo>, Idx<Expr>)>,
) {
    match body.statement_arena[s].clone() {
        Statement::Let { name, value } => {
            lets.push((name, value));
            scan_returns(body, value, returns, lets);
        }
        Statement::LetTuple { value, .. }
        | Statement::LetNewtype { value, .. }
        | Statement::Expr(value)
        | Statement::Defer(value)
        | Statement::Static { value, .. } => scan_returns(body, value, returns, lets),
        Statement::Return(Some(value)) => {
            returns.push(value);
            scan_returns(body, value, returns, lets);
        }
        Statement::For { iter, body: b, .. } => {
            scan_returns(body, iter, returns, lets);
            scan_returns(body, b, returns, lets);
        }
        Statement::While { cond, body: b } => {
            scan_returns(body, cond, returns, lets);
            scan_returns(body, b, returns, lets);
        }
        Statement::WhileLet { value, body: b, .. } => {
            scan_returns(body, value, returns, lets);
            scan_returns(body, b, returns, lets);
        }
        Statement::Fn { .. }
        | Statement::Return(None)
        | Statement::Break
        | Statement::Continue
        | Statement::Comment(_) => {}
    }
}

impl<'a> Builder<'a> {
    /// The flat storage of `container`'s elements, in the order
    /// [`Self::flat_field_path`] resolves it.
    fn flat_container_elem(
        &self,
        container: Idx<Expr>,
    ) -> Result<Option<(ColumnElem, FlatStorage)>, CompilerError> {
        if let Some(elem) = self.vec_column_elem_of(container)? {
            return Ok(Some((elem, FlatStorage::Columns)));
        }
        if let Some(elem) = self.vec_strided_elem_of(container)? {
            return Ok(Some((elem, FlatStorage::Strided)));
        }
        Ok(self
            .map_column_value_of(container)?
            .map(|elem| (elem, FlatStorage::Columns)))
    }

    /// The value shape a flat `ref` with `key` addresses.
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

    /// Whether `ty` is `ref V` or `(ref V)?` for a value struct `V`, the only
    /// `ref` that can address a flat element.
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

    /// `Some(key)` iff `e` denotes a flat-stored element or a nested value
    /// field of one: an index into a flat container, a field of such a place,
    /// a flat `ref` binding, or a call whose `ref` result is flat.
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
                    prefix: Ustr::from(""),
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

    /// The flat arguments and flat result of direct call `e`, or `None` when
    /// it has neither. Cheap for an ordinary call: only `ref` arguments are
    /// classified, and only a value-`ref` result consults the callee's body.
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
            .is_some_and(|ty| self.is_value_ref_type(&ty));
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
            if self.body.spread_args.contains(&a) {
                continue;
            }
            let pidx = match self.body.arg_names.get(&a) {
                Some(nm) => match decl.param_decls.iter().position(|p| p.name == *nm) {
                    Some(i) => i,
                    None => continue,
                },
                None => {
                    next_pos += 1;
                    next_pos - 1
                }
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

    /// Whether argument `a` of call `e` can carry a `ref`: one bone records
    /// for a `ref` parameter, or a `ref`-typed binding or call handed to a
    /// generic parameter (`unwrap(c)` with `T = ref V`), which bone does not
    /// record because `T` is still open when it checks the call.
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
                .is_some_and(|ty| self.is_value_ref_type(&ty)),
            _ => false,
        })
    }

    /// Whether `a` is a flat argument of the call whose arguments are being
    /// lowered, which lowers through [`Self::lower_ref_borrow`] like a `ref`
    /// argument bone records.
    pub(crate) fn is_flat_ref_call_arg(&self, a: Idx<Expr>) -> bool {
        self.flat_arg_captures
            .last()
            .is_some_and(|capture| capture.expected.iter().any(|(arg, _, _)| *arg == a))
    }

    /// Whether `inst`, entered with flat arguments `sig`, returns a flat `ref`,
    /// and with which key. Every returned place must agree: one `ref` result
    /// cannot be `vec, slot` on one path and a table on another.
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
        let returns_value_ref = plan.signature.result.is_some_and(|result| {
            self.is_value_ref_type(
                &self.concrete_mono_view(result, ConcreteMonoConsumer::ReturnLowering),
            )
        });
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
        let mut scan = Builder::generated(
            self.ws,
            self.scx,
            self.call_scx,
            plan,
            self.concrete_plans,
            body,
            inf,
            MirFunc::new(decl.name),
        );
        scan.generic_bounds = &decl.generic_bounds;
        scan.site_owner = Some(inst.clone());
        scan.flat_variants = self.flat_variants.clone();
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
            scan_returns(body, root, &mut returns, &mut lets);
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
            // Read an answer of an enclosing scan, which rescans this one
            // once its own answer changes.
            Some(d) if d < depth => {
                let reader = reg.scanning.last_mut().expect("an enclosing scan");
                reader.depends_on = Some(reader.depends_on.map_or(d, |r| r.min(d)));
                if answer.is_none() {
                    reader.unknown_reads += 1;
                }
            }
            // Settled. A body whose every return recurses never returns and
            // keeps the ordinary boxed result.
            _ => {
                reg.returns.insert(key, answer.flatten());
            }
        }
        Ok(answer.flatten())
    }

    /// The agreed result of `returns` for the scan at `depth`, repeated with
    /// each new answer assumed for the scan's own recursive calls until the
    /// answer stops changing. `None` while every return reads an unknown
    /// answer.
    fn settle_flat_returns(
        &self,
        decl: &Function,
        returns: &[Idx<Expr>],
        depth: usize,
    ) -> Result<Option<Option<FlatRefKey>>, CompilerError> {
        // Unknown, then the first answer, then its confirmation.
        const PASSES: usize = 3;
        for _ in 0..PASSES {
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
        Err(CompilerError::new_with_label(
            decl.name_span,
            format!(
                "`{}` returns a `ref` whose storage does not settle across its recursive calls; \
                 the Lua backend passes a flat-stored element `ref` as the container and a slot, \
                 so every return must address the same storage",
                decl.name
            ),
        ))
    }

    fn agree_flat_returns(
        &self,
        decl: &Function,
        returns: &[Idx<Expr>],
        depth: usize,
    ) -> Result<Option<Option<FlatRefKey>>, CompilerError> {
        let unknown_reads = || self.flat_variants.borrow().scanning[depth].unknown_reads;
        let mut agreed: Option<Option<FlatRefKey>> = None;
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

    /// The flat `ref` result of the body being lowered, when its declared
    /// result is a value `ref` and every return addresses a flat element.
    pub(crate) fn own_flat_return(
        &self,
        subject: &Instance,
    ) -> Result<Option<FlatRefKey>, CompilerError> {
        let decl = &self.ws.module(subject.func.module).functions[subject.func.func];
        let sig: FlatSig = vec![None; decl.param_decls.len()];
        self.flat_return_scan(subject, self.concrete_plan, &sig)
    }

    /// Reject a call path that cannot take the `vec, slot` result of an
    /// instance whose `ref` result is flat (a safe-nav call, a function value).
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
                 call can receive on the Lua backend",
                decl.name
            ),
        ))
    }

    /// Lower `e`, which [`Self::classify_flat_ref`] classified flat, to its
    /// vec and slot.
    pub(crate) fn lower_flat_ref(&mut self, e: Idx<Expr>) -> Result<FlatRefVal, CompilerError> {
        let key = self
            .classify_flat_ref(e)?
            .ok_or_else(|| self.gap_here("lowering a flat `ref` of a non-flat place"))?;
        match self.body.expr_arena[e] {
            Expr::Index { .. } => {
                let place = match key.storage {
                    FlatStorage::Columns => self.column_index_place(e)?,
                    FlatStorage::Strided => self.strided_index_place(e)?,
                };
                let (vec, slot, _) =
                    place.ok_or_else(|| self.gap_here("flat element index has no column place"))?;
                Ok(FlatRefVal { vec, slot, key })
            }
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
        let any_named = arg_idxs
            .iter()
            .any(|a| self.body.arg_names.contains_key(a) || self.body.spread_args.contains(a));
        let dc = self.plan_direct_call(e, func)?;
        let plan = self
            .flat_call_plan(e)?
            .ok_or_else(|| self.gap_here("flat `ref` call has no flat plan"))?;
        self.lower_flat_call(e, func, &arg_idxs, any_named, &dc, &plan, None)
    }

    /// Lower a direct call with a flat plan. Flat arguments go first as
    /// slots, in parameter order, and each one's own argument position holds
    /// its vec; a flat result arrives as `vec, slot`.
    pub(crate) fn lower_flat_call(
        &mut self,
        e: Idx<Expr>,
        func: FuncRef,
        arg_idxs: &[Idx<Expr>],
        any_named: bool,
        dc: &DirectCall,
        plan: &FlatCallPlan,
        ret_tuple: Option<TupleRetShape>,
    ) -> Result<FlatCallOut, CompilerError> {
        if plan.inst != dc.callee_inst {
            return Err(self.gap_here("flat `ref` call disagrees with its concrete callee"));
        }
        self.flat_arg_captures.push(FlatArgCapture {
            expected: plan.args.clone(),
            slots: Vec::new(),
        });
        let args = self.direct_call_args(e, func, arg_idxs, any_named, dc);
        let capture = self.flat_arg_captures.pop().expect("pushed above");
        let args = args?;
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
            let call = self.emit_cur(Inst::CallMulti {
                dests: dests.clone(),
                func,
                args,
            });
            self.record_call(call, func, dc)?;
            let vec = self.emit_cur(Inst::Load(dests[0]));
            let slot = self.emit_cur(Inst::Load(dests[1]));
            return Ok(FlatCallOut::Flat(FlatRefVal { vec, slot, key }));
        }
        let hoisted = self.flat_variant(plan, dc)?;
        if !self.f.hoisted_fn_globals.contains(&hoisted) {
            self.f.hoisted_fn_globals.push(hoisted);
        }
        // Only ever called directly, so the call names the global rather than
        // a reload code slot; the variant never escapes as a value.
        let callee = self.emit_cur(Inst::Global(hoisted));
        let mut argv: Vec<Idx<Inst>> = slots.into_iter().map(|(_, slot)| slot).collect();
        argv.extend(args);
        if let Some(key) = plan.ret {
            let dests = vec![self.f.new_cell(None), self.f.new_cell(None)];
            self.emit_cur(Inst::CallValueMulti {
                dests: dests.clone(),
                callee,
                args: argv,
            });
            let vec = self.emit_cur(Inst::Load(dests[0]));
            let slot = self.emit_cur(Inst::Load(dests[1]));
            return Ok(FlatCallOut::Flat(FlatRefVal { vec, slot, key }));
        }
        let ret_shape = if dc.plan.uses_scalar_abi() {
            dc.plan.ret_shape()
        } else {
            None
        };
        if let Some(shape) = ret_shape {
            let dests: Vec<Idx<Cell>> = (0..shape.leaf_count())
                .map(|_| self.f.new_cell(None))
                .collect();
            self.emit_cur(Inst::CallValueMulti {
                dests: dests.clone(),
                callee,
                args: argv,
            });
            let leaves: Vec<Idx<Inst>> = dests
                .iter()
                .map(|c| self.emit_cur(Inst::Load(*c)))
                .collect();
            return Ok(FlatCallOut::Value(
                self.rebuild_from_leaves(&shape, &leaves, &mut 0),
            ));
        }
        if let Some(shape) = &ret_tuple {
            let dests: Vec<Idx<Cell>> = self
                .tuple_opaque_slot_types(e, shape)
                .into_iter()
                .map(|ty| self.f.new_cell_typed(None, ty))
                .collect();
            self.emit_cur(Inst::CallValueMulti {
                dests: dests.clone(),
                callee,
                args: argv,
            });
            let loads: Vec<Idx<Inst>> = dests
                .iter()
                .map(|c| self.emit_cur(Inst::Load(*c)))
                .collect();
            return Ok(FlatCallOut::Value(self.tuple_result(shape, &loads)));
        }
        Ok(FlatCallOut::Value(ValueRepr::Scalar(
            self.emit_cur(Inst::CallValue { callee, args: argv }),
        )))
    }

    /// The hoisted flat variant of `plan`'s callee for its flat signature,
    /// lowered on first use and shared by every call in this root.
    fn flat_variant(
        &mut self,
        plan: &FlatCallPlan,
        dc: &DirectCall,
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
            let name = Ustr::from(&format!(
                "__flatref_{}_{n}",
                root.symbol(self.ws, self.scx.interner())
            ));
            // Generated callable machinery of the root, named by its symbol.
            let identity = crate::code_identity::CodeIdentity::Generated {
                enclosing: crate::code_identity::CodeOwner::Instance(root),
                role: name,
            };
            reg.by_key.insert(key, name);
            (name, identity)
        };
        let mf = self.lower_flat_variant(hoisted, &identity, plan, dc)?;
        self.flat_variants.borrow_mut().funcs.push(mf);
        Ok(hoisted)
    }

    fn lower_flat_variant(
        &self,
        hoisted: Ustr,
        identity: &crate::code_identity::CodeIdentity,
        plan: &FlatCallPlan,
        dc: &DirectCall,
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
            return unsupported("it is a coroutine");
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
        mf.rep_arena = Some(self.scx.rep_arena());
        mf.flat_variant_source = Some(inst.func);
        let flat_abi = plan
            .sig
            .iter()
            .map(|key| match key {
                None => "boxed".to_string(),
                Some(key) => format!(
                    "elem={};nullable={};nobox={};storage={:?};prefix={};{}",
                    key.elem.name,
                    key.elem.nullable,
                    key.elem.is_nobox(),
                    key.storage,
                    key.prefix,
                    abi::rep_logical_fingerprint(&self.scx.rep_arena(), key.elem.representation()),
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
        let mut child = Builder::generated(
            self.ws,
            self.scx,
            self.call_scx,
            concrete_plan,
            self.concrete_plans,
            body,
            inf,
            mf,
        );
        child.generic_bounds = &decl.generic_bounds;
        child.defer_mode = stmt::DeferMode::for_body(false, self.call_scx.target);
        child.param_plan = Some(dc.plan.clone());
        child.ret_rep = dc.plan.representation().result();
        child.fn_name_span = Some(decl.name_span);
        child.site_owner = Some(inst.clone());
        child.static_owner = static_owner_tag(self.ws, self.scx, inf, inst);
        child.flat_variants = self.flat_variants.clone();
        child.flat_ret = plan.ret;
        let params: Vec<Idx<BindingInfo>> = body.param_bindings.into_iter().collect();
        let mut slot_cells = Vec::new();
        for (i, &pb) in params.iter().enumerate() {
            if plan.sig.get(i).copied().flatten().is_some() {
                let name = body.binding_arena[pb].name;
                let cell = child.new_binding_cell(
                    pb,
                    Ustr::from(&format!("{name}__slot")),
                    None,
                    Ustr::from("slot"),
                );
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
                        Ustr::from(&format!("{name}__vec")),
                        None,
                        Ustr::from("vec"),
                    );
                    child.f.params.push(vec);
                    let slot = slot_cells.next().expect("one slot cell per flat parameter");
                    child.flat_ref_binds.insert(raw(pb), key);
                    child.flat_ref_cells.insert(raw(pb), (vec, slot));
                }
                None => {
                    let slot = child.param_slot(i)?;
                    child.bind_param_slot(pb, name, &slot)?;
                }
            }
        }
        child.fill_param_defaults()?;
        child.copy_ref_borrowed_params(&params)?;
        child.lower_root()?;
        // Code belongs to the generated variant; persistent state belongs to
        // the concrete callable whose body it implements.
        for cell in &mut child.f.statics {
            cell.owner = Some(crate::code_identity::CodeIdentity::Named(inst.clone()));
        }
        let stats = child.lower_attribution.into_inner();
        let child_f = finish_lower_attribution(child.f, stats);
        self.lower_attribution
            .borrow_mut()
            .add_assign(&child_f.lower_attribution);
        Ok(child_f)
    }

    /// A flat `ref` argument of the call whose arguments are being lowered:
    /// its vec, with the slot recorded for the call. `None` for any other
    /// borrow.
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

    /// `let x: ref T = <flat place>` (or `(ref T)?`, from a borrow call): bind
    /// the vec and slot. `false` when the binding is not a flat `ref`.
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
            Ustr::from(&format!("{bname}__vec")),
            None,
            Ustr::from("vec"),
        );
        let slot = self.new_binding_cell(
            name,
            Ustr::from(&format!("{bname}__slot")),
            None,
            Ustr::from("slot"),
        );
        self.emit_cur(Inst::Store {
            cell: vec,
            value: val.vec,
        });
        self.emit_cur(Inst::Store {
            cell: slot,
            value: val.slot,
        });
        self.flat_ref_binds.insert(raw(name), key);
        self.flat_ref_cells.insert(raw(name), (vec, slot));
        Ok(true)
    }

    /// The `return` wire of a body whose result is a flat `ref`.
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

    /// The leaf keys of the value a flat `ref` addresses, in leaf order.
    fn flat_leaf_keys(&self, key: &FlatRefKey, shape: &ValueShape) -> Vec<Ustr> {
        leaf_names(shape, key.prefix.as_str(), ".")
            .iter()
            .map(|k| Ustr::from(k))
            .collect()
    }

    pub(crate) fn flat_leaf_load(
        &mut self,
        vec: Idx<Inst>,
        slot: Idx<Inst>,
        elem: ColumnElem,
        storage: FlatStorage,
        path: Ustr,
    ) -> Result<Idx<Inst>, CompilerError> {
        match storage {
            FlatStorage::Columns => {
                let col = self.emit_cur(Inst::Field {
                    base: vec,
                    field: path,
                });
                Ok(self.emit_cur(Inst::Index {
                    base: col,
                    index: slot,
                    kind: ContainerKind::VecColumn,
                }))
            }
            FlatStorage::Strided => {
                let (at, data) = self.flat_strided_slot(vec, slot, elem, path)?;
                let stored = self.emit_cur(Inst::Index {
                    base: data,
                    index: at,
                    kind: ContainerKind::VecStrideSlot,
                });
                Ok(if self.strided_stores_raw(elem) {
                    stored
                } else {
                    self.emit_cur(Inst::VecStrideFalseToNil(stored))
                })
            }
        }
    }

    pub(crate) fn flat_leaf_store(
        &mut self,
        vec: Idx<Inst>,
        slot: Idx<Inst>,
        elem: ColumnElem,
        storage: FlatStorage,
        path: Ustr,
        value: Idx<Inst>,
    ) -> Result<(), CompilerError> {
        match storage {
            FlatStorage::Columns => {
                let col = self.emit_cur(Inst::Field {
                    base: vec,
                    field: path,
                });
                self.emit_cur(Inst::StoreIndex {
                    base: col,
                    index: slot,
                    value,
                    kind: ContainerKind::VecColumn,
                });
            }
            FlatStorage::Strided => {
                let (at, data) = self.flat_strided_slot(vec, slot, elem, path)?;
                let value = if self.strided_stores_raw(elem) {
                    value
                } else {
                    self.emit_cur(Inst::VecStrideNilToFalse(value))
                };
                self.emit_cur(Inst::StoreIndex {
                    base: data,
                    index: at,
                    value,
                    kind: ContainerKind::VecStrideSlot,
                });
            }
        }
        Ok(())
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
        let data = self.emit_cur(Inst::Field {
            base: vec,
            field: Ustr::from("_data"),
        });
        let at = self.strided_slot_index(slot, shape.leaf_count(), offset);
        Ok((at, data))
    }

    /// `(vec, slot, elem, storage)` of the root of a flat field path: an
    /// index into a flat container, or a flat `ref` binding or call.
    pub(crate) fn flat_root_place(
        &mut self,
        root: Idx<Expr>,
    ) -> Result<Option<(Idx<Inst>, Idx<Inst>, ColumnElem, FlatStorage)>, CompilerError> {
        if matches!(self.body.expr_arena[root], Expr::Index { .. }) {
            if let Some((vec, slot, elem)) = self.column_index_place(root)? {
                return Ok(Some((vec, slot, elem, FlatStorage::Columns)));
            }
            if let Some((vec, slot, elem)) = self.strided_index_place(root)? {
                return Ok(Some((vec, slot, elem, FlatStorage::Strided)));
            }
            return Ok(None);
        }
        if self.classify_flat_ref(root)?.is_none() {
            return Ok(None);
        }
        let val = self.lower_flat_ref(root)?;
        Ok(Some((val.vec, val.slot, val.key.elem, val.key.storage)))
    }

    /// The root of a flat field path that is not an index: a flat `ref`
    /// binding or call, with its key.
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

    /// A read through a flat `ref` binding or call (`c`, `c.inner`, `c.w`,
    /// `a.borrow(e).unwrap().w`): the addressed leaves, never a table. An
    /// index-rooted read keeps its own path.
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
                let Some((vec, slot, elem, storage)) = self.flat_root_place(root)? else {
                    return Err(self.gap_here("flat `ref` read has no root place"));
                };
                match nested {
                    None => {
                        let leaf = self.flat_leaf_load(vec, slot, elem, storage, path)?;
                        self.value_bundle_for_expr(e, ValueRepr::Scalar(leaf))
                            .map(Some)
                    }
                    Some(_) => {
                        let key = FlatRefKey {
                            elem,
                            storage,
                            prefix: path,
                        };
                        self.flat_group_read(FlatRefVal { vec, slot, key })
                            .map(Some)
                    }
                }
            }
            _ => Ok(None),
        }
    }

    fn flat_group_read(&mut self, val: FlatRefVal) -> Result<ValueBundle, CompilerError> {
        let shape = self.flat_target_shape(&val.key)?;
        let keys = self.flat_leaf_keys(&val.key, &shape);
        let leaves = keys
            .into_iter()
            .map(|path| self.flat_leaf_load(val.vec, val.slot, val.key.elem, val.key.storage, path))
            .collect::<Result<Vec<_>, _>>()?;
        let representation = if val.key.prefix.is_empty() {
            val.key.elem.representation()
        } else {
            self.nominal_value_representation(shape.name)?
        };
        Ok(ValueBundle::new(
            representation,
            self.rebuild_from_leaves(&shape, &leaves, &mut 0),
        ))
    }

    /// The single leaf `<base>.<field>` names under a flat root, for a
    /// read-modify-write (`c.v += 1`, `xs[i].v++`).
    pub(crate) fn flat_leaf_rmw_place(
        &mut self,
        base: Idx<Expr>,
        field: Ustr,
    ) -> Result<Option<(Idx<Inst>, Idx<Inst>, ColumnElem, FlatStorage, Ustr)>, CompilerError> {
        let Some((root, path, None)) = self.flat_field_path(base, field)? else {
            return Ok(None);
        };
        Ok(self
            .flat_root_place(root)?
            .map(|(vec, slot, elem, storage)| (vec, slot, elem, storage, path)))
    }
}
