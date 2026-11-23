use super::*;
use bone_core::hir_walk::HirChild;

pub(super) fn call_uses_parameter_slots(body: &Body, args: &[Idx<Expr>]) -> bool {
    args.iter()
        .any(|arg| body.arg_names.contains_key(arg) || body.spread_args.contains(arg))
}

pub(super) fn call_argument_parameter(
    body: &Body,
    arg: Idx<Expr>,
    next_positional: &mut usize,
    find_named: impl FnOnce(Ustr) -> Option<usize>,
) -> Option<usize> {
    if body.spread_args.contains(&arg) {
        return None;
    }
    match body.arg_names.get(&arg) {
        Some(&name) => find_named(name),
        None => {
            let index = *next_positional;
            *next_positional += 1;
            Some(index)
        }
    }
}

#[derive(Clone, Copy)]
enum CallArgMode<'s> {
    Boxed(
        &'s crate::instance::Instance,
        &'s crate::concrete::ConcreteSignature,
    ),
    Scalar {
        shapes: &'s [&'s RepLayout],
        types: &'s [crate::monoty::MonoTyId],
    },
}

enum CallArgValue {
    Boxed(Idx<Inst>),
    Scalar(Vec<Idx<Inst>>),
}

impl CallArgValue {
    fn receiver_mut(&mut self) -> Option<&mut Idx<Inst>> {
        match self {
            Self::Boxed(value) => Some(value),
            Self::Scalar(values) => match values.as_mut_slice() {
                [value] => Some(value),
                _ => None,
            },
        }
    }

    fn append_to(self, out: &mut Vec<Idx<Inst>>) {
        match self {
            Self::Boxed(value) => out.push(value),
            Self::Scalar(values) => out.extend(values),
        }
    }
}

fn preflight_scalar_type(ws: &Workspace, inf: &BodyInference, e: Idx<Expr>) -> bool {
    inf.expr_types.get(e).is_some_and(|ty| {
        matches!(
            ws.render(Some(inf), *ty).as_str(),
            "int" | "uint" | "float" | "f32" | "bool"
        )
    })
}

fn preflight_numeric_type(ws: &Workspace, inf: &BodyInference, e: Idx<Expr>) -> bool {
    inf.expr_types.get(e).is_some_and(|ty| {
        matches!(
            ws.render(Some(inf), *ty).as_str(),
            "int" | "uint" | "float" | "f32"
        )
    })
}

fn preflight_safe_node(
    ws: &Workspace,
    body: &Body,
    inf: &BodyInference,
    node: HirChild,
    visiting: &mut HashSet<FuncRef>,
    helper_body: bool,
) -> bool {
    if let HirChild::Expr(e) = node
        && (inf.implicit_iter_args.contains_key(&e) || inf.ref_call_args.contains_key(&e))
    {
        return false;
    }
    let safe = match node {
        HirChild::Expr(e) => match body.expr_arena[e] {
            Expr::Unit | Expr::Null | Expr::Int(_) | Expr::Float(_) | Expr::Bool(_) => true,
            Expr::Var(b) => {
                (matches!(
                    body.binding_arena[b].kind,
                    bone_core::hir::BindingKind::Param
                ) || !helper_body
                    && matches!(body.binding_arena[b].kind, bone_core::hir::BindingKind::Let))
                    && preflight_scalar_type(ws, inf, e)
            }
            Expr::Not(..) => {
                preflight_scalar_type(ws, inf, e)
                    && inf.resolved_overload_calls.get(&e).is_none_or(|(mid, _)| {
                        bone_core::modules::is_stdlib_module_file(ws.module(*mid).file.as_str())
                    })
            }
            Expr::UnaryMinus(..) => {
                preflight_numeric_type(ws, inf, e)
                    && inf.resolved_overload_calls.get(&e).is_none_or(|(mid, _)| {
                        bone_core::modules::is_stdlib_module_file(ws.module(*mid).file.as_str())
                    })
            }
            Expr::As { value, target, .. } => {
                preflight_numeric_type(ws, inf, value)
                    && matches!(ws.render(Some(inf), target).as_str(), "float" | "f32")
            }
            Expr::BinOp { op, lhs, rhs } => {
                matches!(op, BinOp::And | BinOp::Or)
                    || (matches!(
                        op,
                        BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::Leq | BinOp::Gt | BinOp::Geq
                    ) && preflight_scalar_type(ws, inf, lhs)
                        && preflight_scalar_type(ws, inf, rhs)
                        && inf.resolved_overload_calls.get(&e).is_none_or(|(mid, _)| {
                            bone_core::modules::is_stdlib_module_file(ws.module(*mid).file.as_str())
                        }))
            }
            Expr::If { .. } | Expr::Block { .. } => true,
            Expr::Return(_) => helper_body,
            Expr::StructLiteral {
                name,
                bases,
                fields,
            } => {
                bases.is_empty()
                    && ws.nominals.struct_id(name).is_some_and(|id| {
                        let definition = &ws.module(id.module).structs[id.def];
                        definition.is_value
                            && definition.is_nobox
                            && fields.len() == definition.fields.len()
                    })
            }
            Expr::Call {
                callee: Callee::Unresolved { name, .. },
                args,
                ..
            } => {
                let arg_list: Vec<_> = args.into_iter().collect();
                let builtin_operator =
                    inf.resolved_overload_calls.get(&e).is_none_or(|(mid, _)| {
                        bone_core::modules::is_stdlib_module_file(ws.module(*mid).file.as_str())
                    });
                if builtin_operator && matches!(name.as_str(), "__op_add" | "__op_sub" | "__op_mul")
                {
                    arg_list.len() == 2
                        && arg_list.iter().all(|&a| preflight_numeric_type(ws, inf, a))
                } else if builtin_operator
                    && matches!(name.as_str(), "__op_div" | "__op_mod" | "__op_idiv")
                {
                    arg_list.len() == 2
                        && arg_list.iter().all(|&a| preflight_numeric_type(ws, inf, a))
                        && match body.expr_arena[arg_list[1]] {
                            Expr::Int(n) => n != 0,
                            Expr::Float(n) => n.0 != 0.0,
                            _ => false,
                        }
                } else {
                    let Some((mid, fid)) = resolve_call_fn(ws, inf, e, name) else {
                        return false;
                    };
                    let f = &ws.module(mid).functions[fid];
                    if f.name.as_str() == "float"
                        && f.inline_body
                            .is_some_and(|(_, source)| source.trim() == "return value")
                        && bone_core::modules::is_stdlib_module_file(ws.module(mid).file.as_str())
                    {
                        arg_list.len() == 1 && preflight_numeric_type(ws, inf, arg_list[0])
                    } else if f.is_extern
                        || f.inline_body.is_some()
                        || f.is_coroutine
                        || !f.generics.is_empty()
                        || arg_list.len() != f.param_decls.len()
                    {
                        false
                    } else {
                        let target = FuncRef {
                            module: mid,
                            func: fid,
                        };
                        if !visiting.insert(target) {
                            return false;
                        }
                        let safe_body = ws
                            .module(mid)
                            .bodies
                            .get(&bone_core::hir::BodyOwner::Function(fid))
                            .zip(ws.inference(mid).and_then(|i| {
                                i.per_fn.get(&bone_core::hir::BodyOwner::Function(fid))
                            }))
                            .and_then(|(body, inf)| {
                                body.root.map(|root| {
                                    preflight_safe_node(
                                        ws,
                                        body,
                                        inf,
                                        HirChild::Expr(root),
                                        visiting,
                                        true,
                                    )
                                })
                            })
                            .unwrap_or(false);
                        visiting.remove(&target);
                        safe_body
                    }
                }
            }
            _ => false,
        },
        HirChild::Statement(s) => matches!(
            body.statement_arena[s],
            Statement::Return(_) | Statement::Expr(_)
        ),
        HirChild::Pattern(_) => false,
    };
    safe && body
        .try_for_each_child(node, |child| {
            preflight_safe_node(ws, body, inf, child, visiting, helper_body)
                .then_some(())
                .ok_or(())
        })
        .is_ok()
}

pub(crate) struct PreflightPlan {
    helper: FuncRef,
    tail: FuncRef,
    reads: Vec<Idx<Expr>>,
}

fn preflight_collect_reads(
    body: &Body,
    node: HirChild,
    seen: &mut HashSet<u32>,
    out: &mut Vec<Idx<Expr>>,
) {
    if let HirChild::Expr(e) = node
        && let Expr::Var(binding) = body.expr_arena[e]
        && seen.insert(raw(binding))
    {
        out.push(e);
    }
    body.for_each_child(node, |child| {
        preflight_collect_reads(body, child, seen, out)
    });
}

pub(crate) struct DirectCall<'a> {
    tree: Option<&'a bone_core::prelude::BoundTree>,
    pub(super) callee: &'a crate::concrete::ConcreteCallee,
    pub(crate) plan: BackendFnAbiPlanHandle,
    callee_plan: Option<&'a ConcreteBodyPlan>,
}

impl<'a> Builder<'a> {
    fn preflight_plan(
        &self,
        e: Idx<Expr>,
        func: FuncRef,
        arg_idxs: &[Idx<Expr>],
        dc: &DirectCall<'a>,
    ) -> Result<Option<PreflightPlan>, CompilerError> {
        if self.scx.target() != bone_core::hir::FnBackend::Lua {
            return Ok(None);
        }
        let callee = &self.ws.module(func.module).functions[func.func];
        let Some(annotation) = callee
            .annotations
            .iter()
            .find(|a| a.is_builtin("preflight"))
        else {
            return Ok(None);
        };
        let Some(receiver_param) = callee.param_decls.first() else {
            return Err(self.gap_here("@preflight requires a receiver parameter"));
        };
        let bone_core::hir::AnnoValue::String(helper_name) = annotation.args[0].value else {
            return Err(self.gap_here("malformed @preflight helper name"));
        };
        let bone_core::hir::AnnoValue::String(tail_name) = annotation.args[1].value else {
            return Err(self.gap_here("malformed @preflight tail name"));
        };
        let candidates: Vec<_> = self
            .ws
            .module(func.module)
            .functions
            .iter()
            .filter(|(_, function)| {
                function.name.as_str().rsplit("::").next() == Some(helper_name.as_str())
            })
            .map(|(id, _)| (func.module, id))
            .collect();
        let [(module, helper_id)] = candidates.as_slice() else {
            return Err(self.gap_here(format!(
                "@preflight helper `{helper_name}` must name one function in the callee module"
            )));
        };
        let helper = &self.ws.module(*module).functions[*helper_id];
        let tails: Vec<_> = self
            .ws
            .module(func.module)
            .functions
            .iter()
            .filter(|(_, function)| {
                function.name.as_str().rsplit("::").next() == Some(tail_name.as_str())
            })
            .map(|(id, _)| id)
            .collect();
        let [tail_id] = tails.as_slice() else {
            return Err(self.gap_here(format!(
                "@preflight tail `{tail_name}` must name one function in the callee module"
            )));
        };
        let tail = &self.ws.module(func.module).functions[*tail_id];
        let helper_receiver_is_ref = match helper.param_decls.first().map(|p| p.ty) {
            Some(TyId::Static(mid, idx)) => matches!(
                self.ws.module(mid).ty_arena[idx],
                TypeExpr::App {
                    ctor: TypeCtor::Ref,
                    ..
                }
            ),
            _ => false,
        };
        if helper.is_extern
            || helper.inline_body.is_some()
            || helper.is_coroutine
            || !helper.generics.is_empty()
            || helper.param_decls.len() != 1
            || helper.num_required != 1
            || helper_receiver_is_ref
            || self.ws.render(None, helper.param_decls[0].ty)
                != self.ws.render(None, receiver_param.ty)
            || helper.return_ty.is_none()
        {
            return Err(self.gap_here(format!("@preflight helper `{helper_name}` must be Rock-bodied, unary, non-generic, take the same receiver type, and return a context")));
        }
        let context_ty = helper.return_ty.expect("checked above");
        let context_is_boxed_struct = match context_ty {
            TyId::Static(mid, idx) => match self.ws.module(mid).ty_arena[idx] {
                TypeExpr::App {
                    ctor: TypeCtor::Nominal(name),
                    params,
                } if params.is_empty() => self
                    .ws
                    .nominals
                    .struct_id(name)
                    .is_some_and(|id| !self.ws.module(id.module).structs[id.def].is_value),
                _ => false,
            },
            TyId::Local(_) => false,
        };
        if !context_is_boxed_struct
            || tail.is_extern
            || tail.inline_body.is_some()
            || tail.is_coroutine
            || !tail.generics.is_empty()
            || tail.param_decls.len() != callee.param_decls.len()
            || self.ws.render(None, tail.param_decls[0].ty) != self.ws.render(None, context_ty)
            || tail.param_decls[1..]
                .iter()
                .zip(&callee.param_decls[1..])
                .any(|(a, b)| self.ws.render(None, a.ty) != self.ws.render(None, b.ty))
            || tail.return_ty.map(|ty| self.ws.render(None, ty))
                != callee.return_ty.map(|ty| self.ws.render(None, ty))
        {
            return Err(self.gap_here(format!("@preflight tail `{tail_name}` must be Rock-bodied, non-generic, take the context followed by the original arguments, and return the original type")));
        }
        // The receiver must be the first source argument: moving a later named
        // receiver would change argument evaluation order.
        if arg_idxs.len() != callee.param_decls.len()
            || arg_idxs.len() < 2
            || arg_idxs
                .iter()
                .any(|arg| self.body.spread_args.contains(arg) || self.is_ref_call_arg(e, *arg))
        {
            return Ok(None);
        }
        let mut next_pos = 0;
        let mut filled = HashSet::default();
        for (source_index, &arg) in arg_idxs.iter().enumerate() {
            let Some(slot) = call_argument_parameter(self.body, arg, &mut next_pos, |name| {
                callee.param_decls.iter().position(|p| p.name == name)
            }) else {
                return Ok(None);
            };
            if source_index == 0 && slot != 0 || !filled.insert(slot) {
                return Ok(None);
            }
        }
        if filled.len() != callee.param_decls.len() {
            return Ok(None);
        }
        let boxed_receiver = match self.concrete_expr_ty(arg_idxs[0]) {
            Some(MonoTy::App {
                ctor: MonoCtor::Nominal(name),
                params,
            }) if params.is_empty() => self
                .ws
                .nominals
                .struct_id(*name)
                .is_some_and(|id| !self.ws.module(id.module).structs[id.def].is_value),
            _ => false,
        };
        if !boxed_receiver {
            return Err(self
                .gap_here("@preflight requires a non-value struct receiver with boxed call ABI"));
        }
        if callee.return_ty.is_some_and(|ty| {
            !matches!(
                self.ws.render(None, ty).as_str(),
                "()" | "int" | "uint" | "float" | "f32" | "bool" | "string"
            )
        }) {
            return Ok(None);
        }
        let tail_ref = FuncRef {
            module: func.module,
            func: *tail_id,
        };
        let tail_instance = self
            .concrete_callee_for_site_op(e, crate::site::SiteOp::Synthesized("preflight_tail"))?;
        if tail_instance.func != tail_ref {
            return Err(self.gap_here("@preflight concrete tail differs from annotation"));
        }
        let tail_abi = self.callee_abi_plan_for_instance(&tail_instance)?;
        let shapes = dc.plan.argument_shapes();
        let tail_shapes = tail_abi.argument_shapes();
        if tail_abi.uses_scalar_abi() != dc.plan.uses_scalar_abi()
            || shapes != tail_shapes
            || tail_abi.ret_shape() != dc.plan.ret_shape()
            || tail_abi.ret_tuple() != dc.plan.ret_tuple()
        {
            return Ok(None);
        }
        let Some(inf) = self.inf else {
            return Ok(None);
        };
        let mut visiting = HashSet::default();
        if !arg_idxs[1..].iter().all(|&arg| {
            preflight_safe_node(
                self.ws,
                self.body,
                inf,
                HirChild::Expr(arg),
                &mut visiting,
                false,
            )
        }) {
            return Ok(None);
        }
        let mut seen = HashSet::default();
        let mut reads = Vec::new();
        for &arg in &arg_idxs[1..] {
            preflight_collect_reads(self.body, HirChild::Expr(arg), &mut seen, &mut reads);
        }
        Ok(Some(PreflightPlan {
            helper: FuncRef {
                module: *module,
                func: *helper_id,
            },
            tail: tail_ref,
            reads,
        }))
    }

    pub(super) fn preflight_value(&self, binding: Idx<BindingInfo>) -> Option<Idx<Inst>> {
        self.preflight_reads
            .iter()
            .rev()
            .find_map(|reads| reads.get(&raw(binding)).copied())
    }

    fn emit_preflight(
        &mut self,
        e: Idx<Expr>,
        plan: &PreflightPlan,
        receiver: Idx<Inst>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let mut reads = HashMap::default();
        for &e in &plan.reads {
            let Expr::Var(binding) = self.body.expr_arena[e] else {
                unreachable!()
            };
            reads.insert(raw(binding), self.lower_expr(e)?);
        }
        self.preflight_reads
            .last_mut()
            .expect("direct-call scope")
            .extend(reads);
        let op = crate::site::SiteOp::Synthesized("preflight");
        let instance = self.concrete_callee_for_site_op(e, op.clone())?;
        if instance.func != plan.helper {
            return Err(self.gap_here("@preflight concrete helper differs from annotation"));
        }
        let old_op = std::mem::replace(&mut self.cur_op, op);
        let call = self.emit_cur(Inst::Call {
            func: plan.helper,
            args: vec![receiver],
        });
        self.cur_op = old_op;
        let abi = self.callee_abi_plan_for_instance(&instance)?;
        if abi.representation().return_lanes() != 1 || abi.ret_shape().is_some() {
            return Err(self.gap_here("@preflight prepare must return one boxed context"));
        }
        self.f.call_abi_plans.insert(call, abi);
        self.f.callee_instances.insert(call, instance);
        Ok(call)
    }

    fn emit_preflight_tail(
        &mut self,
        e: Idx<Expr>,
        tail: FuncRef,
        args: Vec<Idx<Inst>>,
    ) -> Result<ValueRepr, CompilerError> {
        let op = crate::site::SiteOp::Synthesized("preflight_tail");
        let instance = self.concrete_callee_for_site_op(e, op.clone())?;
        if instance.func != tail {
            return Err(self.gap_here("@preflight concrete tail differs from annotation"));
        }
        let old_op = std::mem::replace(&mut self.cur_op, op);
        let call = self.emit_cur(Inst::Call { func: tail, args });
        self.cur_op = old_op;
        let abi = self.callee_abi_plan_for_instance(&instance)?;
        self.f.call_abi_plans.insert(call, abi);
        self.f.callee_instances.insert(call, instance);
        Ok(ValueRepr::Scalar(call))
    }

    /// A user function may share the stdlib's name.
    fn calls_stdlib_vec_fn(&self, e: Idx<Expr>, name: Ustr, recv: Idx<Expr>) -> bool {
        if self.body.arg_names.contains_key(&recv) || self.body.spread_args.contains(&recv) {
            return false;
        }
        let is_vec = matches!(
            self.concrete_expr_ty(recv),
            Some(MonoTy::App {
                ctor: MonoCtor::Array,
                ..
            })
        );
        if !is_vec {
            return false;
        }
        let Ok(func) = self.resolve_funcref(e, name) else {
            return false;
        };
        let module = self.ws.module(func.module);
        bone_core::modules::is_stdlib_module_file(module.file.as_str())
            && module.functions[func.func].inline_body.is_none()
    }

    fn lower_nullable_field_get(
        &mut self,
        e: Idx<Expr>,
        name: Ustr,
        args: &[Idx<Expr>],
    ) -> Result<Option<ValueRepr>, CompilerError> {
        let func = self.resolve_funcref(e, name)?;
        let module = self.ws.module(func.module);
        if !bone_core::modules::is_stdlib_module_file(module.file.as_str())
            || module.functions[func.func].inline_body.is_none()
        {
            return Ok(None);
        }
        let inst = self.concrete_callee_for_site(e)?;
        let Some(ret) = self.concrete_callee_return_ty(e, &inst)? else {
            return Ok(None);
        };
        let (payload, nullable) = ret.peel_nullable();
        let Some(child) = payload.nominal() else {
            return Ok(None);
        };
        if !nullable || !self.is_nobox_struct(child) {
            return Ok(None);
        }
        let owner = self
            .concrete_callee_param_ty(e, &inst, 0)?
            .ok_or_else(|| self.gap_here("field_get has no concrete owner type"))?;
        let Some(parent) = owner.nominal() else {
            return Ok(None);
        };
        let fields = self.struct_field_names_required(parent)?;
        let mut flattened = false;
        for field in fields {
            if self.struct_field_plan(parent, field, false)?.embed() == Some((child, true)) {
                flattened = true;
                break;
            }
        }
        if !flattened {
            return Ok(None);
        }
        let id = self.concrete_rep_id(&ret, RepUse::Return, false, false)?;
        let shape = self
            .scx
            .rep_arena()
            .value_shape(id)
            .ok_or_else(|| self.gap_here("flattened field_get result has no leaf shape"))?;
        let signature = self.concrete_callee_signature(e, &inst)?;
        let args = self.declared_slotted_call_args(
            e,
            func,
            args,
            CallArgMode::Boxed(&inst, signature),
            None,
        )?;
        let [owner, selector] = args.as_slice() else {
            return Err(self.gap_here("field_get requires an owner and selector"));
        };
        // The field's own key is presence; its dotted keys hold the payload.
        let kind = if self.scx.target() == bone_core::hir::FnBackend::Lua {
            ContainerKind::Record(Some(parent))
        } else {
            ContainerKind::Map
        };
        let mut leaves = vec![self.read_index(*owner, *selector, kind)];
        for path in value_leaf_paths(shape) {
            let suffix = self.emit_cur(Inst::Const(Lit::Str(crate::names::ustr(&format!(
                ".{path}"
            )))));
            let index = self.emit_cur(Inst::StringConcat(*selector, suffix));
            leaves.push(self.read_index(*owner, index, kind));
        }
        Ok(Some(Self::rebuild_from_leaves(shape, &leaves)))
    }

    pub(crate) fn concrete_callee_for_site(
        &self,
        e: Idx<Expr>,
    ) -> Result<crate::instance::Instance, CompilerError> {
        self.concrete_callee_for_site_op(e, self.cur_op.clone())
    }

    pub(crate) fn concrete_callee_for_site_op(
        &self,
        e: Idx<Expr>,
        op: crate::site::SiteOp,
    ) -> Result<crate::instance::Instance, CompilerError> {
        self.concrete_callee_entry(e, op)
            .map(|callee| callee.instance.clone())
    }

    pub(crate) fn concrete_callee_entry(
        &self,
        e: Idx<Expr>,
        op: crate::site::SiteOp,
    ) -> Result<&'a crate::concrete::ConcreteCallee, CompilerError> {
        let plan: &'a ConcreteBodyPlan = self.concrete_plan;
        let site = crate::site::Site {
            domain: self.cur_domain.clone(),
            expr: e,
            op,
        };
        if let Some(callee) = plan.callees.get(&site) {
            return Ok(callee);
        }
        if let Some(reason) = plan.failed_sites.get(&site) {
            return Err(self.gap_here(format!(
                "concrete plan records an unresolved callee site: {reason}"
            )));
        }
        Err(gap(format!(
            "concrete plan is missing callee identity for site {:?}",
            site.op
        )))
    }

    pub(crate) fn concrete_delivery_instance(
        &self,
        inst: &crate::instance::Instance,
    ) -> Option<crate::instance::Instance> {
        if inst.args.is_empty() {
            return None;
        }
        self.concrete_plans.contains_key(inst).then(|| inst.clone())
    }

    /// Checked against the identity the caller resolved for the same site.
    pub(crate) fn concrete_callee_signature(
        &self,
        e: Idx<Expr>,
        inst: &crate::instance::Instance,
    ) -> Result<&'a crate::concrete::ConcreteSignature, CompilerError> {
        let callee = self.concrete_callee_entry(e, self.cur_op.clone())?;
        if crate::verify::checks_enabled() && &callee.instance != inst {
            return Err(self.gap_here("concrete callee identity disagrees with its site plan"));
        }
        self.callee_signature(inst, callee, || {
            self.concrete_plans.get(inst).map(AsRef::as_ref)
        })
    }

    /// A callee with its own body plan answers for its signature; any other keeps
    /// the one its site recorded.
    fn callee_signature(
        &self,
        inst: &crate::instance::Instance,
        site: &'a crate::concrete::ConcreteCallee,
        body_plan: impl FnOnce() -> Option<&'a ConcreteBodyPlan>,
    ) -> Result<&'a crate::concrete::ConcreteSignature, CompilerError> {
        if !callee_has_body_plan(self.ws, self.scx, inst) {
            return Ok(&site.signature);
        }
        body_plan()
            .map(|plan| plan.signature.as_ref())
            .ok_or_else(|| self.gap_here("concrete callee body plan is missing for its site"))
    }

    pub(crate) fn direct_call_signature(
        &self,
        dc: &DirectCall<'a>,
    ) -> Result<&'a crate::concrete::ConcreteSignature, CompilerError> {
        self.callee_signature(&dc.callee.instance, dc.callee, || dc.callee_plan)
    }

    pub(crate) fn concrete_callee_param_ty(
        &self,
        e: Idx<Expr>,
        inst: &crate::instance::Instance,
        pidx: usize,
    ) -> Result<Option<std::borrow::Cow<'a, crate::monoty::MonoTy>>, CompilerError> {
        let signature = self.concrete_callee_signature(e, inst)?;
        self.concrete_param_ty_of(signature, inst, pidx)
    }

    /// A variadic callee's trailing parameter covers every extra argument.
    pub(crate) fn concrete_param_ty_of(
        &self,
        signature: &crate::concrete::ConcreteSignature,
        inst: &crate::instance::Instance,
        pidx: usize,
    ) -> Result<Option<std::borrow::Cow<'a, crate::monoty::MonoTy>>, CompilerError> {
        let callee = &self.ws.module(inst.func.module).functions[inst.func.func];
        let slot = signature.params.get(pidx).or_else(|| {
            callee
                .is_variadic
                .then(|| signature.params.last())
                .flatten()
        });
        let id = slot.ok_or_else(|| {
            self.gap_here("concrete callee signature has no parameter for this argument")
        })?;
        Ok(Some(std::borrow::Cow::Borrowed(self.concrete_mono(*id))))
    }

    pub(crate) fn concrete_callee_return_ty(
        &self,
        e: Idx<Expr>,
        inst: &crate::instance::Instance,
    ) -> Result<Option<crate::monoty::MonoTy>, CompilerError> {
        let signature = self.concrete_callee_signature(e, inst)?;
        Ok(signature.result.map(|id| self.concrete_mono(id).clone()))
    }

    pub(crate) fn concrete_coroutine_channel(
        &self,
        receiver: Idx<Expr>,
    ) -> Result<Option<EnumFlatShape>, CompilerError> {
        let body_plan = self.concrete_plan;
        let site = crate::site::Site {
            domain: self.cur_domain.clone(),
            expr: receiver,
            op: crate::site::SiteOp::DirectCall,
        };
        let result = if let Some(callee) = body_plan.callees.get(&site) {
            let signature = self.callee_signature(&callee.instance, callee, || {
                self.concrete_plans.get(&callee.instance).map(AsRef::as_ref)
            })?;
            signature.result.map(|id| self.concrete_mono(id))
        } else {
            Some(
                self.concrete_expr_ty(receiver)
                    .ok_or_else(|| self.gap_here("concrete coroutine receiver has no type"))?,
            )
        };
        Ok(result
            .as_ref()
            .and_then(|ty| CoroutinePlan::channel_for_concrete_type(self.ws, ty)))
    }

    pub(crate) fn callee_abi_plan_for_instance(
        &self,
        inst: &crate::instance::Instance,
    ) -> Result<BackendFnAbiPlanHandle, CompilerError> {
        crate::lower::abi::callee_abi_plan_for_instance(
            self.ws,
            self.scx,
            self.concrete_plans,
            inst,
        )
    }

    /// A scalar-ABI `Group` result is packed to one box; ABI-boundary callers use
    /// [`Self::lower_call_repr`].
    pub(crate) fn lower_call(
        &mut self,
        e: Idx<Expr>,
        callee: Callee,
        args: IdxRange<Expr>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let r = self.lower_call_repr(e, callee, args)?;
        self.pack_stored(r)
    }

    /// A splatting tuple becomes the flat wire every stored-box reader rebuilds
    /// from, so its `nobox` parts never box.
    pub(crate) fn pack_stored(&mut self, bundle: ValueBundle) -> Result<Idx<Inst>, CompilerError> {
        let arena = self.scx.rep_arena();
        let splats = matches!(arena.layout(bundle.representation()), RepLayout::Tuple(shape) if shape.splats(arena));
        if splats && matches!(bundle.value(), ValueRepr::Tuple { .. }) {
            return self.pack_bundle(&bundle);
        }
        self.pack_maybe_absent(bundle.into_value())
    }

    /// A scalar-ABI direct call yields an unboxed `Group`, so a consuming boundary
    /// splats it without a round-trip through a box.
    pub(crate) fn lower_call_repr(
        &mut self,
        e: Idx<Expr>,
        callee: Callee,
        args: IdxRange<Expr>,
    ) -> Result<ValueBundle, CompilerError> {
        let trusted = match callee {
            Callee::Unresolved { name, .. } => self
                .resolve_funcref(e, name)
                .ok()
                .and_then(|f| self.select_backend_variant(e, name, f).ok())
                .is_some_and(|f| {
                    self.ws.module(f.module).functions[f.func]
                        .inline_body
                        .is_none()
                }),
            _ => false,
        };
        let r = self.with_value_lending(false, |s| s.lower_call_repr_inner(e, callee, args))?;
        self.finish_call_repr(e, r, trusted)
    }

    fn call_result_bundle(
        &self,
        plan: &FunctionRepPlan,
        value: ValueRepr,
    ) -> Result<ValueBundle, CompilerError> {
        let representation = match plan.result() {
            Some(result) => result,
            None => self.concrete_rep_id(&MonoTy::Unit, RepUse::Value, false, false)?,
        };
        Ok(ValueBundle::new(representation, value))
    }

    fn finish_call_repr(
        &mut self,
        e: Idx<Expr>,
        bundle: ValueBundle,
        trusted: bool,
    ) -> Result<ValueBundle, CompilerError> {
        let r = bundle.value();
        // Iterator protocols use phantom()'s nil even at a non-null value type.
        if matches!(r, ValueRepr::Scalar(v) if matches!(self.f.insts[*v], Inst::Const(Lit::Nil))) {
            return Ok(bundle);
        }
        if !self.scx.values_first_class {
            let ty = self.call_result_owner_ty(e, r)?;
            if let Some(ty) = ty {
                // An opaque return can forward storage borrowed from a foreign body; snapshot
                // it at the consuming boundary.
                let share = trusted && !matches!(r, ValueRepr::Scalar(_));
                self.mark_boundary_value_repr(r, &ty, share)?;
            }
        }
        Ok(bundle)
    }

    /// Storage reads carry the receiver's element type even though the inline
    /// primitive is declared `any`.
    fn call_result_owner_ty(
        &self,
        e: Idx<Expr>,
        value: &ValueRepr,
    ) -> Result<Option<std::borrow::Cow<'a, MonoTy>>, CompilerError> {
        if matches!(value, ValueRepr::Group { .. }) {
            if let Expr::Call {
                callee: Callee::Unresolved { name, .. },
                args,
                ..
            } = &self.body.expr_arena[e]
                && name.as_str() == "__vec_data_get"
                && let Some(recv) = args.into_iter().next()
                && let Some(plan) = self.container_plan_for(recv, ContainerFamily::Vec)?
                && matches!(
                    plan.storage(),
                    ContainerStorage::Columns { .. } | ContainerStorage::Strided { .. }
                )
            {
                let owner = self.concrete_ty(recv)?.ok_or_else(|| {
                    self.gap_here("intercepted Vec read has no concrete receiver type")
                })?;
                let MonoTy::App {
                    ctor: MonoCtor::Array,
                    params,
                } = owner.peel_nullable().0
                else {
                    return Err(self.gap_here("intercepted Vec read has a non-Vec receiver type"));
                };
                return params
                    .first()
                    .cloned()
                    .map(std::borrow::Cow::Owned)
                    .map(Some)
                    .ok_or_else(|| {
                        self.gap_here("intercepted Vec read has no concrete element type")
                    });
            }
        }
        Ok(self.concrete_expr_ty(e).map(std::borrow::Cow::Borrowed))
    }

    fn lower_call_repr_inner(
        &mut self,
        e: Idx<Expr>,
        callee: Callee,
        args: IdxRange<Expr>,
    ) -> Result<ValueBundle, CompilerError> {
        let arg_idxs: Vec<Idx<Expr>> = args.into_iter().collect();
        if matches!(
            self.scx.target(),
            bone_core::hir::FnBackend::Lua | bone_core::hir::FnBackend::Js
        ) {
            if let Callee::Unresolved { name, .. } = callee {
                if name.as_str() == "field_get" {
                    if let Some(repr) = self.lower_nullable_field_get(e, name, &arg_idxs)? {
                        let value = repr;
                        return self.value_bundle_for_expr(e, value);
                    }
                }
            }
        }
        if let Callee::Unresolved { name, .. } = callee {
            if matches!(name.as_str(), "inspect" | "inspect_formatted") {
                if let Some(&arg) = arg_idxs.first() {
                    if let Some(name) = self
                        .concrete_expr_ty(arg)
                        .and_then(|ty| ty.peel_nullable().0.nominal())
                        .filter(|name| self.ws.nominals.struct_id(*name).is_some())
                    {
                        self.f.struct_registry_roots.insert(name);
                    }
                    if let Some((elem, _)) = self.vec_flat_elem_of(arg)? {
                        if elem.is_nobox() {
                            return Err(CompilerError::new_with_label(
                                self.body.expr_spans[e],
                                format!(
                                    "cannot inspect a flat-stored nobox value `{}`: \
                                     inspection requires implicit element materialization",
                                    elem.name
                                ),
                            ));
                        }
                    }
                    // Inspect expects one slot per logical tuple element.
                    if self.is_explicit_materialize_call(arg) {
                        self.boundary_materializes.insert(arg);
                    } else if let Some(shape) = self.tuple_box_shape(arg)? {
                        return Err(CompilerError::new_with_label(
                            self.body.expr_spans[e],
                            format!(
                                "cannot inspect a tuple carrying a nobox value: its {} \
                                 leaves ride the box, so it would be read as a wider \
                                 tuple (project the slots, or `materialize` the \
                                 tuple deliberately)",
                                shape.slot_count(self.scx.rep_arena())
                            ),
                        ));
                    }
                }
            }
        }
        // `tostring` of an enum, value record or handle is the structural rendering,
        // never the backend's rendering of its representation.
        if let Some((arg, _)) = self.dispatch().tostring_route(e) {
            return self
                .with_site_expr(arg, |s| s.stringify_structural(arg))
                .map(ValueRepr::Scalar)
                .and_then(|value| self.value_bundle_for_expr(e, value));
        }
        if let Callee::Unresolved { name, .. } = callee {
            // `cast<T>(v)` is identity, so a `nobox` result keeps the argument's
            // representation instead of boxing and rereading its leaves.
            if name.as_str() == "cast" && self.nobox_value_name(e).is_some() {
                if let [inner] = arg_idxs[..] {
                    if self.resolves_to_inline_builtin(e, name) {
                        return self.lower_expr_repr(inner);
                    }
                }
            }
        }
        // On Lua a value with no mutable heap storage is already the owner's copy.
        // C keeps the call: its strings are bytes the destination owner must own.
        // A column or strided Vec has no `_data` for the opaque `clone_into` body
        // to walk, so it is copied per column.
        let gc_target = matches!(
            self.scx.target(),
            bone_core::hir::FnBackend::Lua | bone_core::hir::FnBackend::Js
        );
        if let Callee::Unresolved { name, .. } = callee {
            if name.as_str() == "clone_into" && gc_target {
                if let [value, storage] = arg_idxs[..] {
                    let positional = arg_idxs.iter().all(|a| {
                        !self.body.arg_names.contains_key(a) && !self.body.spread_args.contains(a)
                    });
                    if positional && self.resolves_to_inline_builtin(e, name) {
                        if let Some((elem, flat)) = self.vec_flat_elem_of(value)? {
                            let ty = MonoTy::App {
                                ctor: MonoCtor::Nominal(elem.name),
                                params: Vec::new(),
                            };
                            if super::shape::lua_value_is_immutable(self.ws, &ty) {
                                let vec = self.lower_expr(value)?;
                                self.lower_expr(storage)?;
                                let value = self.clone_flat_data(vec, elem, flat)?;
                                return self.value_bundle_for_expr(e, value);
                            }
                        }
                    }
                    if positional
                        && self.scx.target() == bone_core::hir::FnBackend::Lua
                        && self.resolves_to_inline_builtin(e, name)
                        && self
                            .concrete_expr_ty(value)
                            .is_some_and(|ty| super::shape::lua_value_is_immutable(self.ws, ty))
                    {
                        let copy = self.lower_expr_repr(value)?;
                        self.lower_expr(storage)?;
                        return Ok(copy);
                    }
                }
            }
        }
        if let Callee::Unresolved { name, .. } = callee
            && name.as_str() == "__json_record_type"
            && self.resolves_to_inline_builtin(e, name)
        {
            let [arg] = arg_idxs[..] else {
                return Err(self.gap_here("json record type requires one value"));
            };
            let ty = self
                .concrete_expr_ty(arg)
                .and_then(|ty| ty.nominal())
                .ok_or_else(|| self.gap_here("json record requires a concrete nominal type"))?;
            self.lower_expr_repr(arg)?;
            let value = ValueRepr::Scalar(self.emit_cur(Inst::Const(Lit::Str(
                crate::mangle::type_symbol(self.ws, ty),
            ))));
            return self.value_bundle_for_expr(e, value);
        }
        if let Callee::Unresolved { name, .. } = callee {
            let n = name.as_str();
            if matches!(
                n,
                "__op_add" | "__op_sub" | "__op_mul" | "__op_div" | "__op_idiv" | "__op_mod"
            ) {
                if let [a, b] = arg_idxs[..] {
                    if let (Some(lt), Some(rt)) = (self.arith_ty(a), self.arith_ty(b)) {
                        return self.lower_native_arith(n, lt, rt, a, b);
                    }
                }
            }
        }
        if let Callee::Unresolved { name, .. } = callee {
            if name.as_str() == "native_buffer_alloc" {
                let [n_arg] = arg_idxs[..] else {
                    return Err(gap(
                        "`native_buffer_alloc` takes exactly one argument (count)",
                    ));
                };
                let span = self.body.expr_spans[e];
                let (ctype, representation) = self.native_buffer_alloc_ctype(e, span)?;
                let count = self.lower_expr(n_arg)?;
                let ptr = self.emit_cur(Inst::FfiNewArray {
                    ctype,
                    count,
                    representation,
                });
                let mut fields = vec![
                    (crate::names::ustr("ptr"), ptr),
                    (crate::names::ustr("len"), count),
                ];
                if representation.is_some() && self.is_leaf_addressed(ctype)? {
                    fields.extend(self.leaf_view_fields(ctype, ptr));
                }
                let nb = self.emit_cur(Inst::MakeStruct {
                    ty: Some(crate::names::ustr("NativeBuffer")),
                    fields,
                });
                let value = ValueRepr::Scalar(nb);
                return self.value_bundle_for_expr(e, value);
            }
        }
        if let Callee::Unresolved { name, .. } = callee
            && matches!(name.as_str(), "len" | "vec_len")
            && let [recv] = arg_idxs[..]
            && self.scx.target() == bone_core::hir::FnBackend::Lua
            && !self.scx.values_first_class
            && self.calls_stdlib_vec_fn(e, name, recv)
        {
            let base = self.lower_expr(recv)?;
            let key = self.vec_length_field(recv)?;
            let value = ValueRepr::Scalar(self.read_field(base, key));
            return self.value_bundle_for_expr(e, value);
        }
        if let Callee::Unresolved { name, .. } = callee {
            // The ordinary push stores one boxed element. Flat storage expands it into leaves.
            if matches!(name.as_str(), "push" | "vec_push") {
                if let [recv, item] = arg_idxs[..] {
                    if let Some((elem, storage)) = self.vec_flat_elem_of(recv)? {
                        let value = self.flat_push(recv, item, elem, storage)?;
                        return self.value_bundle_for_expr(e, value);
                    }
                }
            }
            if let Some(r) = self.flat_vec_data_primitive(e, name.as_str(), &arg_idxs)? {
                return Ok(r);
            }
            // The element type comes from the call's own type. The concrete plan must
            // select the storage before applying the ordinary allocator.
            if name.as_str() == "__vec_data_alloc" && arg_idxs.len() == 1 {
                if let Some((elem, storage)) = self.vec_flat_elem_of(e)? {
                    let capacity = self.lower_expr(arg_idxs[0])?;
                    let value = ValueRepr::Scalar(self.empty_flat_vec(elem, storage, capacity)?);
                    return self.value_bundle_for_expr(e, value);
                }
            }
            if name.as_str() == "vec_new" && arg_idxs.is_empty() {
                if let Some((elem, storage)) = self.vec_flat_elem_of(e)? {
                    let capacity = self.emit_cur(Inst::Const(Lit::Int(0)));
                    let value = ValueRepr::Scalar(self.empty_flat_vec(elem, storage, capacity)?);
                    return self.value_bundle_for_expr(e, value);
                }
            }
            // `len`/`clear` are intercepted too: the generic `clear` unsets while
            // iterating the table it walks, and `len` can read the spine in O(1).
            if let Some(r) = self.map_flat_primitive(e, name.as_str(), &arg_idxs)? {
                return Ok(r);
            }
            // The value type comes from the call's own type, not an argument.
            if name.as_str() == "map_new" && arg_idxs.is_empty() {
                if let Some((elem, storage)) = self.map_flat_elem_of(e)? {
                    let value = ValueRepr::Scalar(self.empty_flat_map(elem, storage)?);
                    return self.value_bundle_for_expr(e, value);
                }
            }
            if name.as_str() == "__rock_vec_from_lua_array" {
                if let ([raw, len], Some((elem, storage))) =
                    (&arg_idxs[..], self.vec_flat_elem_of(e)?)
                {
                    let raw = self.lower_expr(*raw)?;
                    let len = self.lower_expr(*len)?;
                    let value =
                        ValueRepr::Scalar(self.flat_vec_from_raw_array(raw, len, elem, storage)?);
                    return self.value_bundle_for_expr(e, value);
                }
            }
        }
        // `on_killed`/`on_resume` hooks become `Inst::CoroHook`, relocated to the
        // constructor: a kill before the first resume must still fire.
        if self.resume_arg.is_some() {
            if let Callee::Unresolved { name, .. } = callee {
                let kind = match name.as_str() {
                    "on_killed" => Some(CoroHookKind::OnKilled),
                    "on_resume" => Some(CoroHookKind::OnResume),
                    "on_hot_reload" => Some(CoroHookKind::OnHotReload),
                    _ => None,
                };
                if let Some(kind) = kind {
                    if let [arg] = arg_idxs[..] {
                        // The constructor can only re-render a lambda literal; any other fn value
                        // would silently install no hook.
                        if !matches!(self.body.expr_arena[arg], Expr::Lambda { .. }) {
                            return Err(gap(format!(
                                "`{name}` argument must be a lambda literal (`() => ...`) -- the hook is relocated to construction time"
                            )));
                        }
                        let lambda = self.lower_expr(arg)?;
                        self.emit_cur(Inst::CoroHook { kind, lambda });
                        let value = ValueRepr::Scalar(self.emit_cur(Inst::Const(Lit::Nil)));
                        return self.value_bundle_for_expr(e, value);
                    }
                }
            }
        }
        if let Callee::Unresolved { name, .. } = callee {
            // A flat-enum yield threads through reused `__y*` frame slots instead of
            // the generic boxing `resume` return: no per-yield allocation.
            if name.as_str() == "resume" && !arg_idxs.is_empty() {
                if let Some(channel) = self.concrete_coroutine_channel(arg_idxs[0])? {
                    let recv = self.lower_expr(arg_idxs[0])?;
                    let advance = self.read_field(recv, crate::names::ustr("__advance"));
                    let mut cargs: Vec<Idx<Inst>> = vec![recv];
                    if let Some(&a) = arg_idxs.get(1) {
                        cargs.push(self.lower_expr(a)?);
                    }
                    self.emit_cur(Inst::CallValue {
                        callee: advance,
                        args: cargs,
                    });
                    let value = self.resume_frame_read_channel(recv, &channel);
                    return self.value_bundle_for_expr(e, value);
                }
            }
        }
        if let Some(inf) = self.inf {
            // Bound `iter` is identity; bound `next` is a placeholder whose concrete
            // callee is substituted per splice from the site's `BoundTree`.
            if inf.bound_identity_calls.contains(&e) {
                let [arg] = arg_idxs[..] else {
                    return Err(gap("bound `iter` call with non-unary args"));
                };
                let value = ValueRepr::Scalar(self.lower_expr(arg)?);
                return self.value_bundle_for_expr(e, value);
            }
            if let Some(&gidx) = inf.bound_calls.get(&e) {
                let [arg] = arg_idxs[..] else {
                    return Err(gap("bound `next` call with non-unary args"));
                };
                let arg_v = self.lower_expr(arg)?;
                // A value element arrives as leaves: an opaque slot would force the splice
                // to rebuild an aggregate, which a presence-carrying element cannot be.
                let shape = self.bound_next_tuple_shape(gidx)?;
                let dests = self.fresh_cells(shape.slot_count(self.scx.rep_arena()));
                let call = self.emit_cur(Inst::CallBoundMulti {
                    dests: dests.clone(),
                    generic_idx: gidx as u32,
                    args: vec![arg_v],
                });
                if shape.splats(self.scx.rep_arena()) {
                    self.f.bound_next_split.insert(call);
                }
                let loads = self.load_cells(&dests);
                let value = self.tuple_result(&shape, &loads);
                return self.value_bundle_for_expr(e, value);
            }
        }
        let any_named = call_uses_parameter_slots(self.body, &arg_idxs);
        if let Some(field) = self.fn_field_of_call(e) {
            let Some((&receiver_arg, call_args)) = arg_idxs.split_first() else {
                return Err(self.gap_here("fn-field call has no receiver"));
            };
            let receiver = self.lower_expr(receiver_arg)?;
            let fieldv = self.read_field(receiver, field);
            let field_ty = self
                .inf
                .and_then(|inf| inf.fn_field_call_types.get(&e))
                .copied()
                .ok_or_else(|| {
                    self.gap_here("fn-field call is missing its resolved function type")
                })?;
            let abi = self
                .fn_value_rep_plan(field_ty)?
                .ok_or_else(|| self.gap_here("fn-field call has no concrete function ABI plan"))?;
            return self.indirect_call_by_plan(abi, fieldv, call_args);
        }
        match callee {
            // A transparent newtype constructor is identity at runtime.
            Callee::Unresolved { name, .. } if self.ws.nominals.newtype_id(name).is_some() => {
                match arg_idxs.as_slice() {
                    [inner] => {
                        let value = ValueRepr::Scalar(self.lower_expr(*inner)?);
                        self.value_bundle_for_expr(e, value)
                    }
                    _ => Err(gap(format!("newtype ctor `{name}` with non-unary args"))),
                }
            }
            // Mangling is codegen's job, not baked into the IR.
            Callee::Unresolved { name, .. } => match self.resolve_funcref(e, name) {
                Ok(func) => {
                    let value = match self.fn_expand_kind(func) {
                        Some(ExpandKind::SceneTypeKey) => {
                            self.expand_scene_type_key(e, func, &arg_idxs)
                        }
                        Some(ExpandKind::Materialize) => self.expand_materialize(e, &arg_idxs),
                        Some(ExpandKind::SortKeyWide) => self.expand_sort_key_wide(&arg_idxs),
                        Some(ExpandKind::SortKeyLt) => self.expand_sort_key_lt(e, &arg_idxs),
                        Some(ExpandKind::ShaderPipeline) => {
                            self.expand_shader_pipeline(e, &arg_idxs)
                        }
                        Some(ExpandKind::VertexLayout) => {
                            self.expand_vertex_layout(e, func, &arg_idxs)
                        }
                        Some(ExpandKind::UniformBlock) => {
                            self.expand_uniform_block(e, func, &arg_idxs)
                        }
                        Some(ExpandKind::UniformValues) => {
                            self.expand_uniform_values(e, func, &arg_idxs)
                        }
                        Some(ExpandKind::BuildVar | ExpandKind::EmbedString) => {
                            let kind = self.fn_expand_kind(func).unwrap();
                            self.expand_comptime_input(e, &arg_idxs, kind)
                        }
                        None => return self.lower_direct_call_repr(e, func, &arg_idxs, any_named),
                    }?;
                    self.value_bundle_for_expr(e, value)
                }
                Err(gap_err) => {
                    if !any_named && self.is_fn_global(name) {
                        let ty = self.ws.globals_by_name[&name];
                        let mono = crate::monoty::convert_decl(self.ws, ty, &SpecSubst::default())
                            .map_err(|reason| {
                                self.gap_here(format!(
                                    "fn-typed global `{name}` has no concrete function type ({reason:?})"
                                ))
                            })?;
                        let abi = self.fn_value_rep_plan_for_mono(&mono)?.ok_or_else(|| {
                            self.gap_here(format!("fn-typed global `{name}` has no function ABI"))
                        })?;
                        let g = self.emit_cur(Inst::Global(name));
                        return self.indirect_call_by_plan(abi, g, &arg_idxs);
                    }
                    Err(gap_err)
                }
            },
            Callee::Local(b) => {
                // A nested fn calls the hoisted top-level fn by name, with no closure.
                if let Some(abi) = self.nested_fn_globals.get(&raw(b)).cloned() {
                    self.f.hoisted_fn_globals.push(abi.hoisted);
                    let g = self.emit_cur(Inst::Global(abi.hoisted));
                    if let Some(identity) = self.f.nested_fn_identities.get(&abi.hoisted).cloned() {
                        self.f.code_value_identities.insert(g, identity);
                    }
                    let param_shapes = abi.param_slots(self.scx.rep_arena());
                    let arg_vals = self.lower_call_arguments(
                        e,
                        &arg_idxs,
                        None,
                        Some(&abi.param_names),
                        CallArgMode::Scalar {
                            shapes: &param_shapes,
                            types: &[],
                        },
                        None,
                    )?;
                    let Some(shape) = abi.result_shape(self.scx.rep_arena()) else {
                        let v = self.emit_call_value(e, g, arg_vals)?;
                        self.f.call_rep_plans.insert(v, abi.representation.clone());
                        return self.call_result_bundle(&abi.representation, ValueRepr::Scalar(v));
                    };
                    let dests = self.fresh_cells(shape.leaf_count());
                    let (call, leaves) = self.call_value_multi(dests, g, arg_vals);
                    self.f
                        .call_rep_plans
                        .insert(call, abi.representation.clone());
                    let value = Self::rebuild_from_leaves(shape, &leaves);
                    return self.call_result_bundle(&abi.representation, value);
                }
                let callee_v = match self.value_binds.get(&raw(b)) {
                    Some(&value) => value,
                    None => {
                        let cell = *self
                            .binds
                            .get(&raw(b))
                            .ok_or_else(|| gap("call to an unbound fn-typed local"))?;
                        self.emit_cur(Inst::Load(cell))
                    }
                };
                if let Some(abi) = self
                    .fn_value_rep_plan(self.binding_ty(b).ok_or_else(|| {
                        gap("fn-typed local has no physical function signature")
                    })?)?
                {
                    return self.indirect_call_by_plan(abi, callee_v, &arg_idxs);
                }
                // Nothing `nobox` in the signature, so args are boxed.
                let arg_vals: Vec<Idx<Inst>> = arg_idxs
                    .iter()
                    .map(|&a| self.lower_expr(a))
                    .collect::<Result<_, _>>()?;
                let v = self.emit_call_value(e, callee_v, arg_vals)?;
                {
                    let value = ValueRepr::Scalar(v);
                    self.value_bundle_for_expr(e, value)
                }
            }
            Callee::Value(ve) => {
                let abi = self.fn_value_rep_plan(
                    *self
                        .inf
                        .and_then(|inf| inf.expr_types.get(ve))
                        .ok_or_else(|| {
                            gap("fn-valued expression has no physical function signature")
                        })?,
                )?;
                let callee_v = self.lower_expr(ve)?;
                if let Some(abi) = abi {
                    return self.indirect_call_by_plan(abi, callee_v, &arg_idxs);
                }
                let arg_vals: Vec<Idx<Inst>> = arg_idxs
                    .iter()
                    .map(|&a| self.lower_expr(a))
                    .collect::<Result<_, _>>()?;
                let v = self.emit_call_value(e, callee_v, arg_vals)?;
                {
                    let value = ValueRepr::Scalar(v);
                    self.value_bundle_for_expr(e, value)
                }
            }
        }
    }

    /// The ABI is derived from the same instance that receives the arguments.
    pub(crate) fn plan_direct_call(
        &mut self,
        e: Idx<Expr>,
    ) -> Result<DirectCall<'a>, CompilerError> {
        // A dispatched helper shares the hole's expression, not its user call's bound tree.
        let tree = if matches!(self.cur_op, crate::site::SiteOp::DirectCall) {
            self.inf
                .and_then(|i| i.bound_call_trees.get(&e))
                .filter(|t| !t.is_empty())
        } else {
            None
        };
        // Identity is independent of delivery: a call the policy declines to
        // specialize still names exactly one concrete instance.
        let callee = self.concrete_callee_entry(e, self.cur_op.clone())?;
        Ok(DirectCall {
            tree,
            callee,
            plan: self.callee_abi_plan_for_instance(&callee.instance)?,
            callee_plan: self.concrete_plans.get(&callee.instance).map(AsRef::as_ref),
        })
    }

    pub(crate) fn record_call(
        &mut self,
        call: Idx<Inst>,
        func: FuncRef,
        dc: &DirectCall<'a>,
    ) -> Result<(), CompilerError> {
        if let Some(tree) = dc.tree {
            self.f.bound_trees.insert(call, tree.clone());
        }
        let owner = &dc.callee.instance;
        if dc.callee_plan.is_some() && !owner.args.is_empty() {
            self.f.spec_bindings.insert(call, owner.clone());
        }
        debug_assert_eq!(
            crate::lower::canonical_variant(self.ws, owner.func),
            crate::lower::canonical_variant(self.ws, func)
        );
        self.record_planned_bound_tree(call, dc.callee_plan, dc.tree.is_some())?;
        self.f.callee_instances.insert(call, owner.clone());
        self.f.call_abi_plans.insert(call, dc.plan.clone());
        Ok(())
    }

    pub(crate) fn record_callee_bound_tree(
        &mut self,
        call: Idx<Inst>,
        owner: &crate::instance::Instance,
        raw_tree_present: bool,
    ) -> Result<(), CompilerError> {
        let plan = self.concrete_plans.get(owner).map(|plan| &**plan);
        self.record_planned_bound_tree(call, plan, raw_tree_present)
    }

    fn record_planned_bound_tree(
        &mut self,
        call: Idx<Inst>,
        plan: Option<&ConcreteBodyPlan>,
        raw_tree_present: bool,
    ) -> Result<(), CompilerError> {
        let Some(plan) = plan else {
            if raw_tree_present {
                return Err(self.gap_here("concrete callee bound plan is missing for its site"));
            }
            return Ok(());
        };
        if raw_tree_present && plan.bound_tree.is_empty() {
            return Err(self.gap_here("concrete callee bound plan is empty for its site"));
        }
        if !plan.bound_tree.is_empty() {
            self.f
                .resolved_bound_trees
                .insert(call, plan.bound_tree.clone());
        }
        Ok(())
    }

    /// Uses the callee's instantiation so generic tuple slots and presence agree
    /// with the emitted call.
    pub(crate) fn call_ret_tuple_shape(
        &self,
        e: Idx<Expr>,
    ) -> Result<Option<TupleLayout>, CompilerError> {
        let owner = self.concrete_callee_for_site(e)?;
        let abi = self.callee_abi_plan_for_instance(&owner)?;
        Ok(abi.ret_tuple().cloned())
    }

    pub(crate) fn lower_direct_call_repr(
        &mut self,
        e: Idx<Expr>,
        func: FuncRef,
        arg_idxs: &[Idx<Expr>],
        any_named: bool,
    ) -> Result<ValueBundle, CompilerError> {
        // A specialized body and its call use the same ABI.
        let dc = self.plan_direct_call(e)?;
        let value = (|| -> Result<ValueRepr, CompilerError> {
            let callee = &self.ws.module(func.module).functions[func.func];
            // Resolve phantom before tuple flattening can project from its nil placeholder.
            if arg_idxs.is_empty()
                && callee
                    .inline_body
                    .is_some_and(|(_, body)| body.trim() == "return nil")
            {
                return Ok(ValueRepr::Scalar(self.emit_cur(Inst::Const(Lit::Nil))));
            }
            let declared_ret_tuple = dc.plan.ret_tuple();
            if let Some(shape) = &declared_ret_tuple {
                self.reject_nobox_opaque_tuple_return(e, func, shape)?;
            } else {
                self.reject_nobox_opaque_value_return(e, func, &dc.callee.instance)?;
            }
            let lua_opaque = self.scx.target() == bone_core::hir::FnBackend::Lua
                && callee.inline_body.is_some()
                && !callee.generics.is_empty()
                && !callee
                    .annotations
                    .iter()
                    .any(|a| a.is_builtin("scalar_abi"));
            let ret_tuple = (!lua_opaque).then_some(declared_ret_tuple).flatten();
            // A flat-stored element passed to a `ref` parameter, or a `ref` result addressing
            // one, takes the `flat_ref` ABI. Only a direct call's own site has a flat plan.
            if matches!(self.cur_op, crate::site::SiteOp::DirectCall)
                && let Some(plan) = self.flat_call_plan(e)?
            {
                return match self
                .lower_flat_call(e, func, arg_idxs, any_named, &dc, &plan, ret_tuple)?
            {
                FlatCallOut::Value(value) => Ok(value),
                FlatCallOut::Flat(_) => Err(CompilerError::new_with_label(
                    self.body.expr_spans[e],
                    "this call returns a `ref` to a flat-stored container element; pass it to a \
                     `ref` parameter, bind it with `let`, read its fields or compare it with null"
                        .to_string(),
                )),
            };
            }
            let (args, tail) = self.direct_call_args(e, func, arg_idxs, any_named, &dc, true)?;
            if let Some(tail) = tail {
                return self.emit_preflight_tail(e, tail, args);
            }
            let ret_shape = if dc.plan.uses_scalar_abi() {
                dc.plan.ret_shape()
            } else {
                self.authorize_gpu_boundary_boxes(&func, &args);
                None
            };
            self.emit_direct_call(e, func, args, ret_shape, ret_tuple, &dc)
        })()?;
        self.call_result_bundle(dc.plan.representation(), value)
    }

    pub(crate) fn direct_call_args(
        &mut self,
        e: Idx<Expr>,
        func: FuncRef,
        arg_idxs: &[Idx<Expr>],
        any_named: bool,
        dc: &DirectCall<'a>,
        allow_preflight: bool,
    ) -> Result<(Vec<Idx<Inst>>, Option<FuncRef>), CompilerError> {
        let preflight = if allow_preflight {
            self.preflight_plan(e, func, arg_idxs, dc)?
        } else {
            None
        };
        let Some(preflight) = preflight else {
            return self
                .direct_call_args_inner(e, func, arg_idxs, any_named, dc, None)
                .map(|args| (args, None));
        };
        self.preflight_reads.push(HashMap::default());
        let result =
            self.direct_call_args_inner(e, func, arg_idxs, any_named, dc, Some(&preflight));
        self.preflight_reads.pop();
        result.map(|args| (args, Some(preflight.tail)))
    }

    fn direct_call_args_inner(
        &mut self,
        e: Idx<Expr>,
        func: FuncRef,
        arg_idxs: &[Idx<Expr>],
        any_named: bool,
        dc: &DirectCall<'a>,
        preflight: Option<&PreflightPlan>,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        let print_routes = self.dispatch().print_routes(e);
        if !print_routes.is_empty() {
            return arg_idxs
                .iter()
                .map(|&arg| {
                    if print_routes.iter().any(|(rendered, _)| *rendered == arg) {
                        self.with_site_expr(arg, |s| s.stringify_structural(arg))
                    } else {
                        self.lower_expr(arg)
                    }
                })
                .collect();
        }
        if let Some(args) = self.sort_call_args(e, dc, arg_idxs, None)? {
            return dc.plan.transport_arguments(args);
        }
        let callee = &self.ws.module(func.module).functions[func.func];
        let scalar_abi = dc.plan.uses_scalar_abi();
        let shapes;
        let mode = if scalar_abi {
            // The callee's use of a parameter cannot change admission at its boundary.
            let dropped = dc.plan.dropped_opaque_parameters();
            if !dropped.is_empty() {
                let mut next_positional = 0;
                let boxed: Vec<_> = arg_idxs
                    .iter()
                    .copied()
                    .filter(|&arg| {
                        call_argument_parameter(self.body, arg, &mut next_positional, |name| {
                            callee.param_decls.iter().position(|p| p.name == name)
                        })
                        .is_some_and(|i| dropped.contains(&i))
                    })
                    .collect();
                self.reject_nobox_boxed_call_args(func, &boxed)?;
            }
            shapes = dc.plan.argument_shapes();
            let types = self
                .direct_call_signature(dc)
                .map_or(&[][..], |signature| &signature.params);
            CallArgMode::Scalar {
                shapes: &shapes,
                types,
            }
        } else {
            self.reject_nobox_boxed_call_args(func, arg_idxs)?;
            self.reject_column_container_opaque_slots(e, &dc.callee.instance)?;
            let has_tuple_storage = dc
                .plan
                .representation()
                .params()
                .iter()
                .any(|&id| matches!(dc.plan.rep_plan(id).layout(), RepLayout::Tuple(_)));
            if has_tuple_storage || (any_named && preflight.is_some()) {
                shapes = vec![&RepLayout::Opaque; callee.param_decls.len()];
                CallArgMode::Scalar {
                    shapes: &shapes,
                    types: &[],
                }
            } else {
                CallArgMode::Boxed(&dc.callee.instance, self.direct_call_signature(dc)?)
            }
        };
        let args = if any_named {
            self.declared_slotted_call_args(e, func, arg_idxs, mode, preflight)?
        } else {
            self.lower_call_arguments(e, arg_idxs, Some(func), None, mode, preflight)?
        };
        if !scalar_abi {
            return Ok(args);
        }
        if let Some(preflight) = preflight {
            let tail_instance = self.concrete_callee_for_site_op(
                e,
                crate::site::SiteOp::Synthesized("preflight_tail"),
            )?;
            if tail_instance.func != preflight.tail {
                return Err(self.gap_here("@preflight concrete tail differs from annotation"));
            }
            return self
                .callee_abi_plan_for_instance(&tail_instance)?
                .transport_arguments(args);
        }
        dc.plan.transport_arguments(args)
    }

    fn emit_direct_call(
        &mut self,
        e: Idx<Expr>,
        func: FuncRef,
        args: Vec<Idx<Inst>>,
        value: Option<&ValueShape>,
        tuple: Option<&TupleLayout>,
        dc: &DirectCall<'a>,
    ) -> Result<ValueRepr, CompilerError> {
        if let Some(shape) = value {
            let dests = self.fresh_cells(shape.leaf_count());
            let (call, leaves) = self.call_multi(dests, func, args);
            self.record_call(call, func, dc)?;
            return Ok(Self::rebuild_from_leaves(shape, &leaves));
        }
        if let Some(shape) = tuple {
            return self.emit_tuple_call_tree(e, func, args, shape, dc);
        }
        let call = self.emit_cur(Inst::Call { func, args });
        self.record_call(call, func, dc)?;
        Ok(ValueRepr::Scalar(call))
    }

    pub(crate) fn emit_tuple_call_tree(
        &mut self,
        e: Idx<Expr>,
        func: FuncRef,
        args: Vec<Idx<Inst>>,
        shape: &TupleLayout,
        dc: &DirectCall<'a>,
    ) -> Result<ValueRepr, CompilerError> {
        let slot_types = self.tuple_opaque_slot_types(e, shape);
        let dests: Vec<Idx<Cell>> = slot_types
            .into_iter()
            .map(|ty| self.f.new_cell_typed(None, ty))
            .collect();
        let (call, loads) = self.call_multi(dests, func, args);
        self.record_call(call, func, dc)?;
        Ok(self.tuple_result(shape, &loads))
    }

    pub(crate) fn tuple_result(&mut self, shape: &TupleLayout, slots: &[Idx<Inst>]) -> ValueRepr {
        if !shape.splats(self.scx.rep_arena()) {
            return ValueRepr::Scalar(self.emit_cur(Inst::MakeTuple(slots.to_vec())));
        }
        let mut idx = 0;
        self.tuple_from_slots(shape, slots, &mut idx)
    }

    /// Uses the ABI derived from the function type.
    pub(crate) fn indirect_call_by_plan(
        &mut self,
        plan: FunctionRepPlan,
        callee: Idx<Inst>,
        arg_idxs: &[Idx<Expr>],
    ) -> Result<ValueBundle, CompilerError> {
        let arena = self.scx.rep_arena();
        let mut argv = Vec::new();
        for (i, &a) in arg_idxs.iter().enumerate() {
            let slot = required_wire_slot(
                plan.params(),
                i,
                false,
                "indirect function-value call",
                |message| self.gap_here(message),
            )?;
            argv.extend(self.arg_values_for_slot(arena.layout(*slot), a)?);
        }
        let Some(result) = plan.result() else {
            let call = self.emit_cur(Inst::CallValue { callee, args: argv });
            self.f.call_rep_plans.insert(call, plan.clone());
            return self.call_result_bundle(&plan, ValueRepr::Scalar(call));
        };
        if let Some(shape) = arena.tuple_shape(result) {
            let dests = self.fresh_cells(shape.slot_count(self.scx.rep_arena()));
            let (call, loads) = self.call_value_multi(dests, callee, argv);
            self.f.call_rep_plans.insert(call, plan.clone());
            return Ok(ValueBundle::new(result, self.tuple_result(shape, &loads)));
        }
        let Some(shape) = arena.value_shape(result) else {
            let call = self.emit_cur(Inst::CallValue { callee, args: argv });
            self.f.call_rep_plans.insert(call, plan.clone());
            return Ok(ValueBundle::new(result, ValueRepr::Scalar(call)));
        };
        let dests = self.fresh_cells(shape.leaf_count());
        let (call, leaves) = self.call_value_multi(dests, callee, argv);
        self.f.call_rep_plans.insert(call, plan);
        Ok(ValueBundle::new(
            result,
            Self::rebuild_from_leaves(shape, &leaves),
        ))
    }

    /// Tuple returns use a multi-return capture so no slots are lost before packing.
    pub(crate) fn emit_call_value(
        &mut self,
        e: Idx<Expr>,
        callee: Idx<Inst>,
        args: Vec<Idx<Inst>>,
    ) -> Result<Idx<Inst>, CompilerError> {
        match self.expr_tuple_shape(e)? {
            Some(shape) => {
                let dests = self.fresh_cells(shape.slot_count(self.scx.rep_arena()));
                let (_, loads) = self.call_value_multi(dests, callee, args);
                let r = self.tuple_result(&shape, &loads);
                Ok(self.pack(r)?)
            }
            None => Ok(self.emit_cur(Inst::CallValue { callee, args })),
        }
    }

    /// Value results stay leaf-shaped for the safe-nav join.
    pub(crate) fn ufcs_call_repr(
        &mut self,
        e: Idx<Expr>,
        func: FuncRef,
        recv: ValueRepr,
        arg_idxs: &[Idx<Expr>],
    ) -> Result<ValueBundle, CompilerError> {
        let trusted = self.ws.module(func.module).functions[func.func]
            .inline_body
            .is_none();
        let r =
            self.with_value_lending(false, |s| s.ufcs_call_repr_inner(e, func, recv, arg_idxs))?;
        self.finish_call_repr(e, r, trusted)
    }

    fn ufcs_call_repr_inner(
        &mut self,
        e: Idx<Expr>,
        func: FuncRef,
        recv: ValueRepr,
        arg_idxs: &[Idx<Expr>],
    ) -> Result<ValueBundle, CompilerError> {
        let pack_receiver = |s: &mut Self, recv| match recv {
            ValueRepr::Scalar(value) => Ok(value),
            _ => s.pack_maybe_absent(recv),
        };
        let dc = self.plan_direct_call(e)?;
        let value = (|| -> Result<ValueRepr, CompilerError> {
            if let ValueRepr::Scalar(recv) = recv
                && let Some(args) = self.sort_call_args(e, &dc, arg_idxs, Some(recv))?
            {
                let args = dc.plan.transport_arguments(args)?;
                return self.emit_direct_call(e, func, args, None, None, &dc);
            }
            self.reject_flat_return_outside_direct_call(e, &dc.callee.instance)?;
            if !dc.plan.uses_scalar_abi() {
                let mut argv = vec![pack_receiver(self, recv)?];
                for &a in arg_idxs {
                    argv.push(self.lower_expr(a)?);
                }
                // Tuple slots go on unpacked, so the per-slot join keeps `nobox` elements
                // unboxed. A `(..)?` return forwards its tag: `a?.m()` is absent when
                // either link is.
                let ret_tuple = dc.plan.ret_tuple();
                return self.emit_direct_call(e, func, argv, None, ret_tuple, &dc);
            };
            let param_shapes = dc.plan.argument_shapes();
            let mut argv = Vec::new();
            // The receiver arrives already lowered, and a tuple receiver has no UFCS
            // spelling, so `Leaves` is the only splatting slot reachable here.
            let receiver_slot =
                *required_wire_slot(&param_shapes, 0, false, "UFCS receiver", |message| {
                    self.gap_here(message)
                })?;
            match receiver_slot {
                RepLayout::Leaves(shape) => {
                    let leaves = match recv {
                        ValueRepr::Scalar(recv) => self.cdata_leaves(recv, shape)?,
                        _ => self.repr_to_leaves_shape(&recv, shape)?,
                    };
                    argv.extend(leaves);
                }
                _ => argv.push(pack_receiver(self, recv)?),
            }
            for (i, &a) in arg_idxs.iter().enumerate() {
                let slot = *required_wire_slot(
                    &param_shapes,
                    i + 1,
                    self.ws.module(func.module).functions[func.func].is_variadic,
                    "UFCS call",
                    |message| self.gap_here(message),
                )?;
                argv.extend(self.arg_values_for_slot(slot, a)?);
            }
            let argv = dc.plan.transport_arguments(argv)?;
            self.emit_direct_call(e, func, argv, dc.plan.ret_shape(), None, &dc)
        })()?;
        self.call_result_bundle(dc.plan.representation(), value)
    }

    fn boxed_call_arg(
        &mut self,
        e: Idx<Expr>,
        arg: Idx<Expr>,
        inst: &crate::instance::Instance,
        signature: &crate::concrete::ConcreteSignature,
        index: usize,
        lend: Option<bool>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let ty = self
            .concrete_param_ty_of(signature, inst, index)?
            .ok_or_else(|| self.gap_here("concrete call has no callee parameter type"))?;
        if self.is_ref_call_arg(e, arg) {
            return self.lower_ref_borrow(arg);
        }
        let lower = |s: &mut Self| s.lower_expr_with_expected(arg, Some(ty.as_ref()));
        let value = match lend {
            Some(lend) => self.with_value_lending(lend, lower),
            None => lower(self),
        }?;
        Ok(self.coerced_mono(value, arg, ty.as_ref()))
    }

    fn declared_slotted_call_args(
        &mut self,
        e: Idx<Expr>,
        func: FuncRef,
        arg_idxs: &[Idx<Expr>],
        mode: CallArgMode<'_>,
        preflight: Option<&PreflightPlan>,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        let params = &self.ws.module(func.module).functions[func.func].param_decls;
        let names: Vec<_> = params.iter().map(|p| p.name).collect();
        self.lower_call_arguments(e, arg_idxs, Some(func), Some(&names), mode, preflight)
    }

    /// Evaluates in source order. Named and spread arguments can fill slots out of
    /// order, so their wires are kept until the full argument list is known.
    fn lower_call_arguments(
        &mut self,
        e: Idx<Expr>,
        arg_idxs: &[Idx<Expr>],
        func: Option<FuncRef>,
        param_names: Option<&[Ustr]>,
        mode: CallArgMode<'_>,
        preflight: Option<&PreflightPlan>,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        let slotted = param_names.is_some();
        let callee = func.map(|f| &self.ws.module(f.module).functions[f.func]);
        let variadic = callee.is_some_and(|f| f.is_variadic);
        let lend = callee.is_some_and(|f| f.inline_body.is_none() && !f.is_extern);
        let mut slots = Vec::new();
        let mut out = Vec::new();
        if let Some(names) = param_names {
            slots.resize_with(names.len(), || None);
        } else {
            out.reserve(arg_idxs.len());
        }

        let mut next_positional = 0;
        for (source_index, &arg) in arg_idxs.iter().enumerate() {
            let parameter = if let Some(names) = param_names {
                if self.body.spread_args.contains(&arg) {
                    for (name, bundle) in self.spread_field_bundles(arg)? {
                        let i = names.iter().position(|p| *p == name).ok_or_else(|| {
                            gap(format!("spread field `{name}` matches no parameter"))
                        })?;
                        slots[i] = Some(match mode {
                            CallArgMode::Boxed(..) => {
                                CallArgValue::Boxed(self.pack(bundle.into_value())?)
                            }
                            CallArgMode::Scalar { shapes, .. } => {
                                let slot = shapes.get(i).copied().unwrap_or(&RepLayout::Opaque);
                                let bundle = self.snapshot_call_bundle(bundle)?;
                                CallArgValue::Scalar(self.snapshot_argument_slot(bundle, slot)?)
                            }
                        });
                    }
                    continue;
                }
                call_argument_parameter(self.body, arg, &mut next_positional, |name| {
                    names.iter().position(|p| *p == name)
                })
                .ok_or_else(|| {
                    gap(format!(
                        "named argument `{}` matches no parameter",
                        self.body.arg_names[&arg]
                    ))
                })?
            } else {
                source_index
            };
            if slotted && parameter >= slots.len() {
                return Err(gap(format!(
                    "argument lands in slot {parameter} of {} parameters",
                    slots.len()
                )));
            }
            let mut value = match mode {
                CallArgMode::Boxed(inst, signature) => CallArgValue::Boxed(self.boxed_call_arg(
                    e,
                    arg,
                    inst,
                    signature,
                    parameter,
                    (!slotted).then_some(lend),
                )?),
                CallArgMode::Scalar { shapes, types } => {
                    let slot = if slotted {
                        shapes.get(parameter).copied().unwrap_or(&RepLayout::Opaque)
                    } else {
                        *required_wire_slot(
                            shapes,
                            parameter,
                            variadic,
                            "direct scalar-ABI call",
                            |message| self.gap_here(message),
                        )?
                    };
                    let slotted_ref = slotted && self.is_ref_call_arg(e, arg);
                    let mut values = if slotted_ref {
                        let place = self.lower_ref_borrow(arg)?;
                        let bundle = self.value_bundle_for_expr(arg, ValueRepr::Scalar(place))?;
                        self.repr_to_slot_values_bundle(&bundle, slot)?
                    } else if slotted {
                        let bundle = match self.json_vec_argument(arg, slot)? {
                            Some(value) => value,
                            None => self.lower_expr_repr(arg)?,
                        };
                        let bundle = self.snapshot_call_bundle(bundle)?;
                        self.snapshot_argument_slot(bundle, slot)?
                    } else {
                        self.arg_values_for_call_slot(e, slot, arg, lend)?
                    };
                    if !slotted_ref
                        && !slot.splats()
                        && let ([value], Some(&ty)) = (values.as_mut_slice(), types.get(parameter))
                    {
                        *value = self.coerced_mono(*value, arg, self.concrete_mono(ty));
                    }
                    CallArgValue::Scalar(values)
                }
            };
            if source_index == 0
                && let Some(plan) = preflight
                && let Some(receiver) = value.receiver_mut()
            {
                *receiver = self.emit_preflight(e, plan, *receiver)?;
            }
            if slotted {
                slots[parameter] = Some(value);
            } else {
                value.append_to(&mut out);
            }
        }

        let defaults = func
            .map(|f| self.call_param_defaults(f))
            .unwrap_or_default();
        let (start, limit) = if slotted {
            (0, slots.len())
        } else {
            (arg_idxs.len(), defaults.len())
        };
        let end = defaults
            .iter()
            .take(limit)
            .rposition(Option::is_some)
            .map_or(0, |i| i + 1);
        let end = end.max(slots.iter().rposition(Option::is_some).map_or(0, |i| i + 1));
        if slotted {
            out.reserve(end);
        }
        let mut supplied = slots.into_iter();
        for i in start..end {
            let slot = match mode {
                CallArgMode::Boxed(..) => &RepLayout::Opaque,
                CallArgMode::Scalar { shapes, .. } if slotted => {
                    *required_wire_slot(shapes, i, false, "slotted call", |message| {
                        self.gap_here(message)
                    })?
                }
                CallArgMode::Scalar { shapes, .. } => {
                    shapes.get(i).copied().unwrap_or(&RepLayout::Opaque)
                }
            };
            if let Some(Some(value)) = supplied.next() {
                value.append_to(&mut out);
            } else {
                self.append_omitted_argument(
                    &mut out,
                    defaults.get(i).and_then(Option::as_ref),
                    slot,
                )?;
            }
        }
        Ok(out)
    }

    // Capture values before another argument or a spread override can change them.
    fn snapshot_call_bundle(&mut self, bundle: ValueBundle) -> Result<ValueBundle, CompilerError> {
        let representation = bundle.representation();
        let owned = self.snapshot_opaque_value(bundle.into_value())?;
        Ok(ValueBundle::new(representation, owned))
    }

    /// The one place a `Group` becomes a real box (an escape: stored, compared or boxed).
    pub(crate) fn pack(&mut self, r: ValueRepr) -> Result<Idx<Inst>, CompilerError> {
        self.pack_boxed(r, BoxKind::Implicit)
    }

    pub(crate) fn pack_boxed(
        &mut self,
        r: ValueRepr,
        kind: BoxKind,
    ) -> Result<Idx<Inst>, CompilerError> {
        let r = match r {
            r @ ValueRepr::Group { .. } => self.owned_value_children(r)?,
            r => r,
        };
        match r {
            ValueRepr::Scalar(v) => {
                if !self.shallow_pack
                    && !self.lending_pack
                    && (self.shareable_value_boxes.contains(&v)
                        || matches!(self.f.insts[v], Inst::Call { .. } | Inst::CallValue { .. }))
                    && let Some(ty) = self.borrowed_value_children.get(&v).cloned()
                {
                    return self.copied_value_box(&ty, v);
                }
                Ok(v)
            }
            // A splatting tuple's `nobox` elements pack too, which is how a whole-tuple
            // use reaches the guarantee's rejection.
            ValueRepr::Tuple { parts, .. } => {
                let vs = parts
                    .into_iter()
                    .map(|p| self.pack_maybe_absent_boxed(p, kind))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(self.element_tuple(vs))
            }
            ValueRepr::Group {
                name,
                parts,
                present,
            } => {
                // A flat-enum Group packs to the `{__variant}` box, not a `<name>_ct` cdata.
                if let Some(plan) = EnumPlan::for_name(self.ws, name)
                    && let Some(shape) = plan.flat()
                {
                    let expected_len = shape.leaf_count();
                    if parts.len() != expected_len {
                        return Err(self.gap_here(format!(
                            "flat enum `{name}` produced {} fields for a plan requiring {expected_len}",
                            parts.len()
                        )));
                    }
                    let packed = parts
                        .into_iter()
                        .map(|p| self.pack_maybe_absent_boxed(p, kind))
                        .collect::<Result<Vec<_>, _>>()?;
                    return Ok(
                        self.materialize_flat_cells(shape, FlatCells::from_slots(shape, &packed))
                    );
                }
                // Ordinary nullable boxes preserve absence as nil. `nobox` groups bypass
                // this so consumers handle presence explicitly and SROA still sees the ctor.
                if present.is_some() && !self.is_nobox_struct(name) {
                    return self.pack_maybe_absent_boxed(
                        ValueRepr::Group {
                            name,
                            parts,
                            present,
                        },
                        kind,
                    );
                }
                // A leaf-table struct's physical keys are its leaves, so it packs through the
                // flat `<Name>_lct` ctor; packing it field-wise would violate the plan.
                let storage = if self.is_value_struct(name) {
                    Some(
                        self.scx
                            .rep_arena()
                            .nominal_value_storage_required(self.ws, self.scx.target, name)
                            .map_err(|e| self.gap_here(e.message))?,
                    )
                } else {
                    None
                };
                // LuaJIT cannot record an aggregate argument to an FFI constructor.
                let leaf_ctor = storage == Some(ValueStorage::LeafTable)
                    || (self.scx.target == bone_core::hir::FnBackend::Lua
                        && storage == Some(ValueStorage::CData)
                        && self
                            .nominal_rep_shape_required(name, false)?
                            .has_registry_field());
                if leaf_ctor {
                    let representation = self.nominal_value_representation(name)?;
                    let shape = self
                        .scx
                        .rep_arena()
                        .value_shape(representation)
                        .ok_or_else(|| {
                            self.gap_here(format!("`{name}` has no planned value shape"))
                        })?;
                    let bundle = ValueBundle::new(
                        representation,
                        ValueRepr::Group {
                            name,
                            parts,
                            present,
                        },
                    );
                    let leaves = self.repr_to_leaves_bundle(&bundle)?;
                    let paths = value_leaf_paths(shape);
                    if paths.len() != leaves.len() {
                        return Err(self.gap_here(format!(
                            "`{name}` leaf plan has {} paths but the value produced {} leaves",
                            paths.len(),
                            leaves.len()
                        )));
                    }
                    let fields = paths.into_iter().zip(leaves).collect();
                    let ctor = kind.value_ctor(name, representation, ValueCtorForm::Leaves, fields);
                    Ok(self.emit_cur(ctor))
                } else {
                    self.pack_group_fields(name, parts, kind)
                }
            }
        }
    }

    /// Slots read in order out of one flat box are that box: a tuple is immutable,
    /// so it is shared, not copied.
    fn flat_tuple_box(&mut self, slots: Vec<Idx<Inst>>) -> Idx<Inst> {
        match self.flat_box_of_slots(&slots) {
            Some(base) => base,
            None => self.emit_cur(Inst::MakeTuple(slots)),
        }
    }

    pub(crate) fn flat_box_of_repr(&self, value: &ValueRepr) -> Option<Idx<Inst>> {
        fn leaves(value: &ValueRepr, out: &mut Vec<Idx<Inst>>) {
            match value {
                ValueRepr::Scalar(v) => out.push(*v),
                ValueRepr::Group { parts, present, .. } | ValueRepr::Tuple { parts, present } => {
                    out.extend(*present);
                    parts.iter().for_each(|p| leaves(p, out));
                }
            }
        }
        if let ValueRepr::Scalar(v) = value {
            return (!self.is_tuple_aggregate(*v)
                && !matches!(self.f.insts[*v], Inst::Const(Lit::Nil)))
            .then_some(*v);
        }
        let mut out = Vec::new();
        leaves(value, &mut out);
        self.flat_box_of_slots(&out)
    }

    /// Only when `slots` were read out of one box in order and in full.
    pub(crate) fn flat_box_of_slots(&self, slots: &[Idx<Inst>]) -> Option<Idx<Inst>> {
        let Some(&Inst::TupleIndex { base, index: 0 }) = slots.first().map(|&v| &self.f.insts[v])
        else {
            return None;
        };
        let in_order = slots.iter().enumerate().all(|(i, &v)| {
            matches!(self.f.insts[v], Inst::TupleIndex { base: b, index } if b == base && index == i)
        });
        (in_order
            && !self.is_tuple_aggregate(base)
            && self.flat_box_width(base) == Some(slots.len()))
        .then_some(base)
    }

    pub(crate) fn pack_bundle(&mut self, bundle: &ValueBundle) -> Result<Idx<Inst>, CompilerError> {
        if let ValueRepr::Group {
            name,
            parts,
            present: Some(present),
        } = bundle.value()
        {
            return self.pack_typed_nullable(*name, parts.clone(), *present, BoxKind::Implicit);
        }
        let Some(shape) = self.scx.rep_arena().tuple_shape(bundle.representation()) else {
            return self.pack(bundle.value().clone());
        };
        // A tuple that is already a box is shared, since it is immutable. Only an
        // element-wise box of a splatting tuple changes layout.
        if let ValueRepr::Scalar(boxed) = bundle.value()
            && !(shape.splats(self.scx.rep_arena()) && self.is_tuple_aggregate(*boxed))
        {
            return Ok(*boxed);
        }
        let mut slots = self.tuple_repr_to_slots(bundle.value().clone(), shape)?;
        if !shape.nullable {
            return Ok(self.flat_tuple_box(slots));
        }
        let present = slots.remove(0);
        self.nil_or_join(present, true, |s| Ok(s.emit_cur(Inst::MakeTuple(slots))))
    }

    /// Preserves absence as `nil`; nullable `nobox` groups require this explicitly.
    pub(crate) fn pack_maybe_absent(&mut self, r: ValueRepr) -> Result<Idx<Inst>, CompilerError> {
        self.pack_maybe_absent_boxed(r, BoxKind::Implicit)
    }

    /// Parent constructors need a single representation, so this emits no join.
    fn pack_typed_nullable(
        &mut self,
        name: Ustr,
        parts: Vec<ValueRepr>,
        present: Idx<Inst>,
        kind: BoxKind,
    ) -> Result<Idx<Inst>, CompilerError> {
        let value = self.pack_boxed(
            ValueRepr::Group {
                name,
                parts,
                present: None,
            },
            kind,
        )?;
        Ok(self.emit_cur(Inst::MakeNullableValue { present, value }))
    }

    pub(crate) fn pack_maybe_absent_boxed(
        &mut self,
        r: ValueRepr,
        kind: BoxKind,
    ) -> Result<Idx<Inst>, CompilerError> {
        let mut value = r;
        let present = match &mut value {
            ValueRepr::Group { present, .. } | ValueRepr::Tuple { present, .. } => present.take(),
            ValueRepr::Scalar(_) => None,
        };
        let Some(present) = present else {
            return self.pack_boxed(value, kind);
        };
        // The present arm takes the same tier dispatch: a leaf-table or flat-enum
        // payload has no declared-field ctor.
        self.nil_or_join(present, true, |s| s.pack_boxed(value, kind))
    }

    /// First-class targets nest children as declared fields and never take this layout.
    pub(crate) fn gc_table_inline_layout(
        &self,
        name: Ustr,
    ) -> Result<Option<Rc<[GcTableField]>>, CompilerError> {
        if self.scx.values_first_class || !self.is_value_struct(name) {
            return Ok(None);
        }
        let storage = self
            .scx
            .rep_arena()
            .nominal_value_storage_required(self.ws, self.scx.target, name)
            .map_err(|e| self.gap_here(e.message))?;
        if storage != ValueStorage::GcTable {
            return Ok(None);
        }
        super::shape::gc_table_inline_fields(self.ws, self.scx.target, name)
            .map_err(|e| self.gap_here(e.message))
    }

    /// A presence tag canonicalizes to `true`/`nil`, the form a `= null` write
    /// stores; the scalar ABI spells absence `false`, and `__eq` compares tags per key.
    fn gc_table_box_fields(
        &mut self,
        name: Ustr,
        layout: &[GcTableField],
        parts: Vec<ValueRepr>,
        kind: BoxKind,
    ) -> Result<Vec<(Ustr, Idx<Inst>)>, CompilerError> {
        if parts.len() != layout.len() {
            return Err(self.gap_here(format!(
                "GC-table value `{name}` has {} parts for {} fields",
                parts.len(),
                layout.len()
            )));
        }
        let mut out = Vec::new();
        for (field, part) in layout.iter().zip(parts) {
            let Some(sub) = &field.inline else {
                out.push((field.name, self.pack_maybe_absent_boxed(part, kind)?));
                continue;
            };
            let leaves = self.repr_to_leaves_shape(&part, sub)?;
            let keys = field.keys();
            if keys.len() != leaves.len() {
                return Err(self.gap_here(format!(
                    "`{name}.{}` has {} keys but the value produced {} leaves",
                    field.name,
                    keys.len(),
                    leaves.len()
                )));
            }
            for ((key, presence), leaf) in keys.into_iter().zip(leaves) {
                let value = if presence {
                    let tag = self.emit_cur(Inst::Const(Lit::Bool(true)));
                    self.emit_cur(Inst::MakeNullableValue {
                        present: leaf,
                        value: tag,
                    })
                } else {
                    leaf
                };
                out.push((key, value));
            }
        }
        Ok(out)
    }

    /// Parts are in declared field order, as the positional ctor ABI requires. The
    /// names let SROA forward a `Field` read to its ctor operand.
    fn pack_group_fields(
        &mut self,
        name: Ustr,
        parts: Vec<ValueRepr>,
        kind: BoxKind,
    ) -> Result<Idx<Inst>, CompilerError> {
        let representation = self.nominal_value_representation(name)?;
        if let Some(layout) = self.gc_table_inline_layout(name)? {
            let fields = self.gc_table_box_fields(name, &layout, parts, kind)?;
            let ctor = kind.value_ctor(name, representation, ValueCtorForm::Declared, fields);
            return Ok(self.emit_cur(ctor));
        }
        let names = struct_field_names(self.ws, name);
        let fields = parts
            .into_iter()
            .enumerate()
            .map(|(i, p)| -> Result<(Ustr, Idx<Inst>), CompilerError> {
                let fname = *names
                    .get(i)
                    .ok_or_else(|| self.gap_here("a planned value group has too many fields"))?;
                // A nullable nobox field is observable as null through its parent, so
                // its presence must reach the field.
                if let ValueRepr::Group {
                    name,
                    parts,
                    present: Some(present),
                } = p
                {
                    return Ok((fname, self.pack_typed_nullable(name, parts, present, kind)?));
                }
                Ok((fname, self.pack_boxed(p, kind)?))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let ctor = kind.value_ctor(name, representation, ValueCtorForm::Declared, fields);
        Ok(self.emit_cur(ctor))
    }

    /// A scalar ABI with no first parameter is invalid rather than silently
    /// dropping the collection.
    pub(crate) fn iter_witness_args(
        &mut self,
        inst: &crate::instance::Instance,
        bundle: ValueBundle,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        let iter_ref = inst.func;
        let plan = self.callee_abi_plan_for_instance(inst)?;
        let slot = if !plan.uses_scalar_abi() {
            &RepLayout::Opaque
        } else {
            plan.argument_shapes().first().cloned().ok_or_else(|| {
                gap(format!(
                    "`for`-protocol iter witness `{}` has a scalar ABI with no parameters, \
                     so it cannot receive the collection",
                    self.ws.module(iter_ref.module).functions[iter_ref.func].name
                ))
            })?
        };
        let args = self.repr_to_slot_values_bundle(&bundle, slot)?;
        plan.transport_arguments(args)
    }

    pub(crate) fn repr_to_leaves_shape(
        &mut self,
        r: &ValueRepr,
        shape: &ValueShape,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        match r {
            ValueRepr::Tuple { .. } => {
                let v = self.pack(r.clone())?;
                self.repr_to_leaves_shape(&ValueRepr::Scalar(v), shape)
            }
            ValueRepr::Group { parts, present, .. } => {
                let mut out = Vec::new();
                if shape.nullable {
                    out.push(match present {
                        Some(value) => *value,
                        None => self.emit_cur(Inst::Const(Lit::Bool(true))),
                    });
                }
                for ((_, fs), part) in shape.fields.iter().zip(parts) {
                    match fs {
                        FieldShape::Prim => out.push(self.pack_maybe_absent(part.clone())?),
                        FieldShape::Nested(sub) => {
                            out.extend(self.repr_to_leaves_shape(part, sub)?)
                        }
                    }
                }
                Ok(out)
            }
            ValueRepr::Scalar(v) if shape.nullable => {
                let field_count = shape.leaf_count() - 1;
                if matches!(self.f.insts[*v], Inst::Const(Lit::Nil)) {
                    let mut out = Vec::with_capacity(1 + field_count);
                    out.push(self.emit_cur(Inst::Const(Lit::Bool(false))));
                    for _ in 0..field_count {
                        out.push(self.emit_cur(Inst::Const(Lit::Nil)));
                    }
                    return Ok(out);
                }

                self.nullable_value_leaves(*v, shape, field_count, Self::cdata_leaves)
            }
            ValueRepr::Scalar(v) => self.cdata_leaves(*v, shape),
        }
    }

    pub(crate) fn repr_to_leaves_bundle(
        &mut self,
        bundle: &ValueBundle,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        let shape = self
            .scx
            .rep_arena()
            .value_shape(bundle.representation())
            .ok_or_else(|| {
                CompilerError::new_no_location(format!(
                    "representation {:?} has no value leaf shape",
                    bundle.representation()
                ))
            })?;
        self.repr_to_leaves_shape(bundle.value(), shape)
    }

    pub(super) fn repr_to_leaves_for_shape(
        &mut self,
        bundle: &ValueBundle,
        actual: &ValueShape,
        expected: &ValueShape,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        let same_fields = actual.name == expected.name && actual.fields == expected.fields;
        if !same_fields {
            return Err(self.gap_here(format!(
                "representation {:?} shape {:?} does not match destination value shape {:?}",
                bundle.representation(),
                actual,
                expected
            )));
        }
        if actual.nullable
            && !expected.nullable
            && !matches!(
                bundle.value(),
                ValueRepr::Group {
                    present: Some(_),
                    ..
                }
            )
        {
            // Dropping the presence leaf is valid only after the group proves presence.
            return Err(self.gap_here(format!(
                "representation {:?} lost its presence witness before narrowing",
                bundle.representation()
            )));
        }
        self.repr_to_leaves_shape(bundle.value(), expected)
    }

    pub(crate) fn repr_to_leaves_bundle_for_shape(
        &mut self,
        bundle: &ValueBundle,
        expected: &ValueShape,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        let actual = self
            .scx
            .rep_arena()
            .value_shape(bundle.representation())
            .ok_or_else(|| {
                self.gap_here(format!(
                    "representation {:?} has no value leaf plan while lowering `{}`",
                    bundle.representation(),
                    self.f.name
                ))
            })?;
        self.repr_to_leaves_for_shape(bundle, actual, expected)
    }

    pub(crate) fn repr_to_storage_leaves(
        &mut self,
        bundle: &ValueBundle,
        expected: &ValueShape,
        reason: &str,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        let owned = self.owned_value_children(bundle.value().clone())?;
        let bundle = &ValueBundle::new(bundle.representation(), owned);
        let plan = self
            .scx
            .rep_arena()
            .try_get(bundle.representation())
            .ok_or_else(|| {
                self.gap_here(format!(
                    "representation {:?} does not belong to this build arena at {reason}",
                    bundle.representation()
                ))
            })?;
        match plan.layout() {
            RepLayout::Opaque => self.repr_to_leaves_shape(bundle.value(), expected),
            RepLayout::Leaves(actual) => self.repr_to_leaves_for_shape(bundle, actual, expected),
            RepLayout::Tuple(_) => Err(self.gap_here(format!(
                "tuple representation {:?} cannot fill a value storage shape at {reason}",
                bundle.representation()
            ))),
        }
    }

    pub(crate) fn repr_to_slot_values_bundle(
        &mut self,
        bundle: &ValueBundle,
        slot: &RepLayout,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        match slot {
            RepLayout::Opaque => self.pack_bundle(bundle).map(|value| vec![value]),
            RepLayout::Leaves(expected) => {
                let plan = self
                    .scx
                    .rep_arena()
                    .try_get(bundle.representation())
                    .ok_or_else(|| {
                        self.gap_here(format!(
                            "representation {:?} is missing at a leaves slot while lowering `{}`",
                            bundle.representation(),
                            self.f.name
                        ))
                    })?;
                match plan.layout() {
                    RepLayout::Leaves(actual) => {
                        if actual != expected
                            && super::abi::expanded_nullable_parameter_shape(self.ws, actual)
                                == *expected
                        {
                            self.repr_to_leaves_shape(bundle.value(), expected)
                        } else {
                            self.repr_to_leaves_for_shape(bundle, actual, expected)
                        }
                    }
                    RepLayout::Opaque => self.repr_to_leaves_shape(bundle.value(), expected),
                    RepLayout::Tuple(_) => Err(self.gap_here(format!(
                        "tuple representation {:?} cannot fill a leaves slot",
                        bundle.representation()
                    ))),
                }
            }
            RepLayout::Tuple(expected) => {
                // A box of the tuple is read through its slots, nil-tested when a slot is nullable.
                if let ValueRepr::Scalar(_) = bundle.value()
                    && matches!(
                        self.scx.rep_arena().get(bundle.representation()).layout(),
                        RepLayout::Opaque
                    )
                {
                    return self.tuple_repr_to_slots(bundle.value().clone(), expected);
                }
                let actual = self
                    .scx
                    .rep_arena()
                    .tuple_shape(bundle.representation())
                    .ok_or_else(|| {
                        self.gap_here(format!(
                            "representation {:?} has no tuple plan for a nested slot",
                            bundle.representation()
                        ))
                    })?;
                if !actual.can_fill(expected, self.scx.rep_arena()) {
                    return Err(self.gap_here(format!(
                        "representation {:?} tuple shape {:?} does not match its nested tuple slot {:?}",
                        bundle.representation(), actual, expected
                    )));
                }
                self.tuple_repr_to_slots(bundle.value().clone(), expected)
            }
        }
    }

    /// The inverse of `pack`. A flat enum box reads `__variant`, positional `[i+1]`
    /// and `.recfield` keys in shape order.
    pub(crate) fn cdata_leaves(
        &mut self,
        v: Idx<Inst>,
        shape: &ValueShape,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        self.cdata_leaf_range(v, shape, 0..shape.leaf_count())
    }

    /// Nullable ancestors still gate their selected payload reads.
    pub(crate) fn cdata_leaf_range(
        &mut self,
        v: Idx<Inst>,
        shape: &ValueShape,
        range: std::ops::Range<usize>,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        let demand: Vec<bool> = (0..shape.leaf_count())
            .map(|i| range.contains(&i))
            .collect();
        self.cdata_leaf_demand(v, shape, &demand)
    }

    pub(crate) fn cdata_leaf_demand(
        &mut self,
        v: Idx<Inst>,
        shape: &ValueShape,
        demand: &[bool],
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        debug_assert_eq!(demand.len(), shape.leaf_count());
        if let Some(es) =
            EnumPlan::for_name(self.ws, shape.name).and_then(|plan| plan.flat().cloned())
        {
            // The box may be `nil` (a null nullable enum), and `nil.__variant` faults.
            let n = es.leaf_count();
            let cells = self.fresh_cells(n);
            let nil_bb = self.f.new_block();
            let val_bb = self.f.new_block();
            let join = self.f.new_block();
            self.branch_on_nil(v, nil_bb, val_bb);
            self.cur = nil_bb;
            for c in &cells {
                let z = self.emit_cur(Inst::Const(Lit::Nil));
                self.store_cell(*c, z);
            }
            self.f.set_term(self.cur, Term::Goto(join));
            self.cur = val_bb;
            let leaves = self.spread_box_to_cells(v, &es);
            for (c, lv) in cells.iter().zip(leaves.slots()) {
                self.store_cell(*c, lv);
            }
            self.f.set_term(self.cur, Term::Goto(join));
            self.cur = join;
            let selected: Vec<_> = cells
                .iter()
                .zip(demand)
                .filter_map(|(&c, &used)| used.then_some(c))
                .collect();
            return Ok(self.load_cells(&selected));
        }
        // Paths come from `boxed_leaf_paths`, the rule the uniform packer reads a
        // global with, so the two readers cannot drift.
        if self.shape_is_plain_keys(shape) {
            // Constructor-derived projections keep logical paths so copy propagation can
            // eliminate their intermediate value constructors.
            let mut root = v;
            while let Inst::Field { base, .. } = self.f.insts[root] {
                root = base;
            }
            let native_aliases = matches!(
                self.f.insts[v],
                Inst::Global(_) | Inst::Index { .. } | Inst::Field { .. }
            ) && !matches!(
                self.f.insts[root],
                Inst::MakeValueStruct { .. } | Inst::MakeStruct { .. }
            );
            return self.boxed_plain_leaves(v, shape, native_aliases, Some(demand));
        }
        // A leaf-table box stores one key per leaf, the inverse of the `<Name>_lct`
        // ctor, with a nullable child's presence key at the child's own name.
        let storage = self
            .scx
            .rep_arena()
            .nominal_value_storage_required(self.ws, self.scx.target, shape.name)
            .map_err(|e| self.gap_here(e.message))?;
        if storage == ValueStorage::LeafTable && !shape.nullable {
            return Ok(value_leaf_paths(shape)
                .into_iter()
                .zip(demand)
                .filter_map(|(path, &used)| used.then_some(path))
                .map(|path| self.read_value_field(v, path))
                .collect());
        }
        // A GC-table box keys an inline child's leaves by dotted path: the exact
        // inverse of `gc_table_box_fields`, presence tag included.
        let inline = self.gc_table_inline_layout(shape.name)?;
        let mut out = Vec::new();
        let mut offset = 0;
        for (i, (fname, fs)) in shape.fields.iter().enumerate() {
            let count = fs.leaf_count();
            let child_demand = &demand[offset..offset + count];
            offset += count;
            if !child_demand.iter().any(|&used| used) {
                continue;
            }
            if let Some(field) = inline
                .as_ref()
                .and_then(|layout| layout.get(i))
                .filter(|f| f.is_inline())
            {
                for (key, _) in field
                    .keys()
                    .into_iter()
                    .zip(child_demand)
                    .filter_map(|(key, &used)| used.then_some(key))
                {
                    out.push(self.read_value_field(v, key));
                }
                continue;
            }
            let fv = self.read_value_field(v, *fname);
            match fs {
                FieldShape::Prim => out.push(fv),
                FieldShape::Nested(sub) if sub.nullable => {
                    out.extend(self.nullable_child_leaf_demand(fv, sub, child_demand)?)
                }
                FieldShape::Nested(sub) => {
                    out.extend(self.cdata_leaf_demand(fv, sub, child_demand)?)
                }
            }
        }
        Ok(out)
    }

    /// Must agree leaf-for-leaf with `repr_to_leaves`' nullable `Scalar` arm, or
    /// `rebuild_from_leaves` indexes past the end.
    fn nullable_child_leaf_demand(
        &mut self,
        fv: Idx<Inst>,
        sub: &ValueShape,
        demand: &[bool],
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        let count = demand[1..].iter().filter(|&&used| used).count();
        let mut leaves = self.nullable_value_leaves(fv, sub, count, |s, value, shape| {
            s.cdata_leaf_demand(value, shape, &demand[1..])
        })?;
        if !demand[0] {
            leaves.remove(0);
        }
        Ok(leaves)
    }

    /// Split a possibly nil box into presence and the requested payload leaves.
    fn nullable_value_leaves(
        &mut self,
        fv: Idx<Inst>,
        sub: &ValueShape,
        field_count: usize,
        read: impl FnOnce(&mut Self, Idx<Inst>, &ValueShape) -> Result<Vec<Idx<Inst>>, CompilerError>,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        let nil = self.emit_cur(Inst::Const(Lit::Nil));
        let present = self.emit_cur(Inst::NotEq(fv, nil));
        let cells = self.fresh_cells(field_count);
        let nil_bb = self.f.new_block();
        let value_bb = self.f.new_block();
        let join = self.f.new_block();
        self.branch_to(present, value_bb, nil_bb);
        self.cur = nil_bb;
        for c in &cells {
            let z = self.emit_cur(Inst::Const(Lit::Nil));
            self.store_cell(*c, z);
        }
        self.f.set_term(self.cur, Term::Goto(join));
        self.cur = value_bb;
        let mut inner = sub.clone();
        inner.nullable = false;
        let leaves = read(self, fv, &inner)?;
        for (c, lv) in cells.iter().zip(leaves) {
            self.store_cell(*c, lv);
        }
        self.f.set_term(self.cur, Term::Goto(join));
        self.cur = join;
        let mut out = Vec::with_capacity(1 + field_count);
        out.push(present);
        out.extend(self.load_cells(&cells));
        Ok(out)
    }

    /// Depth-first leaf order, as consumed by `rebuild_from_leaves` and the boxed reader.
    pub(crate) fn value_projection_range(
        &self,
        shape: &ValueShape,
        path: &[Ustr],
    ) -> Option<(std::ops::Range<usize>, FieldShape)> {
        Self::project_value_fields(shape, path.iter().map(|&field| (field, false)), |_| {})
    }

    pub(super) fn project_value_fields(
        root: &ValueShape,
        path: impl IntoIterator<Item = (Ustr, bool)>,
        mut guard: impl FnMut(usize),
    ) -> Option<(std::ops::Range<usize>, FieldShape)> {
        let mut selected: Option<&FieldShape> = None;
        let mut offset = 0;
        for (field, safe) in path {
            let shape = match selected {
                None => root,
                Some(FieldShape::Nested(shape)) => shape,
                Some(FieldShape::Prim) => return None,
            };
            if shape.nullable && safe {
                guard(offset);
            }
            offset += usize::from(shape.nullable);
            selected = None;
            for (name, field_shape) in &shape.fields {
                if *name == field {
                    selected = Some(field_shape);
                    break;
                }
                offset += field_shape.leaf_count();
            }
            selected?;
        }
        let selected = selected?.clone();
        let count = selected.leaf_count();
        Some((offset..offset + count, selected))
    }

    pub(crate) fn shape_has_flat_enum(&self, shape: &ValueShape) -> bool {
        EnumPlan::for_name(self.ws, shape.name).is_some_and(|plan| plan.flat().is_some())
            || shape.fields.iter().any(|(_, field)| match field {
                FieldShape::Nested(child) => self.shape_has_flat_enum(child),
                FieldShape::Prim => false,
            })
    }

    pub(crate) fn shape_is_plain_keys(&self, shape: &ValueShape) -> bool {
        if shape.nullable || EnumPlan::for_name(self.ws, shape.name).is_some() {
            return false;
        }
        shape.fields.iter().all(|(_, fs)| match fs {
            FieldShape::Prim => true,
            FieldShape::Nested(sub) => self.shape_is_plain_keys(sub),
        })
    }

    /// In `repr_to_leaves` order. A leaf-table box keys every leaf by its dotted
    /// path at the top level.
    pub(crate) fn boxed_leaf_paths(
        &self,
        shape: &ValueShape,
        native_aliases: bool,
    ) -> Result<Vec<Vec<Ustr>>, CompilerError> {
        let mut out = Vec::new();
        self.boxed_leaf_paths_into(shape, native_aliases, &mut Vec::new(), &mut out)?;
        Ok(out)
    }

    fn boxed_leaf_paths_into(
        &self,
        shape: &ValueShape,
        native_aliases: bool,
        prefix: &mut Vec<Ustr>,
        out: &mut Vec<Vec<Ustr>>,
    ) -> Result<(), CompilerError> {
        let storage = self
            .scx
            .rep_arena()
            .nominal_value_storage_required(self.ws, self.scx.target, shape.name)
            .map_err(|e| self.gap_here(e.message))?;
        let native_aliases = native_aliases
            && self.scx.target == bone_core::hir::FnBackend::Lua
            && storage == ValueStorage::CData;
        if storage == ValueStorage::LeafTable || native_aliases {
            for path in value_leaf_paths(shape) {
                let key = if native_aliases && path.contains('.') {
                    cdata_leaf_alias(self.ws, shape.name, path.as_str())
                } else {
                    path
                };
                let mut full = prefix.clone();
                full.push(key);
                out.push(full);
            }
            return Ok(());
        }
        let inline = self.gc_table_inline_layout(shape.name)?;
        for (i, (fname, fs)) in shape.fields.iter().enumerate() {
            if let Some(field) = inline
                .as_ref()
                .and_then(|layout| layout.get(i))
                .filter(|f| f.is_inline())
            {
                for (key, _) in field.keys() {
                    let mut full = prefix.clone();
                    full.push(key);
                    out.push(full);
                }
                continue;
            }
            prefix.push(*fname);
            match fs {
                FieldShape::Prim => out.push(prefix.clone()),
                FieldShape::Nested(sub) => {
                    self.boxed_leaf_paths_into(sub, native_aliases, prefix, out)?
                }
            }
            prefix.pop();
        }
        Ok(())
    }

    pub(crate) fn boxed_plain_leaves(
        &mut self,
        value: Idx<Inst>,
        shape: &ValueShape,
        native_aliases: bool,
        demand: Option<&[bool]>,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        let paths = self.boxed_leaf_paths(shape, native_aliases)?;
        let mut loaded: Vec<(Vec<Ustr>, Idx<Inst>)> = Vec::new();
        let mut out = Vec::new();
        for (index, path) in paths.into_iter().enumerate() {
            if demand.is_some_and(|demand| !demand[index]) {
                continue;
            }
            let mut cur = value;
            for depth in 1..=path.len() {
                let prefix = &path[..depth];
                if let Some((_, inst)) = loaded.iter().find(|(p, _)| p.as_slice() == prefix) {
                    cur = *inst;
                    continue;
                }
                cur = self.read_value_field(cur, path[depth - 1]);
                if depth < path.len() {
                    loaded.push((prefix.to_vec(), cur));
                }
            }
            out.push(cur);
        }
        Ok(out)
    }

    /// Recurses into nested values so later place writes cannot mutate the
    /// snapshot; GC leaf pointees stay shared.
    pub(crate) fn boxed_value_copy_repr(
        &mut self,
        boxed: Idx<Inst>,
        name: Ustr,
        owner_ty: Option<&MonoTy>,
    ) -> Result<ValueRepr, CompilerError> {
        let shape = self.nominal_rep_shape_required(name, false)?;
        self.enter_value_copy(name)?;
        let copy = self.boxed_copy_repr_of(boxed, &shape, owner_ty);
        self.value_copy_stack.pop();
        copy
    }

    /// Copies are unrolled at compile time and there is no runtime copier, so a
    /// copy of `name` inside its own copy is rejected.
    fn enter_value_copy(&mut self, name: Ustr) -> Result<(), CompilerError> {
        if self.value_copy_stack.contains(&name) {
            return Err(self.gap_here(format!(
                "recursive value type `{name}` cannot be copied: its `{name}?` descendant would need a runtime copier"
            )));
        }
        self.value_copy_stack.push(name);
        Ok(())
    }

    fn value_field_ty(
        &self,
        owner: Ustr,
        owner_ty: Option<&MonoTy>,
        field: Ustr,
    ) -> Result<std::borrow::Cow<'a, MonoTy>, CompilerError> {
        let ty = self
            .struct_field_ty(owner, field)
            .ok_or_else(|| self.gap_here(format!("field `{owner}.{field}` is not declared")))?;
        match crate::monoty::convert_decl(self.ws, ty, &crate::monoty::MonoSubst::default()) {
            Ok(ty) => return Ok(std::borrow::Cow::Owned(ty)),
            Err(reason) if !self.struct_is_generic(owner) => {
                return Err(self.gap_here(format!(
                    "field `{owner}.{field}` has no concrete type: {reason:?}"
                )));
            }
            Err(_) => {}
        }
        let owner_ty = owner_ty.ok_or_else(|| {
            self.gap_here(format!(
                "a copy of generic value `{owner}` has no concrete type for `{field}`"
            ))
        })?;
        self.struct_field_ty_in(owner, field, owner_ty)
    }

    fn struct_is_generic(&self, name: Ustr) -> bool {
        self.ws.nominals.struct_id(name).is_some_and(|sid| {
            !self.ws.modules[sid.module.index()].structs[sid.def]
                .generics
                .is_empty()
        })
    }

    pub(crate) fn opaque_value_child(
        &self,
        owner: Ustr,
        owner_ty: Option<&MonoTy>,
        field: Ustr,
    ) -> Result<Option<std::borrow::Cow<'a, MonoTy>>, CompilerError> {
        if !self.struct_is_generic(owner) {
            let Some(ty) = self.struct_field_ty(owner, field) else {
                return Ok(None);
            };
            let Some((child, true)) = super::shape::nominal_name_from_ty(self.ws, ty) else {
                return Ok(None);
            };
            if !self.is_value_struct(child) || self.is_nobox_struct(child) {
                return Ok(None);
            }
        }
        let ty = self.value_field_ty(owner, owner_ty, field)?;
        let ordinary = ty
            .peel_nullable()
            .0
            .nominal()
            .is_some_and(|child| self.is_value_struct(child) && !self.is_nobox_struct(child));
        Ok(ordinary.then_some(ty))
    }

    fn nested_owner_ty(
        &self,
        owner: Ustr,
        owner_ty: Option<&MonoTy>,
        field: Ustr,
        child: Ustr,
    ) -> Result<Option<std::borrow::Cow<'a, MonoTy>>, CompilerError> {
        if !self.struct_is_generic(child) {
            return Ok(None);
        }
        self.value_field_ty(owner, owner_ty, field).map(Some)
    }

    /// The copy happens where the value escapes ([`Self::owned_value_children`]).
    pub(crate) fn mark_borrowed_value_children(
        &mut self,
        shape: &ValueShape,
        repr: &ValueRepr,
        owner_ty: Option<&MonoTy>,
    ) -> Result<(), CompilerError> {
        let ValueRepr::Group { parts, .. } = repr else {
            return Ok(());
        };
        if EnumPlan::for_name(self.ws, shape.name).is_some() {
            return Ok(());
        }
        for ((fname, fs), part) in shape.fields.iter().zip(parts) {
            match (fs, part) {
                (FieldShape::Prim, ValueRepr::Scalar(v)) => {
                    if let Some(ty) = self.opaque_value_child(shape.name, owner_ty, *fname)? {
                        self.borrowed_value_children.insert(*v, ty);
                    }
                }
                (FieldShape::Nested(sub), part) => {
                    let sub_ty = self.nested_owner_ty(shape.name, owner_ty, *fname, sub.name)?;
                    self.mark_borrowed_value_children(sub, part, sub_ty.as_deref())?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Expanded children are snapshotted by the immediately following scalar reads.
    pub(crate) fn own_opaque_parameter_children(
        &mut self,
        r: ValueRepr,
        shape: &ValueShape,
    ) -> Result<ValueRepr, CompilerError> {
        match r {
            ValueRepr::Group {
                name,
                parts,
                present,
            } if parts.len() == shape.fields.len() => {
                let parts = parts
                    .into_iter()
                    .zip(&shape.fields)
                    .map(|(part, (_, field))| match field {
                        FieldShape::Prim => self.owned_value_children(part),
                        FieldShape::Nested(child) => {
                            self.own_opaque_parameter_children(part, child)
                        }
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(ValueRepr::Group {
                    name,
                    parts,
                    present,
                })
            }
            // The boundary immediately reads this box into primitive lanes, and those
            // lanes, not a reconstructed box, own the snapshot.
            ValueRepr::Scalar(v)
                if super::shape::planned_value(self.ws, shape.name).is_some_and(|plan| {
                    plan.scalar_shape()
                        .is_some_and(|plain| plain.fields == shape.fields)
                }) =>
            {
                Ok(ValueRepr::Scalar(v))
            }
            r => self.owned_value_children(r),
        }
    }

    /// Immutable boundaries may share boxes whose source cannot be written through.
    pub(crate) fn owned_value_children(
        &mut self,
        r: ValueRepr,
    ) -> Result<ValueRepr, CompilerError> {
        if self.scx.values_first_class || self.shallow_pack {
            return Ok(r);
        }
        match r {
            ValueRepr::Scalar(v)
                if !(self.lending_pack && self.shareable_value_boxes.contains(&v)) =>
            {
                if let Some(ty) = self.borrowed_value_children.get(&v).cloned() {
                    let copy = self.copied_value_repr(&ty, v)?;
                    return self.owned_value_children(copy);
                }
                Ok(ValueRepr::Scalar(v))
            }
            ValueRepr::Group {
                name,
                parts,
                present,
            } => {
                let parts = parts
                    .into_iter()
                    .map(|part| match part {
                        ValueRepr::Scalar(v) => match self.borrowed_value_children.get(&v).cloned()
                        {
                            Some(_)
                                if self.lending_pack && self.shareable_value_boxes.contains(&v) =>
                            {
                                Ok(ValueRepr::Scalar(v))
                            }
                            Some(ty) => self.copied_value_box(&ty, v).map(ValueRepr::Scalar),
                            None => Ok(ValueRepr::Scalar(v)),
                        },
                        part => self.owned_value_children(part),
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(ValueRepr::Group {
                    name,
                    parts,
                    present,
                })
            }
            ValueRepr::Tuple { parts, present } => {
                let parts = parts
                    .into_iter()
                    .map(|p| self.owned_value_children(p))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(ValueRepr::Tuple { parts, present })
            }
            r => Ok(r),
        }
    }

    /// Retaining a value in cells loses its borrowed-box provenance, so children
    /// are snapshotted before a later expression can mutate their source.
    pub(crate) fn snapshot_opaque_value(
        &mut self,
        r: ValueRepr,
    ) -> Result<ValueRepr, CompilerError> {
        if matches!(&r, ValueRepr::Scalar(v) if !self.shareable_value_boxes.contains(v)) {
            self.with_value_lending(false, |s| {
                if let ValueRepr::Scalar(v) = &r
                    && let Some(ty) = s.borrowed_value_children.get(v).cloned()
                    && let (payload, true) = ty.peel_nullable()
                {
                    let representation = s.concrete_rep_id(&ty, RepUse::Return, false, false)?;
                    if let Some(shape) = s.scx.rep_arena().value_shape(representation) {
                        // Wide values retain the planner's boxed wire rather than widening a frame.
                        let leaves = s.nullable_value_leaves(
                            *v,
                            shape,
                            shape.leaf_count() - 1,
                            |s, value, inner| {
                                let copy = s.boxed_copy_repr_of(value, inner, Some(payload))?;
                                let copy = s.owned_value_children(copy)?;
                                s.repr_to_leaves_shape(&copy, inner)
                            },
                        )?;
                        return Ok(Self::rebuild_from_leaves(shape, &leaves));
                    }
                }
                s.owned_value_children(r)
            })
        } else {
            match r {
                ValueRepr::Group {
                    name,
                    parts,
                    present,
                } => {
                    let parts = parts
                        .into_iter()
                        .map(|part| self.snapshot_opaque_value(part))
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok(ValueRepr::Group {
                        name,
                        parts,
                        present,
                    })
                }
                ValueRepr::Tuple { parts, present } => {
                    let parts = parts
                        .into_iter()
                        .map(|part| self.snapshot_opaque_value(part))
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok(ValueRepr::Tuple { parts, present })
                }
                r => Ok(r),
            }
        }
    }

    /// For a `let` whose children are already snapshotted; shareable children stay borrowed.
    pub(crate) fn pack_lent(&mut self, r: ValueRepr) -> Result<Idx<Inst>, CompilerError> {
        let outer = std::mem::replace(&mut self.shallow_pack, true);
        let packed = self.pack(r);
        self.shallow_pack = outer;
        packed
    }

    pub(crate) fn value_repr_is_shareable(&self, r: &ValueRepr) -> bool {
        match r {
            ValueRepr::Scalar(v) => {
                !self.borrowed_value_children.contains_key(v)
                    || self.shareable_value_boxes.contains(v)
            }
            ValueRepr::Group { parts, .. } | ValueRepr::Tuple { parts, .. } => {
                parts.iter().all(|p| self.value_repr_is_shareable(p))
            }
        }
    }

    pub(crate) fn with_value_lending<T>(
        &mut self,
        lend: bool,
        f: impl FnOnce(&mut Self) -> Result<T, CompilerError>,
    ) -> Result<T, CompilerError> {
        let outer = std::mem::replace(&mut self.lending_pack, lend);
        let result = f(self);
        self.lending_pack = outer;
        result
    }

    fn mark_boundary_value_repr(
        &mut self,
        r: &ValueRepr,
        ty: &MonoTy,
        share: bool,
    ) -> Result<(), CompilerError> {
        let (payload, _) = ty.peel_nullable();
        match r {
            ValueRepr::Scalar(v)
                if payload.nominal().is_some_and(|name| {
                    self.is_value_struct(name) && !self.is_nobox_struct(name)
                }) =>
            {
                self.borrowed_value_children
                    .insert(*v, std::borrow::Cow::Owned(ty.clone()));
                if share {
                    self.shareable_value_boxes.insert(*v);
                }
            }
            ValueRepr::Group { name, .. } => {
                let shape = self.nominal_rep_shape_required(*name, false)?;
                self.mark_borrowed_value_children(&shape, r, Some(payload))?;
                if share {
                    self.share_borrowed_children(r);
                }
            }
            ValueRepr::Tuple { parts, .. } => {
                if let MonoTy::Tuple(types) = payload {
                    for (part, ty) in parts.iter().zip(types) {
                        self.mark_boundary_value_repr(part, ty, share)?;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub(crate) fn share_borrowed_children(&mut self, r: &ValueRepr) {
        match r {
            ValueRepr::Scalar(v) if self.borrowed_value_children.contains_key(v) => {
                self.shareable_value_boxes.insert(*v);
            }
            ValueRepr::Group { parts, .. } | ValueRepr::Tuple { parts, .. } => {
                for part in parts {
                    self.share_borrowed_children(part);
                }
            }
            _ => {}
        }
    }

    pub(crate) fn boxed_copy_uses_gc_fields(
        &self,
        shape: &ValueShape,
    ) -> Result<bool, CompilerError> {
        Ok(!self.scx.values_first_class
            && !shape.nullable
            && EnumPlan::for_name(self.ws, shape.name).is_none()
            && !self.shape_is_plain_keys(shape)
            && self
                .scx
                .rep_arena()
                .nominal_value_storage_required(self.ws, self.scx.target, shape.name)
                .map_err(|e| self.gap_here(e.message))?
                == ValueStorage::GcTable)
    }

    /// Enums are immutable, so a GC-table box's enum child is shared; only the
    /// value's own storage has to be fresh.
    fn boxed_copy_repr_of(
        &mut self,
        v: Idx<Inst>,
        shape: &ValueShape,
        owner_ty: Option<&MonoTy>,
    ) -> Result<ValueRepr, CompilerError> {
        if !self.boxed_copy_uses_gc_fields(shape)? {
            let leaves = self.cdata_leaves(v, shape)?;
            let copy = Self::rebuild_from_leaves(shape, &leaves);
            self.mark_borrowed_value_children(shape, &copy, owner_ty)?;
            return Ok(copy);
        }
        let inline = self.gc_table_inline_layout(shape.name)?;
        let mut parts = Vec::with_capacity(shape.fields.len());
        for i in 0..shape.fields.len() {
            parts.push(self.boxed_gc_field_copy_repr(
                v,
                shape,
                owner_ty,
                i,
                inline.as_ref().and_then(|layout| layout.get(i)),
            )?);
        }
        Ok(ValueRepr::Group {
            name: shape.name,
            parts,
            present: None,
        })
    }

    pub(crate) fn boxed_gc_field_copy_repr(
        &mut self,
        v: Idx<Inst>,
        shape: &ValueShape,
        owner_ty: Option<&MonoTy>,
        index: usize,
        inline: Option<&GcTableField>,
    ) -> Result<ValueRepr, CompilerError> {
        let (fname, fs) = &shape.fields[index];
        if let Some(field) = inline.filter(|f| f.is_inline()) {
            let leaves: Vec<Idx<Inst>> = field
                .keys()
                .into_iter()
                .map(|(key, _)| self.read_value_field(v, key))
                .collect();
            return Ok(match fs {
                FieldShape::Prim => ValueRepr::Scalar(leaves[0]),
                FieldShape::Nested(sub) => Self::rebuild_from_leaves(sub, &leaves),
            });
        }
        let fv = self.read_value_field(v, *fname);
        Ok(match fs {
            FieldShape::Prim => {
                if let Some(ty) = self.opaque_value_child(shape.name, owner_ty, *fname)? {
                    self.borrowed_value_children.insert(fv, ty);
                }
                ValueRepr::Scalar(fv)
            }
            FieldShape::Nested(sub)
                if EnumPlan::for_name(self.ws, sub.name)
                    .is_some_and(|plan| plan.flat().is_some()) =>
            {
                ValueRepr::Scalar(fv)
            }
            FieldShape::Nested(sub) if sub.nullable => {
                let leaves =
                    self.nullable_value_leaves(fv, sub, sub.leaf_count() - 1, Self::cdata_leaves)?;
                let copy = Self::rebuild_from_leaves(sub, &leaves);
                let sub_ty = self.nested_owner_ty(shape.name, owner_ty, *fname, sub.name)?;
                self.mark_borrowed_value_children(sub, &copy, sub_ty.as_deref())?;
                copy
            }
            FieldShape::Nested(sub) => {
                let sub_ty = self.nested_owner_ty(shape.name, owner_ty, *fname, sub.name)?;
                self.boxed_copy_repr_of(fv, sub, sub_ty.as_deref())?
            }
        })
    }

    /// Returns a fresh box or `nil`, not a `Group`: a Group carries no presence for
    /// `pack`, which would box an absent value and break `== null`.
    pub(crate) fn nullable_boxed_value_copy(
        &mut self,
        boxed: Idx<Inst>,
        name: Ustr,
        owner_ty: Option<&MonoTy>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let is_nil = self.is_nil(boxed);
        // The child copies made while packing are this copy's descendants.
        self.nil_or_join(is_nil, false, |s| {
            let shape = s.nominal_rep_shape_required(name, false)?;
            s.enter_value_copy(name)?;
            let packed = s
                .boxed_copy_repr_of(boxed, &shape, owner_ty)
                .and_then(|copy| s.pack(copy));
            s.value_copy_stack.pop();
            packed
        })
    }

    /// For a value already held by one owner that is about to be stored into another.
    pub(crate) fn copied_value_box(
        &mut self,
        ty: &MonoTy,
        v: Idx<Inst>,
    ) -> Result<Idx<Inst>, CompilerError> {
        self.with_value_lending(false, |s| match s.copied_value_repr(ty, v)? {
            ValueRepr::Scalar(copy) if copy != v && ty.peel_nullable().1 => {
                // An opaque child occupies one slot, so the nullable payload stays boxed.
                let nil = s.emit_cur(Inst::Const(Lit::Nil));
                let present = s.emit_cur(Inst::NotEq(copy, nil));
                Ok(s.emit_cur(Inst::MakeNullableValue {
                    present,
                    value: copy,
                }))
            }
            ValueRepr::Scalar(v) => Ok(v),
            copy => s.pack(copy),
        })
    }

    pub(crate) fn copied_value_repr(
        &mut self,
        ty: &MonoTy,
        v: Idx<Inst>,
    ) -> Result<ValueRepr, CompilerError> {
        let (payload, nullable) = ty.peel_nullable();
        let Some(name) = payload
            .nominal()
            .filter(|name| self.is_value_struct(*name) && !self.is_nobox_struct(*name))
        else {
            return Ok(ValueRepr::Scalar(v));
        };
        if self.scx.values_first_class {
            return Ok(ValueRepr::Scalar(v));
        }
        if nullable {
            return Ok(ValueRepr::Scalar(self.nullable_boxed_value_copy(
                v,
                name,
                Some(payload),
            )?));
        }
        self.boxed_value_copy_repr(v, name, Some(payload))
    }

    /// Column storage has no element slot to index, so the read decomposes into
    /// leaf reads; a whole-element `Index` would reach the emitter with no slot.
    pub(crate) fn loop_element_repr_at(
        &mut self,
        arr: Idx<Inst>,
        index: Idx<Inst>,
        kind: ContainerKind,
    ) -> Result<ValueRepr, CompilerError> {
        if let Some((elem, storage)) = FlatStorage::of_vec_kind(kind) {
            return self
                .flat_element_repr(arr, index, elem, storage)
                .map(ValueBundle::into_value);
        }
        match kind {
            ContainerKind::NativeBufferValue(elem) if self.is_leaf_addressed(elem)? => self
                .nb_leaf_element_repr(arr, index, elem)
                .map(ValueBundle::into_value),
            _ => Ok(ValueRepr::Scalar(self.read_index(arr, index, kind))),
        }
    }

    /// The inverse of `repr_to_leaves`, consuming leaves depth-first.
    pub(crate) fn rebuild_from_leaves(shape: &ValueShape, leaves: &[Idx<Inst>]) -> ValueRepr {
        Self::rebuild_leaves_at(shape, leaves, &mut 0)
    }

    pub(crate) fn rebuild_leaves_at(
        shape: &ValueShape,
        leaves: &[Idx<Inst>],
        idx: &mut usize,
    ) -> ValueRepr {
        let present = shape.nullable.then(|| {
            let value = leaves[*idx];
            *idx += 1;
            value
        });
        let parts = shape
            .fields
            .iter()
            .map(|(_, field)| match field {
                FieldShape::Prim => {
                    let value = leaves[*idx];
                    *idx += 1;
                    ValueRepr::Scalar(value)
                }
                FieldShape::Nested(sub) => Self::rebuild_leaves_at(sub, leaves, idx),
            })
            .collect();
        ValueRepr::Group {
            name: shape.name,
            parts,
            present,
        }
    }

    pub(crate) fn field_index(&self, name: Ustr, field: Ustr) -> Option<usize> {
        let fields = self.struct_field_names(name);
        fields?.iter().position(|f| *f == field)
    }
}
