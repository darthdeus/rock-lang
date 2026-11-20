use super::*;
use crate::monoty::MonoTy;

impl<'a> Builder<'a> {
    pub(crate) fn mark_record_base(&mut self, source: Idx<Expr>, value: Idx<Inst>) {
        // Shader lowering uses native first-class structs, with no host objects.
        if self.scx.target == bone_core::hir::FnBackend::Any {
            return;
        }
        let may_callback = if let Some(mut ty) = self.concrete_expr_ty(source) {
            loop {
                match ty {
                    MonoTy::Nullable(inner) => ty = inner,
                    MonoTy::App {
                        ctor: crate::monoty::MonoCtor::Ref,
                        params,
                    } if params.len() == 1 => ty = &params[0],
                    _ => break,
                }
            }
            match ty {
                MonoTy::App {
                    ctor: crate::monoty::MonoCtor::Nominal(name),
                    ..
                } => self
                    .ws
                    .nominals
                    .struct_id(*name)
                    .is_some_and(|sid| !self.ws.module(sid.module).structs[sid.def].is_value),
                MonoTy::Any => true,
                _ => false,
            }
        } else {
            match self.field_base_ty(source) {
                Some(TypeExpr::App {
                    ctor: TypeCtor::Nominal(name),
                    ..
                }) => self
                    .ws
                    .nominals
                    .struct_id(name)
                    .is_some_and(|sid| !self.ws.module(sid.module).structs[sid.def].is_value),
                None
                | Some(
                    TypeExpr::Generic(..) | TypeExpr::Infer(..) | TypeExpr::Builtin(BuiltinTy::Any),
                ) => true,
                _ => false,
            }
        };
        if may_callback {
            self.callback_records.insert(value);
        }
    }

    pub(crate) fn container_plan_for_type(
        &self,
        ty: &MonoTy,
        family: ContainerFamily,
    ) -> Result<ContainerPlan, CompilerError> {
        ContainerPlan::for_type(self.ws, self.scx.target, self.scx.rep_arena(), family, ty)
            .map_err(|error| self.rep_error(error))
    }

    pub(crate) fn container_plan_for(
        &self,
        e: Idx<Expr>,
        family: ContainerFamily,
    ) -> Result<Option<ContainerPlan>, CompilerError> {
        let Some(ty) = self.concrete_ty(e)? else {
            return Ok(None);
        };
        if !ContainerPlan::matches_type(family, ty) {
            return Ok(None);
        }
        self.container_plan_for_type(ty, family).map(Some)
    }

    pub(crate) fn expr_is_flat_vec(&self, e: Idx<Expr>) -> bool {
        self.concrete_expr_ty(e).is_some_and(|ty| {
            matches!(
                ContainerFamily::of(ty.peel_nullable().0),
                Some((ContainerFamily::FlatVec, _))
            )
        })
    }

    pub(crate) fn nb_value_elem(&self, e: Idx<Expr>) -> Result<Option<Ustr>, CompilerError> {
        let Some(concrete) = self.concrete_expr_ty(e) else {
            return Ok(None);
        };
        let family = match ContainerFamily::of(concrete.peel_nullable().0) {
            Some((family @ (ContainerFamily::NativeBuffer | ContainerFamily::FlatVec), _)) => {
                family
            }
            _ => return Ok(None),
        };
        let arena = self.scx.rep_arena();
        let plan = self.container_plan_for_type(concrete, family)?;
        if !plan.is_leaf_addressed() {
            return Ok(None);
        }
        Ok(plan.ffi_element_name(arena))
    }

    pub(crate) fn nb_type_value_elem(&self, e: Idx<Expr>) -> Result<Option<Ustr>, CompilerError> {
        let Some(concrete) = self.concrete_expr_ty(e) else {
            return Ok(None);
        };
        let element = match concrete.peel_nullable().0 {
            MonoTy::App {
                ctor: crate::monoty::MonoCtor::Nominal(name),
                params,
            } if matches!(name.as_str(), "NativeBuffer" | "FlatVec") => params.first(),
            _ => return Ok(None),
        };
        let Some(element) = element else {
            return Ok(None);
        };
        let (payload, nullable) = element.peel_nullable();
        if nullable {
            return Ok(None);
        }
        let crate::monoty::MonoTy::App {
            ctor: crate::monoty::MonoCtor::Nominal(name),
            ..
        } = payload
        else {
            return Ok(None);
        };
        let Some(sid) = self.ws.nominals.struct_id(*name) else {
            return Ok(None);
        };
        if !has_value_plan(&self.ws.modules[sid.module.index()].structs[sid.def]) {
            return Ok(None);
        }
        let arena = self.scx.rep_arena();
        let rep = arena
            .plan_container_element(self.ws, self.scx.target, payload)
            .map_err(|error| self.rep_error(error))?;
        Ok(arena.value_shape(rep).map(|shape| shape.name))
    }

    /// Whether `e` is a Vec, Map or Arena whose elements have no per-element table.
    pub(crate) fn flat_stored_container(&self, e: Idx<Expr>) -> Result<bool, CompilerError> {
        match self.concrete_ty(e)? {
            Some(ty) => self.flat_stored_container_type(ty),
            None => Ok(false),
        }
    }

    pub(crate) fn flat_stored_container_type(&self, ty: &MonoTy) -> Result<bool, CompilerError> {
        let Some((
            family @ (ContainerFamily::Vec | ContainerFamily::Map | ContainerFamily::Arena),
            _,
        )) = ContainerFamily::of(ty.peel_nullable().0)
        else {
            return Ok(false);
        };
        let plan = self.container_plan_for_type(ty, family)?;
        Ok(plan.flat_elem(self.scx.rep_arena()).is_some())
    }

    pub(crate) fn arena_column_elem_of(
        &self,
        e: Idx<Expr>,
    ) -> Result<Option<ColumnElem>, CompilerError> {
        Ok(self
            .container_plan_for(e, ContainerFamily::Arena)?
            .and_then(|plan| plan.column_elem(self.scx.rep_arena())))
    }

    pub(crate) fn vec_kind(&self, e: Idx<Expr>) -> Result<ContainerKind, CompilerError> {
        if let Some((elem, storage)) = self.vec_flat_elem_of(e)? {
            return Ok(storage.vec_kind(elem));
        }
        let elem = match self.concrete_expr_ty(e) {
            Some(MonoTy::App {
                ctor: crate::monoty::MonoCtor::Array,
                params,
            }) => params.first(),
            _ => None,
        };
        Ok(self.boxed_vec_kind(elem))
    }

    /// On Lua an element that is never nil fills every live slot, so an in-bounds
    /// access cannot reach a metamethod of a foreign backing table.
    pub(crate) fn boxed_vec_kind(&self, elem: Option<&MonoTy>) -> ContainerKind {
        if matches!(self.scx.target, bone_core::hir::FnBackend::Lua)
            && elem.is_some_and(|ty| crate::nilness::monoty_never_nil(self.ws, ty))
        {
            ContainerKind::VecNonNull
        } else {
            ContainerKind::Vec
        }
    }

    pub(crate) fn vec_flat_elem_of(
        &self,
        e: Idx<Expr>,
    ) -> Result<Option<(ColumnElem, FlatStorage)>, CompilerError> {
        Ok(self
            .container_plan_for(e, ContainerFamily::Vec)?
            .and_then(|plan| plan.flat_elem(self.scx.rep_arena())))
    }

    pub(crate) fn vec_flat_elem_for_type(
        &self,
        ty: &MonoTy,
    ) -> Result<Option<(ColumnElem, FlatStorage)>, CompilerError> {
        if !ContainerPlan::matches_type(ContainerFamily::Vec, ty) {
            return Ok(None);
        }
        self.container_plan_for_type(ty, ContainerFamily::Vec)
            .map(|plan| plan.flat_elem(self.scx.rep_arena()))
    }

    /// Nullable elements add a presence column.
    pub(crate) fn column_shape(&self, elem: ColumnElem) -> Result<ValueShape, CompilerError> {
        let shape = self
            .scx
            .rep_arena()
            .value_shape(elem.representation())
            .cloned()
            .ok_or_else(|| gap("column element is not a value struct"))?;
        Ok(shape)
    }

    pub(crate) fn map_column_value_of(
        &self,
        e: Idx<Expr>,
    ) -> Result<Option<ColumnElem>, CompilerError> {
        Ok(self
            .container_plan_for(e, ContainerFamily::Map)?
            .and_then(|plan| plan.column_elem(self.scx.rep_arena())))
    }

    pub(crate) fn concrete_ty(&self, e: Idx<Expr>) -> Result<Option<&'a MonoTy>, CompilerError> {
        if self.concrete_expr_is_untyped(e) {
            return Ok(None);
        }
        self.concrete_expr_ty(e)
            .map(Some)
            .ok_or_else(|| self.gap_here("concrete plan has no type for this expression"))
    }

    /// A column-stored Map's values live in the column Vec `m.data`.
    pub(crate) fn map_col_data(&mut self, m: Idx<Inst>) -> Idx<Inst> {
        self.emit_cur(Inst::Field {
            nonthrowing: false,
            base: m,
            field: crate::names::ustr("data"),
        })
    }

    /// Value-struct elements carry their name so the JS emitter can lower flat slots.
    pub(crate) fn nb_kind(&self, e: Idx<Expr>) -> Result<ContainerKind, CompilerError> {
        match self.nb_value_elem(e)? {
            Some(elem) => Ok(ContainerKind::NativeBufferValue(elem)),
            None => Ok(ContainerKind::NativeBuffer),
        }
    }

    /// FlatVec indexing addresses the backing NativeBuffer.
    pub(crate) fn lower_index_base(
        &mut self,
        base: Idx<Expr>,
    ) -> Result<(Idx<Inst>, ContainerKind), CompilerError> {
        if self.expr_is_flat_vec(base) {
            let kind = self.nb_kind(base)?;
            let b = self.lower_expr(base)?;
            let buf = self.emit_cur(Inst::Field {
                nonthrowing: false,
                base: b,
                field: crate::names::ustr("buf"),
            });
            return Ok((buf, kind));
        }
        let kind = self.coll_kind(base)?;
        let b = self.lower_expr(base)?;
        Ok((b, kind))
    }

    pub(crate) fn coll_kind(&self, base: Idx<Expr>) -> Result<ContainerKind, CompilerError> {
        if let Some(ty) = self.concrete_expr_ty(base) {
            let (payload, _) = ty.peel_nullable();
            return match payload {
                MonoTy::App {
                    ctor: crate::monoty::MonoCtor::Array,
                    ..
                } => self.vec_kind(base),
                MonoTy::App {
                    ctor: crate::monoty::MonoCtor::Map,
                    ..
                } => Ok(ContainerKind::Map),
                MonoTy::App {
                    ctor: crate::monoty::MonoCtor::Nominal(name),
                    ..
                } if is_native_buffer(*name) => Ok(self.nb_kind(base)?),
                _ => Err(self.gap_here("concrete type is not an indexable container")),
            };
        }
        if self.concrete_expr_is_untyped(base)
            && matches!(self.body.expr_arena[base], Expr::Array(..))
        {
            return Ok(ContainerKind::Vec);
        }
        Err(self.gap_here("container expression is missing from the concrete type plan"))
    }

    pub(crate) fn arith_ty(&self, e: Idx<Expr>) -> Option<ArithTy> {
        if let Some(ty) = self.concrete_expr_ty(e) {
            return self.arith_ty_of_mono(ty);
        }
        self.arith_ty_of(self.expr_ty(e)?)
    }

    pub(crate) fn arith_ty_of_mono(&self, ty: &MonoTy) -> Option<ArithTy> {
        let (payload, _) = ty.peel_nullable();
        match payload {
            MonoTy::Int => Some(ArithTy::Int),
            MonoTy::Uint => Some(ArithTy::Uint),
            MonoTy::Float => Some(ArithTy::Float),
            MonoTy::String => Some(ArithTy::Str),
            MonoTy::App {
                ctor: crate::monoty::MonoCtor::Nominal(name),
                ..
            } if self.ws.nominals.newtype_id(*name).is_some() => {
                let nid = self.ws.nominals.newtype_id(*name)?;
                let wrapped = self.ws.modules[nid.module.index()].newtypes[nid.def].wrapped;
                self.arith_ty_of(self.resolve_ty(wrapped)?)
            }
            _ => None,
        }
    }

    /// Coerce typed integer reads at expected-float boundaries.
    pub(crate) fn coerced(
        &mut self,
        value: Idx<Inst>,
        value_expr: Idx<Expr>,
        expected: TyId,
    ) -> Idx<Inst> {
        let to_float = self
            .concrete_ty_id(expected)
            .as_ref()
            .and_then(|t| self.arith_ty_of_mono(t))
            == Some(ArithTy::Float);
        self.coerce_numeric_to_float(
            value,
            self.arith_ty(value_expr).unwrap_or(ArithTy::Str),
            to_float,
        )
    }

    pub(crate) fn coerced_mono(
        &mut self,
        value: Idx<Inst>,
        value_expr: Idx<Expr>,
        expected: &MonoTy,
    ) -> Idx<Inst> {
        self.coerce_numeric_to_float(
            value,
            self.arith_ty(value_expr).unwrap_or(ArithTy::Str),
            self.arith_ty_of_mono(expected) == Some(ArithTy::Float),
        )
    }

    pub(crate) fn binding_ty(&self, name: Idx<BindingInfo>) -> Option<TyId> {
        self.inf
            .and_then(|i| i.binding_types.get(name).copied())
            .or(self.body.binding_arena[name].declared_ty)
    }

    pub(crate) fn binding_is_ref(&self, name: Idx<BindingInfo>) -> bool {
        self.binding_ty(name)
            .and_then(|ty| self.concrete_ty_id(ty))
            .is_some_and(|ty| {
                matches!(
                    ty,
                    MonoTy::App {
                        ctor: MonoCtor::Ref,
                        ..
                    }
                )
            })
    }

    pub(crate) fn coerced_to_binding(
        &mut self,
        value: Idx<Inst>,
        value_expr: Idx<Expr>,
        name: Idx<BindingInfo>,
    ) -> Idx<Inst> {
        match self.binding_ty(name) {
            Some(t) => self.coerced(value, value_expr, t),
            None => value,
        }
    }

    pub(crate) fn coerce_numeric_to_float(
        &mut self,
        value: Idx<Inst>,
        from: ArithTy,
        to_float: bool,
    ) -> Idx<Inst> {
        if !to_float {
            return value;
        }
        match from {
            ArithTy::Int => self.emit_cur(Inst::IntToFloat(value)),
            ArithTy::Uint => self.emit_cur(Inst::UintToFloat(value)),
            _ => value,
        }
    }
}
