//! `nobox` policy: the authorized-box mints (`materialize`, the `@gpu_only`
//! call boundary), the rejections that keep an implicit box from being emitted,
//! and the fn-value ABI compatibility check.
//!
//! This is the ENFORCEMENT side of the mark. The representation it enforces is
//! described in `shape`.
use super::*;

impl Builder<'_> {
    /// The caller reaches this only when the body keeps the boxed ABI
    /// (`BackendFnAbiPlan::scalar` is `None`).
    /// Under `values_first_class`, a `value` struct is already a first-class
    /// `vecN`, so there is no box to authorize.
    /// Re-mark a nobox box packed for a `@gpu_only` callee as an EXPLICIT one.
    ///
    /// Emission is identical (both render `<name>_ct`), the verifier ignores
    /// `MaterializeValueStruct`, and the flatten pass already treats it as
    /// sanctioned -- so this authorizes exactly the box the GPU boundary needs
    /// through the channel that already exists for authorized boxes, instead of
    /// switching the guarantee off for the calling body.
    ///
    /// Args are lowered per call site, so each box here is this call's own.
    pub(crate) fn authorize_gpu_boundary_boxes(&mut self, func: &FuncRef, args: &[Idx<Inst>]) {
        let callee = &self.ws.module(func.module).functions[func.func];
        if !crate::intrinsics::is_gpu_only(callee) {
            return;
        }
        for &a in args {
            self.authorize_nobox_box(a);
        }
    }

    /// Re-mark one implicit nobox box as AUTHORIZED, in place. The single
    /// conversion point: every boundary the compiler MINTS a sanctioned box at
    /// goes through here, so "which boxes are authorized" is one list of callers
    /// rather than a repeated `MakeValueStruct` -> `MaterializeValueStruct`
    /// rewrite. A no-op on anything that is not an implicit box of a `nobox`
    /// struct.
    pub(crate) fn authorize_nobox_box(&mut self, inst: Idx<Inst>) {
        let Inst::MakeValueStruct { name, fields } = &self.f.insts[inst] else { return };
        let Some(sid) = self.ws.struct_by_name.get(name) else { return };
        if !self.ws.modules[sid.module.index()].structs[sid.def].is_nobox {
            return;
        }
        let (name, fields) = (*name, fields.clone());
        self.f.insts[inst] = Inst::MaterializeValueStruct { name, fields };
    }

    pub(crate) fn reject_nobox_boxed_call_args(
        &self,
        func: FuncRef,
        args: &[Idx<Expr>],
    ) -> Result<(), CompilerError> {
        if self.scx.values_first_class {
            return Ok(());
        }
        let callee = &self.ws.module(func.module).functions[func.func];
        // A `@gpu_only` intrinsic's boxed param is not a real boundary: on the
        // GPU the arg is a native `vec2`/`vec4` and no box exists, and the Lua
        // body only `error(...)`s. Rather than exempting the CALLER's whole
        // body, the box is AUTHORIZED at this one boundary
        // ([`Self::authorize_gpu_boundary_boxes`]) -- the guarantee keeps
        // applying to everything else the caller does.
        // Rationale for shader work: docs/shaders.md, "nobox values and shaders".
        if crate::intrinsics::is_gpu_only(callee) {
            return Ok(());
        }
        for &arg in args {
            if self.is_explicit_materialize_call(arg) {
                continue;
            }
            if let Some(ty) = self.concrete_expr_ty(arg) {
                let Some(name) = ty.peel_nullable().0.nominal() else { continue };
                let Some(sid) = self.ws.struct_by_name.get(&name) else { continue };
                if self.ws.modules[sid.module.index()].structs[sid.def].is_nobox {
                    return Err(CompilerError::new_with_label(
                        self.body.expr_spans[arg],
                        format!(
                            "nobox value `{name}` cannot cross boxed call boundary `{}`",
                            callee.name
                        ),
                    ));
                }
                continue;
            }
            if self.concrete_plan.is_some() {
                continue;
            }
            let Some(TypeExpr::App {
                ctor: TypeCtor::Nominal(name),
                ..
            }) = self.expr_ty(arg)
            else {
                continue;
            };
            let Some(sid) = self.ws.struct_by_name.get(&name) else {
                continue;
            };
            if self.ws.modules[sid.module.index()].structs[sid.def].is_nobox {
                return Err(CompilerError::new_with_label(
                    self.body.expr_spans[arg],
                    format!(
                        "nobox value `{name}` cannot cross boxed call boundary `{}`",
                        callee.name
                    ),
                ));
            }
        }
        Ok(())
    }

    /// An OPAQUE body cannot be handed a COLUMN container.
    ///
    /// A column-stored `[P]` is per-leaf arrays with no `_data` at all. Codegen
    /// routes its OWN container operations around the stdlib primitives -- a
    /// column `push` emits stores and no call -- but that routing covers only the
    /// hand-maintained primitive set and reaches nothing a user writes. An
    /// `inline(lua)`/`inline(js)` body is a string to the compiler and an
    /// `extern` has no body at all, so neither can be adapted to the
    /// representation: `v._data` is simply `nil`, and the call silently answers
    /// wrong or dies indexing it.
    ///
    /// Not a `nobox` rule, unlike its sibling: an UNMARKED value struct is
    /// column-eligible on the same terms and fails identically. The element type
    /// is the discriminant -- a boxed `[int]` keeps `_data` and is fine.
    ///
    /// Keyed on the DECLARED SEMANTIC param type under the concrete
    /// substitution, not the argument's (`docs/boundary-contracts.md` 2.4). A
    /// param spelled `[T]` or `Map<K, V>` states that the body expects that
    /// container's boxed representation, and a column-stored one is not it. A
    /// param spelled `any` states the opposite -- it claims no representation at
    /// all -- and the column-introspection probes that read `v["@vec"]`
    /// deliberately take `any`, so keying on the argument would reject the
    /// bodies that handle columns CORRECTLY.
    ///
    /// A method's IMPLICIT receiver is such a declared type: it is `param_decls`
    /// slot 0 and carries the semantic `Map<K, V>` though the source writes no
    /// type for it. Reading the written syntax instead would miss every method.
    ///
    /// The intercepted stdlib names are exempt BY CONSTRUCTION, not by an
    /// exception here: `lower_call` replaces them and returns before this check
    /// is reached.
    pub(crate) fn reject_column_container_opaque_slots(
        &self,
        e: Idx<Expr>,
        func: FuncRef,
        concrete_callee: Option<&crate::instance::Instance>,
    ) -> Result<(), CompilerError> {
        if let Some(instance) = concrete_callee {
            let plan = self
                .concrete_plan
                .ok_or_else(|| self.gap_here("concrete callee has no owner plan"))?;
            let site = crate::site::Site {
                domain: self.cur_domain.clone(),
                expr: e,
                op: self.cur_op.clone(),
            };
            if plan.callees.get(&site) != Some(instance) {
                return Err(self.gap_here("concrete callee identity disagrees with its site plan"));
            }
            let signature = plan.callee_signatures.get(&site).ok_or_else(|| {
                self.gap_here("concrete callee signature is missing for its site")
            })?;
            return self.reject_concrete_column_container_opaque_slots(
                e,
                instance.func,
                signature,
            );
        }
        let callee = &self.ws.module(func.module).functions[func.func];
        if callee.inline_body.is_none() && !callee.is_extern {
            return Ok(());
        }
        let name = callee.name;
        let params = callee.param_decls.len();
        let ret = callee.return_ty;
        for i in 0..params {
            // `@repr(opaque)` is keyed to the DECLARED index, so it follows the
            // signature rather than the call's argument order. PER PARAMETER: a
            // waiver on one must not disarm the call, which is what
            // `vec_push(sink, @repr(opaque) item)` needs.
            if self.param_is_repr_opaque(func, i) {
                continue;
            }
            let Some(ty) = self.param_ty(func, i) else { continue };
            if let Some((rendered, physical)) = self.declared_slot_hazard(e, func, ty)? {
                return Err(CompilerError::new_with_label(
                    self.body.expr_spans[e],
                    format!(
                        "column-stored `{rendered}` cannot cross opaque body boundary \
                         `{name}` (an inline/extern body sees per-leaf columns, not \
                         {physical}; take `any` if the body reads the column layout \
                         itself, else use a boxed element type)"
                    ),
                ));
            }
        }
        // The constructor mirror: an opaque body whose RETURN is a container
        // builds that physical form BY HAND, so it can mint boxed storage for a
        // column-planned result.
        let Some(ret) = ret else { return Ok(()) };
        let Some((rendered, physical)) = self.declared_slot_hazard(e, func, ret)? else {
            return Ok(());
        };
        Err(CompilerError::new_with_label(
            self.body.expr_spans[e],
            format!(
                "opaque body boundary `{name}` cannot BUILD column-stored \
                 `{rendered}` (an inline/extern body hand-builds {physical}, which \
                 a column-planned result does not have; return `any` if the body \
                 builds the column layout itself, else use a boxed element type)"
            ),
        ))
    }

    /// The hazard a callee's DECLARED type carries at this call, or `None`.
    ///
    /// It converts the declared type to a CONCRETE one and asks the same
    /// storage oracles the routing asks (`vec_column_elem_mono`,
    /// `map_column_value_mono`, `opaque_container_hazard`). It never walks the
    /// argument list: arguments are in the CALLER's order, so pairing them with
    /// parameter indices is wrong the moment a call names its arguments, and a
    /// defaulted or spread slot has no argument expression to pair with at all.
    ///
    /// It goes through `MonoTy` rather than walking `TypeExpr`, because that is
    /// what carries NULLABILITY: a generic bound to `E?` substitutes to its
    /// PAYLOAD (a `Nullable` node needs an arena index), and an unmarked
    /// nullable element is a BOXED vec while its non-nullable twin is columns
    /// (`nullable_generic_binding_sroa`).
    fn declared_slot_hazard(
        &self,
        e: Idx<Expr>,
        func: FuncRef,
        ty: TyId,
    ) -> Result<Option<(String, &'static str)>, CompilerError> {
        let Some(mono) = self.declared_slot_mono(e, func, ty)? else { return Ok(None) };
        Ok(self.slot_hazard_mono(&mono))
    }

    fn slot_hazard_mono(
        &self,
        mono: &crate::monoty::MonoTy,
    ) -> Option<(String, &'static str)> {
        use crate::lower::shape::{map_column_value_mono, vec_column_elem_mono};
        if let Some(el) = vec_column_elem_mono(self.ws, self.scx.target, mono) {
            return Some((format!("[{}]", el.name), "`_data`"));
        }
        if let Some(el) = map_column_value_mono(self.ws, self.scx.target, mono) {
            return Some((format!("Map<_, {}>", el.name), "a bare key table"));
        }
        let crate::monoty::MonoTy::App {
            ctor: crate::monoty::MonoCtor::Nominal(c),
            params,
        } = mono
        else {
            return None;
        };
        if !crate::lower::shape::is_nominal_container(*c) {
            return None;
        }
        let [only] = params.as_slice() else { return None };
        let (payload, nullable) = only.peel_nullable();
        let Some(elem) = payload.nominal() else { return None };
        crate::lower::shape::opaque_container_hazard(
            self.ws,
            self.scx.target,
            *c,
            elem,
            nullable,
        )
        .map(|physical| (format!("{c}<{elem}>"), physical))
    }

    fn reject_concrete_column_container_opaque_slots(
        &self,
        e: Idx<Expr>,
        func: FuncRef,
        signature: &crate::concrete::ConcreteSignature,
    ) -> Result<(), CompilerError> {
        let callee = &self.ws.module(func.module).functions[func.func];
        if callee.inline_body.is_none() && !callee.is_extern {
            return Ok(());
        }
        if callee.param_decls.len() != signature.params.len() {
            return Err(self.gap_here("concrete callee signature has the wrong parameter count"));
        }
        for (i, id) in signature.params.iter().enumerate() {
            if self.param_is_repr_opaque(func, i) {
                continue;
            }
            let mono = self.scx.interner().get(*id);
            if let Some((rendered, physical)) = self.slot_hazard_mono(&mono) {
                return Err(CompilerError::new_with_label(
                    self.body.expr_spans[e],
                    format!(
                        "column-stored `{rendered}` cannot cross opaque body boundary \
                         `{}` (an inline/extern body sees per-leaf columns, not \
                         {physical}; take `any` if the body reads the column layout \
                         itself, else use a boxed element type)",
                        callee.name
                    ),
                ));
            }
        }
        let Some(id) = signature.result else { return Ok(()) };
        let mono = self.scx.interner().get(id);
        let Some((rendered, physical)) = self.slot_hazard_mono(&mono) else {
            return Ok(());
        };
        Err(CompilerError::new_with_label(
            self.body.expr_spans[e],
            format!(
                "opaque body boundary `{}` cannot BUILD column-stored \
                 `{rendered}` (an inline/extern body hand-builds {physical}, which \
                 a column-planned result does not have; return `any` if the body \
                 builds the column layout itself, else use a boxed element type)",
                callee.name
            ),
        ))
    }

    /// A callee-declared type as a CONCRETE type at this call site.
    ///
    /// Two substitutions compose. The callee's own generics bind from bone's
    /// `call_generic_bindings`; the types they bind TO belong to the caller and
    /// may name the CALLER's generics, which only the enclosing `SpecSubst`
    /// resolves. So each binding is converted under the enclosing substitution
    /// first, and the declared slot is then converted under the result.
    ///
    /// The substitution is deliberately PARTIAL. A generic the call never pins
    /// is skipped rather than abandoning the call, because it may have nothing
    /// to do with the slot being asked about -- `fn probe<T, U>(sink: [T])`
    /// determines `sink` whether or not `U` is ever bound, and abandoning the
    /// call let an unrelated variable disarm the rule.
    ///
    /// Two reasons are out of scope BY DECISION, matching `concrete_ty` arm for
    /// arm so the two cannot disagree: `UnboundGeneric` is the shared generic
    /// body Stage 2 owns, and `UnresolvedInfer` is the S 0h decided rejection
    /// the instance graph already records and Stage 3 raises. The rest --
    /// recovery poison, a `TyId` naming no node, a cyclic substitution chain,
    /// missing inference -- refuse the call with its span rather than picking a
    /// representation from a failure.
    fn declared_slot_mono(
        &self,
        e: Idx<Expr>,
        func: FuncRef,
        ty: TyId,
    ) -> Result<Option<crate::monoty::MonoTy>, CompilerError> {
        use crate::monoty::MonoTyReason as R;
        let f = &self.ws.module(func.module).functions[func.func];
        let refuse = |what: String| {
            CompilerError::new_with_label(
                self.body.expr_spans[e],
                format!(
                    "cannot decide the representation contract of opaque callee \
                     `{}`: {what}. The call is refused rather than admitted.",
                    f.name
                ),
            )
        };
        let inf = self
            .inf
            .ok_or_else(|| refuse("this body has no inference".into()))?;
        let mut subst = self.subst().clone();
        // Why a generic could NOT be converted, kept per name rather than acted
        // on here: a generic the slot does not mention is none of its business,
        // and rejecting in this loop would let an unrelated variable disarm the
        // rule. The reason is consulted only if the slot turns out to need it.
        let mut unconverted: Vec<(Ustr, &'static str)> = Vec::new();
        for (i, g) in f.generics.iter().enumerate() {
            let Some(TypeExpr::Generic(var, name)) = self.raw_ty_node(*g) else { continue };
            let Some(bound) = self.callee_binding_at(e, func, i)? else {
                unconverted.push((name, "no recorded binding"));
                continue;
            };
            match crate::monoty::convert(self.ws, inf, bound, &self.subst()) {
                Ok(m) => {
                    subst.insert(var, m);
                }
                // `UnresolvedInfer` is the S 0h DECIDED rejection (a type
                // argument no constraint reaches), already recorded by the
                // instance graph as `UnconstrainedTypeVar` and landing with
                // Stage 3. Raising it here too would reject it twice on two
                // schedules -- measured: `tests/vec_sort_test.rock`.
                // `concrete_ty` answers `None` for it for the same reason.
                Err(R::UnboundGeneric(_)) | Err(R::UnresolvedInfer(_)) => {}
                Err(r) => unconverted.push((name, r.bucket())),
            }
        }
        match crate::monoty::convert(self.ws, inf, ty, &subst) {
            Ok(m) => Ok(Some(m)),
            Err(R::UnboundGeneric(missing)) => {
                // The slot needs a generic that did not convert. If it failed for
                // a reason other than being unbound, that reason is the answer --
                // reporting "out of scope" here would hide unfinished inference
                // behind the Stage-2 exemption.
                match unconverted.iter().find(|(n, _)| *n == missing) {
                    Some((_, why)) => Err(refuse(format!(
                        "the slot needs generic `{missing}`, which resolves to {why}"
                    ))),
                    // Genuinely still generic: the shared generic body, which
                    // Stage 2 owns and this rule does not answer for.
                    None => Ok(None),
                }
            }
            Err(R::UnresolvedInfer(_)) => Ok(None),
            Err(r) => Err(refuse(format!("the slot resolves to {}", r.bucket()))),
        }
    }

    /// What this call binds a CALLEE generic to. Fails CLOSED: a generic whose
    /// binding cannot be resolved is a slot whose representation is unknown,
    /// and admitting it is how the silent wrong answer gets back in.
    fn callee_binding_at(
        &self,
        e: Idx<Expr>,
        func: FuncRef,
        at: usize,
    ) -> Result<Option<TyId>, CompilerError> {
        let f = &self.ws.module(func.module).functions[func.func];
        let refuse = |what: String| {
            CompilerError::new_with_label(
                self.body.expr_spans[e],
                format!(
                    "internal: cannot resolve a generic of opaque callee `{}`: {what}. \
                     Its representation contract cannot be checked, so the call is \
                     refused rather than admitted.",
                    f.name
                ),
            )
        };
        let inf = self.inf.ok_or_else(|| refuse("this body has no inference".into()))?;
        let Some(bindings) = inf.call_generic_bindings.get(&e) else {
            // Not every opaque callee is generic; only a generic one owes a
            // binding list. A non-generic callee reaching here has no `var` to
            // resolve in the first place.
            return if f.generics.is_empty() {
                Ok(None)
            } else {
                Err(refuse("inference recorded no generic bindings for this call".into()))
            };
        };
        if bindings.len() != f.generics.len() {
            return Err(refuse(format!(
                "the call supplied {} binding(s) for {} declared generic(s)",
                bindings.len(),
                f.generics.len()
            )));
        }
        Ok(bindings.get(at).copied())
    }

    /// Whether parameter `i` of `func` carries the `@repr(opaque)` contract.
    ///
    /// Read off `ParamDecl::annotations`, the declaration -- never off the
    /// call. The registry (`bone_core::annotations`) has already checked the
    /// kind and that the enclosing fn is an opaque boundary, so reaching here
    /// with the name present means the contract is valid.
    fn param_is_repr_opaque(&self, func: FuncRef, i: usize) -> bool {
        self.ws.module(func.module).functions[func.func]
            .param_decls
            .get(i)
            .is_some_and(|pd| pd.annotations.iter().any(|a| a.name.name == "repr"))
    }


    /// A fn taken BY VALUE must have the ABI an indirect caller can derive.
    ///
    /// A definition reads its [`BackendFnAbiPlan`]; an indirect call site has only
    /// the fn TYPE, so it reads [`Self::fn_value_abi`]. Those agree by construction
    /// for an ordinary body -- the plan narrows an address-taken one to exactly
    /// the `nobox` shapes the type yields. `@scalar_abi` is the one thing that
    /// bypasses that narrowing, because its hand-written `inline(lua)` body
    /// literally spells the leaf params, so narrowing it would break the body
    /// instead of the call.
    ///
    /// When those two disagree the program cannot be emitted correctly either way:
    /// the definition took leaf params and the call site packed one cdata, so the
    /// first leaf got the whole box and the rest went nil. Rejecting is the honest
    /// answer, and it is what the fn-value ABI comment always claimed held.
    pub(crate) fn reject_fn_value_abi_mismatch(
        &self,
        e: Idx<Expr>,
        func: FuncRef,
    ) -> Result<(), CompilerError> {
        let callee = &self.ws.module(func.module).functions[func.func];
        let arity = callee.param_decls.len();
        let boxed = || (vec![RetSlot::Opaque; arity], None);
        let plan = BackendFnAbiPlan::for_declaration(self.ws, self.scx, func);
        let (def_params, def_ret) = match plan.scalar() {
            Some(abi) => (abi.param_shapes.clone(), abi.ret_shape.clone()),
            None => boxed(),
        };
        let (val_params, val_ret) = self.expr_fn_value_abi(e).unwrap_or_else(boxed);
        if def_params == val_params && def_ret == val_ret {
            return Ok(());
        }
        Err(CompilerError::new_with_label(
            self.body.expr_spans[e],
            format!(
                "`{}` cannot be taken as a fn value: its `@scalar_abi` definition \
                 takes leaf params, but an indirect call derives the ABI from the \
                 fn TYPE alone, which boxes them -- so the first leaf would get \
                 the whole box and the rest nil (mark the value type `nobox` so \
                 both ends agree, or call it directly)",
                callee.name,
            ),
        ))
    }

    /// The return-position companion of [`Self::reject_nobox_boxed_call_args`]:
    /// a `nobox` element only rides a tuple return as leaves if skull emits the
    /// callee's body. An `extern`, or an `inline(lua)` body that has not opted
    /// in with `@scalar_abi`, writes its own `return` -- so the call site would
    /// read leaves the body never produced. That is a boxed boundary; reject it
    /// where an argument would be rejected.
    pub(crate) fn reject_nobox_opaque_tuple_return(
        &self,
        e: Idx<Expr>,
        func: FuncRef,
        shape: &TupleRetShape,
    ) -> Result<(), CompilerError> {
        if self.scx.values_first_class || !shape.splats() {
            return Ok(());
        }
        let callee = &self.ws.module(func.module).functions[func.func];
        let opaque = callee.is_extern
            || (callee.inline_body.is_some()
                && !callee.annotations.iter().any(|a| a.name.name.as_str() == "scalar_abi"));
        if !opaque {
            return Ok(());
        }
        Err(CompilerError::new_with_label(
            self.body.expr_spans[e],
            format!(
                "nobox value cannot cross the boxed tuple return of `{}` \
                 (an `inline(lua)` body must opt in with `@scalar_abi` and return the leaves)",
                callee.name
            ),
        ))
    }

    /// The BARE-return companion of [`Self::reject_nobox_opaque_tuple_return`].
    /// That one covers `-> (bool, NV2)`; a plain `-> NV2` had no check in any
    /// position, so an opaque body handing back a hand-built box compiled clean
    /// AND PASSED -- Lua's `.field` does not distinguish a table from cdata, so
    /// the guarantee was violated with no diagnostic. Live shape:
    /// `stone_measure_text_vec2_for` and its `inline(js)` twin.
    ///
    /// Keyed on the SIGNATURE, which is transport-independent -- the JS backend
    /// packs leaves into an array rather than multi-returning them, and this
    /// check is correct for both.
    pub(crate) fn reject_nobox_opaque_value_return(
        &self,
        e: Idx<Expr>,
        func: FuncRef,
    ) -> Result<(), CompilerError> {
        if self.scx.values_first_class {
            return Ok(());
        }
        let callee = &self.ws.module(func.module).functions[func.func];
        // Same authorization as the arg direction: a `@gpu_only` intrinsic's
        // boxed return is not a real boundary (on the GPU it is a native vector,
        // and the Lua body only `error(...)`s).
        if crate::intrinsics::is_gpu_only(callee) {
            return Ok(());
        }
        let opaque = callee.is_extern
            || (callee.inline_body.is_some()
                && !callee.annotations.iter().any(|a| a.name.name.as_str() == "scalar_abi"));
        if !opaque {
            return Ok(());
        }
        let Some(ret) = callee.return_ty else {
            return Ok(());
        };
        let Some(TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            ..
        }) = static_ty(self.ws, ret)
        else {
            return Ok(());
        };
        let Some(sid) = self.ws.struct_by_name.get(&name) else {
            return Ok(());
        };
        if !self.ws.modules[sid.module.index()].structs[sid.def].is_nobox {
            return Ok(());
        }
        Err(CompilerError::new_with_label(
            self.body.expr_spans[e],
            format!(
                "nobox value `{name}` cannot cross the boxed return of `{}` \
                 (an `inline(lua)` body must opt in with `@scalar_abi` and return the leaves)",
                callee.name
            ),
        ))
    }

    pub(crate) fn is_explicit_materialize_call(&self, expr: Idx<Expr>) -> bool {
        let Expr::Call {
            callee: Callee::Unresolved { name, .. },
            ..
        } = self.body.expr_arena[expr]
        else {
            return false;
        };
        self.resolve_funcref(expr, name)
            .ok()
            .is_some_and(|func| matches!(self.fn_expand_kind(func), Some(ExpandKind::Materialize)))
    }

    /// A (value) struct field's declared type, for the value-struct-literal
    /// construction sink (`Vec2 { x: 0 }` -> the `x` field coerces int->float).
    pub(crate) fn struct_field_ty(&self, name: Ustr, field: Ustr) -> Option<TyId> {
        let sid = *self.ws.struct_by_name.get(&name)?;
        let sd = &self.ws.modules[sid.module.index()].structs[sid.def];
        sd.fields.iter().find(|f| f.name == field).map(|f| f.ty)
    }

    /// Coerce a value-struct field's repr int->float at CONSTRUCTION (before any
    /// scalar-ABI splat, per the plan): only a `Scalar` leaf can be an int; a
    /// `Group` / nested-value repr is already composite.
    pub(crate) fn coerce_field_repr(&mut self, r: ValueRepr, value_expr: Idx<Expr>, struct_name: Ustr, field: Ustr) -> ValueRepr {
        match r {
            ValueRepr::Scalar(s) => match self.struct_field_ty(struct_name, field) {
                Some(fty) => ValueRepr::Scalar(self.coerced(s, value_expr, fty)),
                None => ValueRepr::Scalar(s),
            },
            other => other,
        }
    }


    /// The scalar class of a resolved type, seeing through a TRANSPARENT newtype
    /// to its wrapped type (a `newtype Score(int)` is the raw int at runtime, so
    /// `<`/`+`/... on it are native int ops — matching its identity lowering).
    pub(crate) fn arith_ty_of(&self, te: TypeExpr) -> Option<ArithTy> {
        match te {
            TypeExpr::Builtin(BuiltinTy::Int) => Some(ArithTy::Int),
            TypeExpr::Builtin(BuiltinTy::Float) => Some(ArithTy::Float),
            TypeExpr::Builtin(BuiltinTy::String) => Some(ArithTy::Str),
            TypeExpr::App { ctor: TypeCtor::Nominal(name), .. }
                if self.ws.newtypes_by_name.contains_key(&name) =>
            {
                let nid = self.ws.newtypes_by_name.get(&name)?;
                let wrapped = self.ws.modules[nid.module.index()].newtypes[nid.def].wrapped;
                self.arith_ty_of(self.resolve_ty(wrapped)?)
            }
            _ => None,
        }
    }

    pub(crate) fn nobox_value_name(&self, e: Idx<Expr>) -> Option<Ustr> {
        self.dispatch().nobox_value_name(e)
    }

    /// Under `values_first_class` NOTHING is nobox: the flag says the target has no
    /// boxed representation to avoid, so a `value` struct is already a first-class
    /// `vecN` and there is no leaf form to prefer. Answering `true` here would put
    /// a local into leaf cells named `<name>__<i>`, which is both a duplicated
    /// initializer and a RESERVED GLSL identifier (spec 3.7).
    ///
    /// The gate lives on the predicate rather than on each shape helper because
    /// every representation decision reaches nobox-ness through here -- binding
    /// shapes, expression shapes, signature splat gates and the column-storage
    /// rejections alike. Gating one consumer leaves the others to remember it, and
    /// they did not: `Statement::Let` falls through `nobox_binding_shape` to
    /// `nobox_value_shape_of`, so gating the binding shape alone still emitted
    /// `shift__0`.
    pub(crate) fn is_nobox_struct(&self, name: Ustr) -> bool {
        self.dispatch().is_nobox_struct(name)
    }

    pub(crate) fn nobox_value_shape_of(&self, e: Idx<Expr>) -> Option<ValueShape> {
        let mut shape = value_shape_of_nominal(self.ws, self.nobox_value_name(e)?)?;
        shape.nullable = self.expr_is_nullable(e);
        Some(shape)
    }

    pub(crate) fn nobox_binding_shape(&self, binding: Idx<BindingInfo>) -> Option<ValueShape> {
        self.nobox_shape_of_ty(self.binding_ty(binding)?)
    }

    /// The leaf shape of a `nobox`-typed `ty` (nullable included: presence is
    /// leaf 0), `None` for every other type.
    /// Argument positions of a `ShaderPipeline` expansion, where a bare fn name is
    /// consumed by the compiler and never becomes a runtime fn value:
    /// `rock_shader(frag)` lowers its argument to the entry's NAME as a string
    /// literal, so no function-value ABI applies there.
    pub(crate) fn expansion_arg_exprs(&self) -> HashSet<Idx<Expr>> {
        let mut out = HashSet::new();
        for (e, expr) in self.body.expr_arena.iter() {
            let Expr::Call { callee, args } = expr else { continue };
            let Callee::Unresolved { name, .. } = callee else { continue };
            let Ok(func) = self.resolve_funcref(e, *name) else { continue };
            // ONLY `ShaderPipeline`. It resolves its argument to a fn NAME and emits a
            // string literal, so the name never becomes a runtime value. The sibling
            // kinds evaluate theirs -- `expand_materialize` calls `lower_expr_repr` on
            // it -- so exempting them let `materialize(<nobox-carrying fn>)` bypass the
            // ABI rejection entirely, which is a hole, not a narrowing.
            if matches!(self.fn_expand_kind(func), Some(ExpandKind::ShaderPipeline)) {
                out.extend(args.clone().into_iter());
            }
        }
        out
    }

    pub(crate) fn reject_incompatible_nobox_fn_value_abi(&self) -> Result<(), CompilerError> {
        if self.scx.values_first_class {
            return Ok(());
        }
        let expansion_args = self.expansion_arg_exprs();
        for (e, expr) in self.body.expr_arena.iter() {
            let Expr::Global(name) = expr else { continue };
            if expansion_args.contains(&e) {
                continue;
            }
            let Some(overloads) = self.ws.functions_by_name.get(name) else { continue };
            let incompatible = overloads.iter().any(|(mid, fid)| {
                let fref = FuncRef { module: *mid, func: *fid };
                let expected =
                    super::abi::fn_value_nobox_abi(self.ws, fref, &SpecSubst::default());
                let emitted =
                    BackendFnAbiPlan::for_declaration(self.ws, self.scx, fref).scalar().cloned();
                expected.is_some() && emitted != expected
            });
            if incompatible {
                return Err(CompilerError::new_with_label(
                    self.body.expr_spans[e],
                    format!(
                        "`{name}` cannot be used as a fn value because its fixed ABI does not \
                         match the type-derived `nobox` fn-value ABI"
                    ),
                ));
            }
        }
        Ok(())
    }

    /// The leaf ABI of a FN TYPE, or `None` when nothing in the signature is a
    /// `nobox` value and the boxed fn-value ABI stands unchanged.
    ///
    /// Keyed on the TYPE and nothing else. An indirect call site cannot see its
    /// callee, so the definition, a direct call and a call through a fn value
    /// can only agree by all three reading the same signature -- the rule the
    /// tuple-return ABI already follows (`lambda_ret_tuple_shape` next to
    /// `expr_tuple_shape`). Any callee-identity test here would make the two
    /// ends disagree exactly where the callee is opaque, which is the case that
    /// matters.
    ///
    /// Narrowed to `nobox` types on purpose: an ordinary value struct has no
    /// guarantee to keep, so widening this to every FFI-clean value would
    /// re-ABI every lambda in the tree for no correctness gain. Both ends
    /// compute the same predicate from the same type, so they still agree.
    pub(crate) fn fn_value_abi(&self, ty: TyId) -> Option<(Vec<RetSlot>, Option<ValueShape>)> {
        if self.concrete_plan.is_some() {
            let ty = self.concrete_ty_id(ty)?;
            return self.fn_value_abi_mono(&ty);
        }
        let ty = self.resolve_tyid(ty)?;
        // Nullability lives on the REFERENCE, not on the call ABI: an absent fn
        // value is never called, and a present one is a callable with exactly the
        // leaf ABI its type spells. A LAMBDA LITERAL at a nullable param unifies
        // its own expr type with that param, so it arrives here as
        // `Nullable(Function(..))` while the same lambda bound to a local first
        // arrives as `Function(..)`. Without this peel the two spellings get
        // different ABIs and the literal packs its `nobox` return.
        let ty = match self.ty_node(ty)? {
            TypeExpr::Nullable(inner) => self.resolve_tyid(ty.with_idx(inner))?,
            _ => ty,
        };
        let TypeExpr::Function { params, ret } = self.ty_node(ty)? else { return None };
        let ps: Vec<RetSlot> =
            params.into_iter().map(|p| self.nobox_param_slot_of_ty(ty.with_idx(p))).collect();
        let rs = self.nobox_shape_of_ty(ty.with_idx(ret));
        if rs.is_none() && !ps.iter().any(RetSlot::splats) {
            return None;
        }
        Some((ps, rs))
    }

    fn fn_value_abi_mono(
        &self,
        ty: &crate::monoty::MonoTy,
    ) -> Option<(Vec<RetSlot>, Option<ValueShape>)> {
        let (ty, _) = ty.peel_nullable();
        let crate::monoty::MonoTy::Function { params, ret } = ty else { return None };
        let ps = params
            .iter()
            .map(|ty| {
                mono_nobox_value_shape(self.ws, ty)
                    .map(RetSlot::Leaves)
                    .or_else(|| {
                        mono_tuple_ret_shape(self.ws, ty)
                            .filter(TupleRetShape::splats)
                            .map(RetSlot::Nested)
                    })
                    .unwrap_or(RetSlot::Opaque)
            })
            .collect::<Vec<_>>();
        let rs = mono_nobox_value_shape(self.ws, ret);
        (rs.is_some() || ps.iter().any(RetSlot::splats)).then_some((ps, rs))
    }

    /// [`Self::nobox_shape_of_ty`] widened to the param SLOT vocabulary, so a
    /// TUPLE param carrying a `nobox` element rides its leaves like any other
    /// splatting slot. Both ends of a fn value read this same predicate off the
    /// same type, so they agree without either seeing the other.
    pub(crate) fn nobox_param_slot_of_ty(&self, ty: TyId) -> RetSlot {
        if let Some(shape) = self.nobox_shape_of_ty(ty) {
            return RetSlot::Leaves(shape);
        }
        self.tuple_param_slot_of_ty(ty)
    }

    /// [`Self::fn_value_abi`] for the fn VALUE an expression denotes.
    pub(crate) fn expr_fn_value_abi(
        &self,
        e: Idx<Expr>,
    ) -> Option<(Vec<RetSlot>, Option<ValueShape>)> {
        self.fn_value_abi(*self.inf?.expr_types.get(e)?)
    }

    pub(crate) fn nobox_shape_of_ty(&self, ty: TyId) -> Option<ValueShape> {
        if let Some(concrete) = self.concrete_ty_id(ty) {
            return mono_nobox_value_shape(self.ws, &concrete);
        }
        if self.concrete_plan.is_some() {
            return None;
        }
        // The BODY arena's spelling of [`nullable_nominal_shape`]: same rule, its
        // own type-node lookup. `is_nobox_struct` (not `is_nobox_nominal`) is the
        // one real difference -- it also honours `scx.values_first_class`, which
        // the GLSL backend sets -- so the guard stays here and the core sees an
        // already-filtered nominal.
        if !self.scx.values_first_class {
            let mut shape = nullable_nominal_shape(self.ws, ty, &|t| {
                self.ty_node(self.resolve_tyid(t)?)
            })?;
            // A generic bound to `E?` reaches the core as its payload nominal, so
            // the `?` has to be re-applied from the binding. Idempotent for a
            // spelled-out `E?`, which the core already marked.
            shape.nullable |= self.is_nullable_ty(ty);
            return Some(shape);
        }
        None
    }

    /// The leaf globals a place rooted at a `nobox` global denotes, plus the
    /// shape of the projected part (`None` = one primitive leaf). `SPOT` is the
    /// whole group, `SPOT.x` / `SPOT.a.b` a contiguous sub-range of it -- the
    /// leaf order is `repr_to_leaves`', so a projection is a slice, never a
    /// scatter. `None` when the place is not rooted at a `nobox` global.
    pub(crate) fn nobox_global_place(&self, e: Idx<Expr>) -> Option<(Vec<Ustr>, Option<ValueShape>)> {
        match self.body.expr_arena[e].clone() {
            // The DECLARED type, never `e`'s: a refined read of a `global X: NV2?`
            // has expr type `NV2`, whose 2-leaf shape would address the presence
            // leaf as `x`. Storage layout is a property of the global, not of the
            // read site.
            Expr::Global(name) => {
                let shape = nobox_declared_shape(self.ws, *self.ws.globals_by_name.get(&name)?)?;
                let leaves =
                    (0..shape.leaf_count()).map(|i| nobox_global_leaf_name(name, i)).collect();
                Some((leaves, Some(shape)))
            }
            Expr::FieldAccess { base, field } => {
                let (leaves, Some(shape)) = self.nobox_global_place(base)? else {
                    return None;
                };
                let mut off = usize::from(shape.nullable);
                for (fname, fs) in &shape.fields {
                    let (n, sub) = match fs {
                        FieldShape::Prim => (1, None),
                        FieldShape::Nested(s) => (s.leaf_count(), Some(s.clone())),
                    };
                    if *fname == field {
                        return Some((leaves[off..off + n].to_vec(), sub));
                    }
                    off += n;
                }
                None
            }
            _ => None,
        }
    }

    /// Read a `nobox` global place back as an unboxed repr.
    pub(crate) fn nobox_global_load(
        &mut self,
        leaves: &[Ustr],
        shape: Option<ValueShape>,
    ) -> ValueRepr {
        let vals: Vec<Idx<Inst>> =
            leaves.iter().map(|g| self.emit_cur(Inst::Global(*g))).collect();
        match shape {
            Some(shape) => {
                let mut idx = 0;
                self.rebuild_from_leaves(&shape, &vals, &mut idx)
            }
            None => ValueRepr::Scalar(vals[0]),
        }
    }

    /// Write `value` into a `nobox` global place, leaf by leaf.
    pub(crate) fn nobox_global_store(
        &mut self,
        leaves: &[Ustr],
        shape: Option<ValueShape>,
        value: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        let vals = match shape {
            Some(shape) => {
                let r = self.lower_expr_repr(value)?;
                self.repr_to_leaves(&r, &shape)
            }
            None => vec![self.lower_expr(value)?],
        };
        for (g, v) in leaves.iter().zip(vals) {
            self.emit_cur(Inst::StoreGlobal { name: *g, value: v });
        }
        Ok(())
    }

    pub(crate) fn emit_all_equal(&mut self, lhs: &[Idx<Inst>], rhs: &[Idx<Inst>]) -> Idx<Inst> {
        debug_assert_eq!(lhs.len(), rhs.len());
        let result = self.f.new_cell(None);
        let fail = self.f.new_block();
        let join = self.f.new_block();
        for (&a, &b) in lhs.iter().zip(rhs) {
            let equal = self.emit_cur(Inst::Eq(a, b));
            let next = self.f.new_block();
            self.f.set_term(
                self.cur,
                Term::Branch {
                    cond: equal,
                    then_bb: next,
                    else_bb: fail,
                },
            );
            self.cur = next;
        }
        let yes = self.emit_cur(Inst::Const(Lit::Bool(true)));
        self.emit_cur(Inst::Store {
            cell: result,
            value: yes,
        });
        self.f.set_term(self.cur, Term::Goto(join));

        self.cur = fail;
        let no = self.emit_cur(Inst::Const(Lit::Bool(false)));
        self.emit_cur(Inst::Store {
            cell: result,
            value: no,
        });
        self.f.set_term(self.cur, Term::Goto(join));

        self.cur = join;
        self.emit_cur(Inst::Load(result))
    }
}
